//! Linux: the peer's uid from `SO_PEERCRED`, the process pinned with `SO_PEERPIDFD`
//! (or `pidfd_open` on an older kernel), and its binary from `/proc/<pid>/exe` where
//! rlx's app is installed from a package.
#![allow(unsafe_code)]

use std::fmt;
use std::fs;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

use super::{Expected, LINUX_APP_NAME};

/// `SO_PEERPIDFD`, Linux 6.5 and later: a pidfd for the process that connected.
const SO_PEERPIDFD: libc::c_int = 77;

/// How the process is pinned.
enum Pin {
    /// `SO_PEERPIDFD`: the kernel's own handle on the process that connected.
    PeerPidfd(OwnedFd),
    /// `pidfd_open` on `SO_PEERCRED`'s pid at accept, on a kernel without `SO_PEERPIDFD`:
    /// weaker, as the pid could have been reused before the accept.
    PidfdOpen(OwnedFd),
    /// Neither could be had.
    None(String),
}

/// The peer of one Unix socket connection.
pub struct Peer {
    credentials: Result<libc::ucred, String>,
    pin: Pin,
}

impl Peer {
    pub fn of_socket(fd: BorrowedFd<'_>) -> Self {
        let fd = fd.as_raw_fd();
        let credentials = peer_credentials(fd).map_err(|e| format!("SO_PEERCRED: {e}"));
        let pin = match peer_pidfd(fd) {
            Ok(pidfd) => Pin::PeerPidfd(pidfd),
            Err(error) => match &credentials {
                Ok(credentials) => match pidfd_open(credentials.pid) {
                    Ok(pidfd) => Pin::PidfdOpen(pidfd),
                    Err(open) => Pin::None(format!(
                        "no SO_PEERPIDFD ({error}), and pidfd_open failed ({open})"
                    )),
                },
                Err(_) => Pin::None(format!("no SO_PEERPIDFD ({error}) and no SO_PEERCRED")),
            },
        };
        Self { credentials, pin }
    }

    pub fn pid(&self) -> Option<u32> {
        let pid = match &self.pin {
            Pin::PeerPidfd(pidfd) | Pin::PidfdOpen(pidfd) => pid_of(pidfd.as_raw_fd()),
            Pin::None(_) => None,
        };
        pid.or_else(|| self.credentials.as_ref().ok().map(|c| c.pid))
            .and_then(|pid| u32::try_from(pid).ok())
    }

    /// [`super::Peer::while_pinned`]: through the pidfd, never a bare pid.
    pub fn while_pinned(&self, check: impl FnOnce(u32) -> bool) -> bool {
        let pidfd = match &self.pin {
            Pin::PeerPidfd(pidfd) | Pin::PidfdOpen(pidfd) => pidfd.as_raw_fd(),
            Pin::None(_) => return false,
        };
        let Some(pid) = pid_of(pidfd).and_then(|pid| u32::try_from(pid).ok()) else {
            return false;
        };
        alive(pidfd).is_ok() && check(pid) && alive(pidfd).is_ok()
    }

    pub fn image(&self) -> Option<String> {
        let exe = fs::read_link(format!("/proc/{}/exe", self.pid()?)).ok()?;
        Some(exe.display().to_string())
    }

    pub fn same_user(&self) -> Result<(), String> {
        let credentials = self.credentials.as_ref().map_err(Clone::clone)?;
        // SAFETY: geteuid cannot fail.
        let mine = unsafe { libc::geteuid() };
        if credentials.uid == mine {
            Ok(())
        } else {
            Err(format!(
                "SO_PEERCRED: the peer runs as uid {}, not rlxd's {mine}",
                credentials.uid
            ))
        }
    }

