//! A cgroup v2 for each Nix run under [`Budget::Fuzzing`](crate::Budget),
//! limiting its memory and recording whether it ran out.

use std::fs::File;
use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// The cgroup the fuzzer's processes run in, beside those of the Nix runs.
const LEAF: &str = "fuzzer";

/// The start of a Nix run's cgroup's name, followed by the ID of the process
/// that made it and a count.
const RUN: &str = "nix-";

/// Set up this process to limit the memory of each Nix run under
/// [`Budget::Fuzzing`](crate::Budget), moving it into a cgroup that the
/// processes it starts inherit. It must be the only process in its cgroup
/// v2, which must be delegated to it with the memory controller available,
/// as under `systemd-run --user --scope`; one started by such a process is
/// set up already. Fails, saying why, where that is not so. A systemd unit
/// running it needs `OOMPolicy=continue`, or it is stopped the first time
/// Nix runs out of memory.
///
/// Must be called before this process starts any other that may run Nix
/// under that budget; that budget otherwise calls it, and panics if it
/// fails.
pub fn limit_nix_memory() -> Result<(), String> {
    parent().as_ref().map(drop).map_err(Clone::clone)
}

/// The cgroup holding [`LEAF`] and each Nix run's cgroup.
fn parent() -> &'static Result<PathBuf, String> {
    static PARENT: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    PARENT.get_or_init(set_up)
}

fn set_up() -> Result<PathBuf, String> {
    let read = |path: &Path| {
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))
    };
    let mounts = read(Path::new("/proc/self/mounts"))?;
    let root = mounts
        .lines()
        .find_map(|line| {
            let mut fields = line.split(' ');
            let dir = fields.nth(1)?;
            (fields.next()? == "cgroup2").then_some(dir)
        })
        .ok_or("no cgroup v2 hierarchy is mounted")?;
    let cgroup = read(Path::new("/proc/self/cgroup"))?;
    let own = cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or("this process is in no cgroup v2")?;
    let own = Path::new(root).join(own.trim_start_matches('/'));
    let parent = match own.parent() {
        Some(parent)
            if own.ends_with(LEAF) && memory(&read(&parent.join("cgroup.subtree_control"))?) =>
        {
            parent.to_owned()
        }
        _ => {
            take(&own, read)?;
            own
        }
    };
    sweep(&parent).map_err(|e| format!("removing stale cgroups in {}: {e}", parent.display()))?;
    Ok(parent)
}

/// Whether `controllers`, a cgroup's list of them, has the memory controller.
fn memory(controllers: &str) -> bool {
    controllers.split_whitespace().any(|c| c == "memory")
}

/// Move this process from `own`, its cgroup, into [`LEAF`] within it, and
/// enable the memory controller for `own`'s children.
fn take(own: &Path, read: impl Fn(&Path) -> Result<String, String>) -> Result<(), String> {
    let own_display = own.display();
    if !memory(&read(&own.join("cgroup.controllers"))?) {
        return Err(format!(
            "the memory controller is not available in {own_display}"
        ));
    }
    let me = std::process::id().to_string();
    if read(&own.join("cgroup.procs"))?
        .lines()
        .any(|pid| pid != me)
    {
        return Err(format!(
            "{own_display} holds processes other than this one; start the fuzzer in a \
             cgroup of its own, as with `systemd-run --user --scope -p OOMPolicy=continue`"
        ));
    }
    let leaf = own.join(LEAF);
    match std::fs::create_dir(&leaf) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
        Err(e) => {
            return Err(format!(
                "creating {}: {e}; the fuzzer needs its cgroup delegated to it",
                leaf.display()
            ));
        }
    }
    let write = |file: &str, value: &str| {
        let path = own.join(file);
        std::fs::write(&path, value)
            .map_err(|e| format!("writing {value} to {}: {e}", path.display()))
    };
    write(&format!("{LEAF}/cgroup.procs"), "0")?;
    write("cgroup.subtree_control", "+memory")
}

/// Remove each Nix run's cgroup in `parent` made by a process that has since
/// exited, or by an earlier process with this one's ID.
fn sweep(parent: &Path) -> std::io::Result<()> {
    let me = std::process::id();
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        let pid = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_prefix(RUN)?.split_once('-')?.0.parse().ok());
        if let Some(pid) = pid
            && (pid == me || !alive(pid))
        {
            remove(&entry.path())?;
        }
    }
    Ok(())
}

fn alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks that the process exists.
    let signalled = unsafe { libc::kill(pid, 0) } == 0;
    signalled || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Kill every process in the cgroup `dir`, then remove it.
