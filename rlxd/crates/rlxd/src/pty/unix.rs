//! Process groups on macOS and Linux: how a PTY's process ended, and its whole group
//! signalled.
//!
//! portable-pty starts each child with `setsid`, so its pid is its process group's and
//! session's id, and every process it starts stays in that group unless it leaves it
//! itself. rlxd reaps the child here, by pid, rather than through `std`'s `Child`, which
//! reports a signal as exit code 1 and its name: the frame's `status` is 128 plus the
//! signal's number, as a shell gives it.

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

/// Waits for the child `pid` to end, and reaps it.
pub fn wait(pid: u32) -> io::Result<Ended> {
    let Some(pid) = pid_of(pid) else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "no such pid"));
    };
    let mut status: libc::c_int = 0;
    loop {
        // SAFETY: `status` is a valid, writable int for the call's duration.
        let got = unsafe { libc::waitpid(pid, &raw mut status, 0) };
        if got == pid {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    if libc::WIFSIGNALED(status) {
        let signal = libc::WTERMSIG(status).unsigned_abs();
        return Ok(Ended {
            status: 128 + signal,
            signal: Some(signal),
        });
    }
    Ok(Ended {
        status: libc::WEXITSTATUS(status).unsigned_abs(),
        signal: None,
    })
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
