//! Statistics for `NIX_SHOW_STATS`: heap allocations, counted by the global
//! allocator, and the instructions the process retires in user mode.

use std::alloc::GlobalAlloc;
use std::alloc::Layout;
use std::alloc::System;
use std::ffi::CStr;
use std::sync::LazyLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;

/// Whether `NIX_SHOW_STATS` is set and not `0`.
pub static ENABLED: LazyLock<bool> = LazyLock::new(|| {
    // Not std::env: it allocates, and this runs inside the first allocation.
    // SAFETY: the name is NUL-terminated, and nothing sets the environment
    // before the first allocation.
    let v = unsafe { libc::getenv(c"NIX_SHOW_STATS".as_ptr()) };
    // SAFETY: getenv returns null or a NUL-terminated string.
    !v.is_null() && unsafe { CStr::from_ptr(v) } != c"0"
});

/// The system allocator, counting what passes through it if [`ENABLED`].
pub struct Counting;

static LIVE: AtomicU64 = AtomicU64::new(0);
static PEAK: AtomicU64 = AtomicU64::new(0);
static ALLOCATED: AtomicU64 = AtomicU64::new(0);
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

fn allocated(size: usize) {
    let size = size as u64;
    let live = LIVE.fetch_add(size, Relaxed) + size;
    PEAK.fetch_max(live, Relaxed);
    ALLOCATED.fetch_add(size, Relaxed);
    ALLOCATIONS.fetch_add(1, Relaxed);
}

fn count_new(p: *mut u8, size: usize) -> *mut u8 {
    if !p.is_null() && *ENABLED {
        allocated(size);
    }
    p
}

fn freed(size: usize) {
    LIVE.fetch_sub(size as u64, Relaxed);
}

// SAFETY: every call is forwarded to `System` unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `alloc`'s contract.
        count_new(unsafe { System.alloc(layout) }, layout.size())
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `alloc_zeroed`'s contract.
        count_new(unsafe { System.alloc_zeroed(layout) }, layout.size())
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `dealloc`'s contract.
        unsafe { System.dealloc(ptr, layout) };
        if *ENABLED {
            freed(layout.size());
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller upholds `realloc`'s contract.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() && *ENABLED {
            freed(layout.size());
            allocated(new_size);
        }
        p
    }
}

/// Counts the instructions this process and the threads it starts later
/// retire in user mode.
#[cfg(target_os = "linux")]
pub struct Instructions(std::fs::File);

#[cfg(target_os = "linux")]
impl Instructions {
    /// Starts counting, or `None` if the kernel refuses.
    pub fn start() -> Option<Self> {
        use std::os::fd::FromRawFd;

        // The first 64 bytes of `struct perf_event_attr`, its original size.
        #[repr(C)]
        struct Attr {
            kind: u32,
            size: u32,
            config: u64,
            sample_period: u64,
            sample_type: u64,
            read_format: u64,
            flags: u64,
            wakeup_events: u32,
            bp_type: u32,
            config1: u64,
        }
        const PERF_TYPE_HARDWARE: u32 = 0;
        const PERF_COUNT_HW_INSTRUCTIONS: u64 = 1;
        const INHERIT: u64 = 1 << 1;
        const EXCLUDE_KERNEL: u64 = 1 << 5;
        const EXCLUDE_HV: u64 = 1 << 6;
        const PERF_FLAG_FD_CLOEXEC: libc::c_ulong = 1 << 3;

        let attr = Attr {
            kind: PERF_TYPE_HARDWARE,
            size: size_of::<Attr>() as u32,
            config: PERF_COUNT_HW_INSTRUCTIONS,
            sample_period: 0,
            sample_type: 0,
            read_format: 0,
            flags: INHERIT | EXCLUDE_KERNEL | EXCLUDE_HV,
            wakeup_events: 0,
            bp_type: 0,
            config1: 0,
        };
        // SAFETY: `attr` is a valid `perf_event_attr` of the size it gives,
        // for this process on any CPU, in no group.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_perf_event_open,
                &attr,
                0,
                -1,
                -1,
                PERF_FLAG_FD_CLOEXEC,
            )
        };
        if fd < 0 {
            return None;
        }
        // SAFETY: the syscall returned a new file descriptor we own.
        Some(Self(unsafe { std::fs::File::from_raw_fd(fd as i32) }))
    }

    /// The count so far, including threads that have exited.
    pub fn read(&self) -> Option<u64> {
        use std::io::Read;

        let mut buf = [0; 8];
        (&self.0).read_exact(&mut buf).ok()?;
        Some(u64::from_ne_bytes(buf))
    }
}

#[cfg(not(target_os = "linux"))]
pub struct Instructions;

#[cfg(not(target_os = "linux"))]
impl Instructions {
    pub fn start() -> Option<Self> {
        None
    }

    pub fn read(&self) -> Option<u64> {
        None
    }
}

/// The statistics as JSON. `allocatedBytes` and `allocations` count a
/// reallocation as freeing the old block and allocating the new one.
pub fn json(instructions: Option<&Instructions>) -> serde_json::Value {
    let mut stats = serde_json::json!({
        "peakLiveBytes": PEAK.load(Relaxed),
        "allocatedBytes": ALLOCATED.load(Relaxed),
        "allocations": ALLOCATIONS.load(Relaxed),
    });
    if let Some(n) = instructions.and_then(Instructions::read) {
        stats["instructions"] = n.into();
    }
    stats
}
