//! Giving up root once the daemon has been asked.

use std::ffi::CStr;
use std::io;
use std::mem::MaybeUninit;
use std::ptr;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;

const NOBODY: &CStr = c"nobody";

/// If running as root, become `nobody` for the rest of the process: root
/// cannot be regained, and no other `nobody` process can attach to this
/// one or reach its open files. Does nothing when not root.
pub fn drop_root() -> Result<()> {
    // SAFETY: getuid and geteuid cannot fail and touch no memory.
    if unsafe { libc::getuid() != 0 && libc::geteuid() != 0 } {
        return Ok(());
    }
    let (uid, gid) = nobody()?;

    // SAFETY: plain syscalls on integer arguments; setgroups is given an
    // empty list.
    unsafe {
        check(
            libc::setgroups(0, ptr::null()),
            "clearing supplementary groups",
        )?;
        check(libc::setgid(gid), "setting the group to nobody")?;
        // As root, setuid sets the real, effective and saved IDs alike.
        check(libc::setuid(uid), "setting the user to nobody")?;
    }
    // SAFETY: as above.
    if unsafe { libc::getuid() != uid || libc::geteuid() != uid } {
        bail!("still privileged after switching to nobody");
    }

    // Changing user leaves dumpability to the fs.suid_dumpable sysctl.
    #[cfg(target_os = "linux")]
    {
        // prctl reads its arguments as unsigned longs.
        const OFF: libc::c_ulong = 0;
        const ON: libc::c_ulong = 1;
        // SAFETY: prctl with integer arguments only.
        unsafe {
            check(
                libc::prctl(libc::PR_SET_DUMPABLE, OFF, OFF, OFF, OFF),
                "making the process undumpable",
            )?;
            check(
                libc::prctl(libc::PR_SET_NO_NEW_PRIVS, ON, OFF, OFF, OFF),
                "forbidding new privileges",
            )?;
        }
    }
    Ok(())
}

fn nobody() -> Result<(libc::uid_t, libc::gid_t)> {
    let mut passwd = MaybeUninit::<libc::passwd>::uninit();
    let mut buffer = vec![0; 4096];
    let mut found = ptr::null_mut();
    // SAFETY: every pointer is valid for the duration of the call, and the
    // buffer's length is passed alongside it.
    let error = unsafe {
        libc::getpwnam_r(
            NOBODY.as_ptr(),
            passwd.as_mut_ptr(),
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut found,
        )
    };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error)).context("looking up the nobody user");
    }
    if found.is_null() {
        bail!("there is no nobody user");
    }
    // SAFETY: getpwnam_r filled `passwd` in when it set `found`.
    let passwd = unsafe { passwd.assume_init() };
    Ok((passwd.pw_uid, passwd.pw_gid))
}

fn check(result: libc::c_int, action: &'static str) -> Result<()> {
    if result == -1 {
        return Err(io::Error::last_os_error()).context(action);
    }
    Ok(())
}
