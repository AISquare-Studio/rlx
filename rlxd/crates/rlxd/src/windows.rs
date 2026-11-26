//! Windows: what a process that starts a detached rlxd must do first, and a rename that
//! is on disk when it returns.
#![allow(unsafe_code)]

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::{HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation};
use windows_sys::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};

/// Stops this process's stdin, stdout and stderr from being inherited by the processes it
/// starts. `CreateProcess` passes every inheritable handle on, whatever the child's own
/// stdio is, so a detached rlxd would otherwise hold the pipe a caller reads this
/// process's output from, and `rlxd serve --detach | jq` would wait for rlxd to exit.
pub fn keep_std_handles_from_children() {
    let handles = [
        io::stdin().as_raw_handle(),
        io::stdout().as_raw_handle(),
        io::stderr().as_raw_handle(),
    ];
    for handle in handles {
        if !handle.is_null() {
            // SAFETY: the handle is this process's own std handle; a stale or invalid one
            // makes the call fail, which changes nothing.
            unsafe { SetHandleInformation(handle as HANDLE, HANDLE_FLAG_INHERIT, 0) };
        }
    }
}

/// Renames `from` over `to`, and returns once the rename is on disk
/// (`MOVEFILE_WRITE_THROUGH`), as POSIX's rename and an fsync of the folder would.
pub fn replace_durably(from: &Path, to: &Path) -> io::Result<()> {
    let wide = |path: &Path| -> Vec<u16> { path.as_os_str().encode_wide().chain([0]).collect() };
    let (from, to) = (wide(from), wide(to));
    // SAFETY: both are NUL-terminated UTF-16 paths that outlive the call.
    let moved = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
