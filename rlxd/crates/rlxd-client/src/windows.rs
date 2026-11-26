//! Windows: who a process runs as, so rlxd can admit only its own user to its pipe and a
//! client can tell that the pipe it opened is served by that user.
//!
//! This is raw FFI to the Win32 security API, the one place in this crate with `unsafe`.
#![allow(unsafe_code)]

use std::io;
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{
    CopySid, EqualSid, GetLengthSid, GetTokenInformation, IsValidSid, PSID, TOKEN_QUERY,
    TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// A security identifier: the user a process runs as.
pub struct Sid {
    /// The SID's bytes, in `u32`s so that they are aligned as a `SID` must be.
    words: Vec<u32>,
}

impl Sid {
    /// The user this process runs as.
    pub fn current_user() -> io::Result<Self> {
        // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing.
        let process = unsafe { GetCurrentProcess() };
        Self::of_process_handle(process)
    }

    /// The user the process `pid` runs as.
    pub fn of_process(pid: u32) -> io::Result<Self> {
        // SAFETY: a plain call; a null handle is checked below.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        let process = Owned(process);
        Self::of_process_handle(process.0)
    }

    fn of_process_handle(process: HANDLE) -> io::Result<Self> {
        let mut token: HANDLE = ptr::null_mut();
        // SAFETY: `process` is a valid process handle and `token` a place for the result.
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = Owned(token);
        let mut len = 0u32;
        // SAFETY: asking for the size only, with no buffer; this call fails by design.
        unsafe { GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut len) };
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        // u64s, so the TOKEN_USER at the buffer's start is aligned for its pointer.
        let mut buffer = vec![0u64; (len as usize).div_ceil(8)];
        // SAFETY: `buffer` holds at least `len` bytes, as the call asked for.
        let filled = unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                len,
                &mut len,
            )
        };
        if filled == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the call filled the buffer's start with a TOKEN_USER, whose SID points
        // into the same buffer, which lives until the copy below is made.
        let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        // SAFETY: `sid` is the token's user SID, valid while `buffer` lives.
        unsafe { Self::copy(sid) }
    }

    /// A copy of the SID at `sid`.
    ///
    /// # Safety
    ///
    /// `sid` must point to a valid SID for the duration of the call.
    unsafe fn copy(sid: PSID) -> io::Result<Self> {
        // SAFETY: the caller's promise.
        if unsafe { IsValidSid(sid) } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a valid SID",
            ));
        }
        // SAFETY: as above.
        let len = unsafe { GetLengthSid(sid) };
        let mut words = vec![0u32; (len as usize).div_ceil(4)];
        // SAFETY: `words` holds at least `len` bytes.
        if unsafe { CopySid(len, words.as_mut_ptr().cast(), sid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { words })
    }

    /// The SID, for a Win32 call that reads it while `self` lives.
    pub fn as_psid(&self) -> PSID {
        self.words.as_ptr().cast_mut().cast()
    }

    /// Whether the SID at `other` is this one.
    ///
    /// # Safety
    ///
    /// `other` must point to a valid SID for the duration of the call.
    pub unsafe fn equals(&self, other: PSID) -> bool {
        // SAFETY: both point to valid SIDs: ours while `self` lives, `other` by the
        // caller's promise.
        unsafe { IsValidSid(other) != 0 && EqualSid(self.as_psid(), other) != 0 }
    }

    /// The SID in its string form, such as `S-1-5-21-…-1001`, as SDDL writes it.
    pub fn to_string_form(&self) -> io::Result<String> {
        let mut text: *mut u16 = ptr::null_mut();
        // SAFETY: our SID is valid, and `text` receives a LocalAlloc'ed string.
        if unsafe { ConvertSidToStringSidW(self.as_psid(), &mut text) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the call returned a NUL-terminated wide string at `text`.
        let len = (0..).take_while(|&i| unsafe { *text.add(i) } != 0).count();
        // SAFETY: `len` wide chars precede the NUL.
        let wide = unsafe { std::slice::from_raw_parts(text, len) };
        let string = String::from_utf16_lossy(wide);
        // SAFETY: `text` was allocated by the call with LocalAlloc.
        unsafe { LocalFree(text as HLOCAL) };
        Ok(string)
    }
}

impl PartialEq for Sid {
    fn eq(&self, other: &Self) -> bool {
        // SAFETY: both SIDs are valid while borrowed.
        unsafe { self.equals(other.as_psid()) }
    }
}

impl Eq for Sid {}

impl std::fmt::Debug for Sid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.to_string_form() {
            Ok(text) => f.write_str(&text),
            Err(_) => f.write_str("<a SID>"),
        }
    }
}

/// The user the process serving the pipe client `pipe` runs as.
pub fn pipe_server_user(pipe: &impl AsRawHandle) -> io::Result<Sid> {
    let mut pid = 0u32;
    let handle: RawHandle = pipe.as_raw_handle();
    // SAFETY: `handle` is the client end of a pipe, open while `pipe` is borrowed.
    if unsafe { GetNamedPipeServerProcessId(handle as HANDLE, &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Sid::of_process(pid)
}

/// A handle closed when dropped.
struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the handle is ours and open.
        unsafe { CloseHandle(self.0) };
    }
}