fn remove(dir: &Path) -> std::io::Result<()> {
    match std::fs::write(dir.join("cgroup.kill"), "1") {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        written => written?,
    }
    loop {
        match std::fs::remove_dir(dir) {
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
                // The killed processes have not all exited yet.
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
            removed => return removed,
        }
    }
}

/// Whether `events`, a cgroup's `memory.events`, records it running out of
/// memory.
fn oom(events: &str) -> bool {
    events
        .lines()
        .any(|line| matches!(line.split_once(' '), Some(("oom" | "oom_kill", n)) if n != "0"))
}

/// The cgroup of one Nix run, which kills any process left in it and is
/// removed on drop.
pub(crate) struct Cgroup {
    dir: PathBuf,
    procs: File,
}

impl Cgroup {
    /// A new cgroup, whose processes may use `max` bytes of memory and no
    /// swap. Panics if this process cannot be [set up](limit_nix_memory).
    pub(crate) fn new(max: u64) -> Cgroup {
        static RUNS: AtomicU64 = AtomicU64::new(0);
        let parent = match parent() {
            Ok(parent) => parent,
            Err(e) => panic!("limiting Nix's memory: {e}"),
        };
        let dir = parent.join(format!(
            "{RUN}{}-{}",
            std::process::id(),
            RUNS.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).expect("creating Nix's cgroup");
        let write = |file: &str, value: &str| {
            std::fs::write(dir.join(file), value).expect("limiting Nix's memory");
        };
        write("memory.max", &max.to_string());
        write("memory.swap.max", "0");
        let procs = File::options()
            .write(true)
            .open(dir.join("cgroup.procs"))
            .expect("opening Nix's cgroup");
        Cgroup { dir, procs }
    }

    /// Have `cmd` start its process in this cgroup.
    pub(crate) fn place(&self, cmd: &mut Command) {
        let procs = self.procs.as_raw_fd();
        // SAFETY: write is async-signal-safe, and `procs` stays open until
        // this is dropped, after `cmd` has started its process.
        unsafe {
            cmd.pre_exec(move || {
                if libc::write(procs, b"0".as_ptr().cast(), 1) == 1 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
    }

    /// Whether a process in it ran out of memory: an allocation reached
    /// its limit and could not be met, or the kernel's OOM killer, for any
    /// limit, killed one.
    pub(crate) fn exhausted(&self) -> bool {
        oom(&std::fs::read_to_string(self.dir.join("memory.events"))
            .expect("reading Nix's memory events"))
    }
}

impl Drop for Cgroup {
    fn drop(&mut self) {
        remove(&self.dir).expect("removing Nix's cgroup");
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;

    use super::*;

    #[test]
    fn running_out_is_an_oom_or_a_kill() {
        let events = |max, oom, kill| {
            format!(
                "low 0\nhigh 0\nmax {max}\noom {oom}\noom_kill {kill}\n\
                 oom_group_kill 0\nsock_throttled 0\n"
            )
        };
        // Reclaim kept it within its limit.
        assert!(!oom(&events(19, 0, 0)));
        assert!(oom(&events(90, 1, 1)));
        // An allocation failed rather than killing anything.
        assert!(oom(&events(90, 1, 0)));
        // Killed for another cgroup's limit, or the system's.
        assert!(oom(&events(0, 0, 1)));
    }

    #[test]
    #[ignore = "needs a cgroup of its own: run the test binary under \
                `systemd-run --user --scope -p OOMPolicy=continue`"]
    fn limits_a_process_and_cleans_up() {
        let run = |program: &str, args: &[&str]| {
            let cgroup = Cgroup::new(4 << 20);
            let mut cmd = Command::new(program);
            cmd.args(args);
            cgroup.place(&mut cmd);
            let mut child = cmd.spawn().expect("starting the process");
            child.wait().expect("waiting for the process");
            (cgroup.exhausted(), cgroup.dir.clone())
        };
        let (exhausted, dir) = run("true", &[]);
        assert!(!exhausted);
        assert!(!dir.exists());
        // tail holds the line it is reading, which never ends.
        let (exhausted, dir) = run("tail", &["/dev/zero"]);
        assert!(exhausted);
        assert!(!dir.exists());
        // Left running, it is killed with its cgroup.
        let cgroup = Cgroup::new(4 << 20);
        let mut cmd = Command::new("sleep");
        cmd.arg("1000");
        cgroup.place(&mut cmd);
        let mut child = cmd.spawn().expect("starting the process");
        let dir = cgroup.dir.clone();
        drop(cgroup);
        assert!(!dir.exists());
        let status = child.wait().expect("waiting for the process");
        assert_eq!(status.signal(), Some(libc::SIGKILL));
    }
}