    pub fn verify_identity(&self, expected: &Expected) -> Result<String, String> {
        self.same_user()?;
        let (pidfd, how) = match &self.pin {
            Pin::PeerPidfd(pidfd) => (pidfd, "SO_PEERPIDFD"),
            Pin::PidfdOpen(pidfd) => (pidfd, "pidfd_open (no SO_PEERPIDFD: a weaker pin)"),
            Pin::None(why) => return Err(why.clone()),
        };
        let pid = pid_of(pidfd.as_raw_fd()).ok_or_else(|| format!("{how}: the peer has exited"))?;
        let exe = fs::read_link(format!("/proc/{pid}/exe"))
            .map_err(|e| format!("{how}: pid {pid}'s binary is unreadable: {e}"))?;
        // The binary was read through the pid; the pidfd says that pid is still the
        // process that connected.
        alive(pidfd.as_raw_fd())
            .map_err(|e| format!("{how}: pid {pid} is no longer the peer that connected: {e}"))?;
        let binary = match &expected.binaries {
            None if exe.file_name().is_some_and(|name| name == LINUX_APP_NAME) => {
                return Ok(format!(
                    "{how}: pid {pid} runs {}; no packaged rlx app on this host, so only its \
                     uid and its binary's name are checked (Linux's weaker guarantee)",
                    exe.display()
                ));
            }
            None => None,
            Some(binaries) => binaries.iter().find(|b| same_file(b, &exe)),
        };
        match binary {
            Some(binary) => Ok(format!(
                "{how}: pid {pid} runs {}, rlx's app",
                binary.display()
            )),
            None => Err(format!(
                "{how}: pid {pid} runs {}, which is not rlx's app",
                exe.display()
            )),
        }
    }
}

impl fmt::Display for Peer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.credentials {
            Ok(credentials) => write!(f, "pid {}", credentials.pid),
            Err(_) => f.write_str("an unknown pid"),
        }
    }
}

fn same_file(expected: &Path, exe: &Path) -> bool {
    match fs::canonicalize(expected) {
        Ok(expected) => expected == exe,
        Err(_) => expected == exe,
    }
}

fn peer_credentials(fd: RawFd) -> io::Result<libc::ucred> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `fd` is an open socket, and `credentials` is as large as `length` says.
    let read = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if read == 0 {
        Ok(credentials)
    } else {
        Err(io::Error::last_os_error())
    }
}

fn peer_pidfd(fd: RawFd) -> io::Result<OwnedFd> {
    let mut pidfd: libc::c_int = -1;
    let mut length = mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `fd` is an open socket, and `pidfd` is as large as `length` says.
    let read = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            SO_PEERPIDFD,
            (&mut pidfd as *mut libc::c_int).cast(),
            &mut length,
        )
    };
    if read != 0 {
        return Err(io::Error::last_os_error());
    }
    if pidfd < 0 {
        return Err(io::Error::other("the kernel gave no pidfd"));
    }
    // SAFETY: the kernel just made this descriptor for us; nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(pidfd) })
}

fn pidfd_open(pid: libc::pid_t) -> io::Result<OwnedFd> {
    // SAFETY: the syscall takes a pid and flags, and returns a new descriptor or -1.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the kernel just made this descriptor for us; nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// Signal 0 through the pidfd: succeeds only while that very process lives.
fn alive(pidfd: RawFd) -> io::Result<()> {
    // SAFETY: signal 0 to a pidfd we own, with no siginfo and no flags.
    let sent = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd,
            0,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if sent == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// The pid a pidfd refers to, from `/proc/self/fdinfo`, or `None` once it has exited and
/// been reaped (the kernel then says `-1`).
fn pid_of(pidfd: RawFd) -> Option<libc::pid_t> {
    let info = fs::read_to_string(format!("/proc/self/fdinfo/{pidfd}")).ok()?;
    let pid = info
        .lines()
        .find_map(|line| line.strip_prefix("Pid:"))?
        .trim()
        .parse::<libc::pid_t>()
        .ok()?;
    (pid > 0).then_some(pid)
}
