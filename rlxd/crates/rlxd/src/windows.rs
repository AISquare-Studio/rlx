//! Windows: what a process that starts a detached rlxd must do first.
#![allow(unsafe_code)]

use std::io;
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Foundation::{HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation};

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
