//! Process groups on macOS and Linux: how a PTY's process ended, and its whole group
//! signalled.
//!
//! portable-pty starts each child with `setsid`, so its pid is its process group's and
//! session's id, and every process it starts stays in that group unless it leaves it
//! itself. rlxd waits for the child here, by pid, rather than through `std`'s `Child`,
//! which reports a signal as exit code 1 and its name: the frame's `status` is 128 plus
//! the signal's number, as a shell gives it.
//!
//! [`wait`] reads how the child ended without reaping it: it stays a zombie until
//! `pty.kill` (or `rlxd stop`) has signalled its group and [`reap`]s it. While it is one,
//! neither its pid nor its group's id can be given to another process, so the group
//! signal can't reach a stranger, however long the PTY's screen is kept.

#![allow(unsafe_code)]

use std::io;

/// How a process ended: its exit status, and the signal that ended it, if one did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ended {
    /// The exit code, or 128 plus the signal.
    pub status: u32,
    /// The signal, when one ended it.
    pub signal: Option<u32>,
}

fn pid_of(pid: u32) -> Option<libc::pid_t> {
    libc::pid_t::try_from(pid).ok().filter(|&pid| pid > 0)
}

/// Waits for the child `pid` to end, and says how, leaving it a zombie for [`reap`].
pub fn wait(pid: u32) -> io::Result<Ended> {
    let Some(pid) = pid_of(pid) else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "no such pid"));
    };
    let id = libc::id_t::try_from(pid).map_err(|_| io::ErrorKind::InvalidInput)?;
    // SAFETY: a zeroed siginfo_t is a valid one to be written.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: `info` is a valid, writable siginfo_t for the call's duration.
        let got = unsafe {
            libc::waitid(
                libc::P_PID,
                id,
                &raw mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if got == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    let status = child_status(&info);
    if info.si_code == libc::CLD_KILLED || info.si_code == libc::CLD_DUMPED {
        let signal = status.unsigned_abs();
        return Ok(Ended {
            status: 128 + signal,
            signal: Some(signal),
        });
    }
    Ok(Ended {
        status: status.unsigned_abs() & 0xff,
        signal: None,
    })
}

/// A `waitid` record's status: the exit code, or the signal.
#[cfg(target_os = "macos")]
fn child_status(info: &libc::siginfo_t) -> libc::c_int {
    // SAFETY: waitid filled it in for a child that ended.
    unsafe { info.si_status() }
}

/// A `waitid` record's status: the exit code, or the signal. The libc crate has no
/// accessor for it on Linux; this is the kernel's layout of a SIGCHLD's record, which
/// its ABI keeps: the three ints, the union aligned to a pointer, then pid, uid, status.
#[cfg(not(target_os = "macos"))]
fn child_status(info: &libc::siginfo_t) -> libc::c_int {
    #[repr(C)]
    struct Chld {
        signo: libc::c_int,
        errno: libc::c_int,
        code: libc::c_int,
        #[cfg(target_pointer_width = "64")]
        pad: libc::c_int,
        pid: libc::pid_t,
        uid: libc::uid_t,
        status: libc::c_int,
    }
    const _: () = assert!(size_of::<Chld>() <= size_of::<libc::siginfo_t>());
    // SAFETY: siginfo_t is larger than Chld, and Chld is its SIGCHLD layout.
    unsafe { (*std::ptr::from_ref(info).cast::<Chld>()).status }
}

/// Reaps the child `pid`, a zombie by now or soon: up to `within`. Whether it was.
pub fn reap(pid: u32, within: std::time::Duration) -> bool {
    let Some(pid) = pid_of(pid) else {
        return false;
    };
    let deadline = std::time::Instant::now() + within;
    loop {
        let mut status: libc::c_int = 0;
        // SAFETY: `status` is a valid, writable int for the call's duration.
        let got = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) };
        if got == pid {
            return true;
        }
        if got < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return false;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Sends `signal` to every process in the group `pgid`. Whether any got it.
pub fn signal_group(pgid: u32, signal: libc::c_int) -> bool {
    let Some(pgid) = pid_of(pgid) else {
        return false;
    };
    // SAFETY: killpg takes plain integers and touches no memory of ours.
    unsafe { libc::killpg(pgid, signal) == 0 }
}

/// Whether any process is still in the group `pgid`.
pub fn group_alive(pgid: u32) -> bool {
    let Some(pgid) = pid_of(pgid) else {
        return false;
    };
    // SAFETY: signal 0 checks for the group only; no memory is touched.
    if unsafe { libc::killpg(pgid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// SIGHUP, as a terminal hanging up sends.
pub const SIGHUP: libc::c_int = libc::SIGHUP;
/// SIGTERM.
pub const SIGTERM: libc::c_int = libc::SIGTERM;
/// SIGKILL.
pub const SIGKILL: libc::c_int = libc::SIGKILL;
