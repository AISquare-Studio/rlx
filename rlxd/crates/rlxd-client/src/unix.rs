//! macOS and Linux: who owns the short socket's folder and the socket in it.
//!
//! A home whose socket path is too long for the OS has its socket in
//! [`Home::short_socket_dir`](crate::Home::short_socket_dir), `/tmp/rlxd-<uid>/`. `/tmp`
//! is shared, so another user can make that folder first. rlxd only listens there in a
//! folder this user owns that no one else may enter. A client sends the token only to a
//! socket in such a folder, made by this user. Both use the folder by path after they
//! check it, which is safe only while `/tmp` is root's and sticky ([`is_sticky_and_roots`]):
//! then no other user can rename or replace an entry of ours in it in between.
#![allow(unsafe_code)]

use std::fs::{self, Metadata};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// The user this process runs as (its real uid).
pub fn uid() -> u32 {
    // SAFETY: getuid takes nothing, touches no memory of ours and cannot fail.
    unsafe { libc::getuid() }
}

/// Whether `dir`, read without following a link (`fs::symlink_metadata`), is a folder
/// that this user owns and no one else may enter: mode 0700 or narrower.
pub fn is_private_dir(dir: &Metadata) -> bool {
    dir.is_dir() && dir.uid() == uid() && dir.mode() & 0o077 == 0
}

/// Whether this user owns `file`, read without following a link.
pub fn is_owned(file: &Metadata) -> bool {
    file.uid() == uid()
}

/// Whether `dir` is a folder that root owns with the sticky bit set, as `/tmp` must be:
/// then only an entry's owner (or root) may rename or remove it.
pub fn is_sticky_and_roots(dir: &Metadata) -> bool {
    dir.is_dir() && dir.uid() == 0 && dir.mode() & 0o1000 != 0
}

/// Checks that `dir`, followed through a link (macOS's `/tmp` is one to `/private/tmp`),
/// is root's and sticky ([`is_sticky_and_roots`]): an error saying so otherwise.
pub fn check_sticky(dir: &Path) -> io::Result<()> {
    let found = fs::metadata(dir)?;
    if is_sticky_and_roots(&found) {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "{} is not root's with the sticky bit set (mode {:o}, uid {}), so another user \
         could replace a folder of this user's in it between a check and its use",
        dir.display(),
        found.mode() & 0o7777,
        found.uid()
    )))
}

/// The uid of the process at the other end of the connected Unix socket `socket`: the
/// one that listens, for a client. Read from the connection, not the path, so it is who
/// was actually reached. `getpeereid` on macOS and the BSDs, `SO_PEERCRED` on Linux.
pub fn peer_uid(socket: std::os::fd::BorrowedFd<'_>) -> io::Result<u32> {
    use std::os::fd::AsRawFd;
    let fd = socket.as_raw_fd();
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut cred = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: `cred` and `len` are ours and the size `len` says; the call fills them.
        let done = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut len,
            )
        };
        if done != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(cred.uid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: `uid` and `gid` are ours; the call fills them.
        if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(uid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_peer_of_ours_is_this_user() {
        use std::os::fd::AsFd;
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        assert_eq!(peer_uid(a.as_fd()).unwrap(), uid());
    }

    #[test]
    fn tmp_is_roots_and_sticky_and_a_folder_of_ours_is_not() {
        check_sticky(Path::new("/tmp")).unwrap();
        let ours = std::env::temp_dir().join(format!("rlxd-sticky-{}", std::process::id()));
        let _ = fs::remove_dir_all(&ours);
        fs::create_dir(&ours).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&ours, fs::Permissions::from_mode(0o1777)).unwrap();
        let refused = check_sticky(&ours).unwrap_err().to_string();
        assert!(refused.contains("not root's"), "{refused}");
        fs::remove_dir(&ours).unwrap();
    }
}
