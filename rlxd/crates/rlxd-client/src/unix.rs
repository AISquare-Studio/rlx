//! macOS and Linux: who owns the short socket's folder and the socket in it.
//!
//! A home whose socket path is too long for the OS has its socket in
//! [`Home::short_socket_dir`](crate::Home::short_socket_dir), `/tmp/rlxd-<uid>/`. `/tmp`
//! is shared, so another user can make that folder first. rlxd only listens there in a
//! folder this user owns that no one else may enter. A client sends the token only to a
//! socket in such a folder, made by this user.
#![allow(unsafe_code)]

use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;

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
