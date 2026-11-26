//! Windows: the pipe client's process, opened the moment the pipe connected and held for
//! the whole check, its creation time, its image and that image's Authenticode signer.
#![allow(unsafe_code)]

use std::fmt;
use std::mem;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::ptr;

use windows_sys::Win32::Foundation::{FILETIME, HANDLE, INVALID_HANDLE_VALUE, STILL_ACTIVE};
use windows_sys::Win32::Security::Cryptography::{
    CERT_NAME_SIMPLE_DISPLAY_TYPE, CertGetNameStringW,
};
use windows_sys::Win32::Security::WinTrust::{
    WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO,
    WTD_CHOICE_FILE, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY, WTD_UI_NONE,
    WTHelperGetProvSignerFromChain, WTHelperProvDataFromStateData, WinVerifyTrust,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_SHARE_READ, OPEN_EXISTING,
};
use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;
use windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};

use super::Expected;

/// The client of one pipe connection, pinned when it connected.
pub struct Peer {
    pid: Option<u32>,
    pinned: Result<Pinned, String>,
}

/// The client's process, held open, and when it and the connection began.
struct Pinned {
    process: OwnedHandle,
    created: u64,
    connected: u64,
}

/// Now, as a `FILETIME` count: 100 ns since 1601.
pub fn now() -> u64 {
    let mut time = FILETIME::default();
    // SAFETY: `time` is a live FILETIME to write.
    unsafe { GetSystemTimeAsFileTime(&mut time) };
    count(time)
}

fn count(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

impl Peer {
    pub fn of_pipe(pipe: BorrowedHandle<'_>, connected: u64) -> Self {
        let mut pid = 0u32;
        // SAFETY: `pipe` is an open, connected pipe instance; `pid` is a live u32.
        if unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle() as HANDLE, &mut pid) } == 0 {
            let error = std::io::Error::last_os_error();
            return Self {
                pid: None,
                pinned: Err(format!("GetNamedPipeClientProcessId: {error}")),
            };
        }
        Self {
            pid: Some(pid),
            pinned: pin(pid, connected),
        }
    }

    /// The pipe's DACL admits only the user rlxd runs as (`listener`), so every client
    /// is that user's.
    pub fn same_user(&self) -> Result<(), String> {
        Ok(())
    }

    pub fn verify_identity(&self, expected: &Expected) -> Result<String, String> {
        let pinned = self.pinned.as_ref().map_err(Clone::clone)?;
        let pid = self.pid.unwrap_or_default();
        if !created_before(pinned.created, pinned.connected) {
            return Err(format!(
                "pid {pid} was created after the pipe connected: the pid was reused"
            ));
        }
        let handle = pinned.process.as_raw_handle() as HANDLE;
        let mut code = 0u32;
        // SAFETY: `handle` is a process handle we hold open; `code` is a live u32.
        if unsafe { GetExitCodeProcess(handle, &mut code) } == 0 {
            return Err(format!(
                "pid {pid}: GetExitCodeProcess: {}",
                std::io::Error::last_os_error()
            ));
        }
        if code != STILL_ACTIVE as u32 {
            return Err(format!("pid {pid} has exited"));
        }
        let image = image_of(handle).map_err(|e| format!("pid {pid}: {e}"))?;
        let binaries = expected.binaries.as_deref().unwrap_or_default();
        if !binaries.iter().any(|b| same_path(b, &image)) {
            return Err(format!(
                "pid {pid} runs {}, which is not rlx's app",
                image.display()
            ));
        }
        let signer = authenticode_signer(&image).map_err(|e| {
            format!(
                "pid {pid}: {}'s signature does not verify: {e}",
                image.display()
            )
        })?;
        if signer != expected.publisher {
            return Err(format!(
                "pid {pid}: {} is signed by {signer:?}, not rlx's publisher",
                image.display()
            ));
        }
        Ok(format!(
            "GetNamedPipeClientProcessId: pid {pid}, held open since the connection, runs {}, \
             signed by {signer:?}",
            image.display()
        ))
    }
}

impl fmt::Display for Peer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.pid {
            Some(pid) => write!(f, "pid {pid}"),
            None => f.write_str("an unknown pid"),
        }
    }
}

/// Whether a process created at `created` can be the one that connected at
/// `connected`. A later creation means the pid now names another process.
pub(super) fn created_before(created: u64, connected: u64) -> bool {
    created <= connected
}

fn pin(pid: u32, connected: u64) -> Result<Pinned, String> {
    // SAFETY: the call takes any pid, and returns a new handle we then own, or null.
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if raw.is_null() {
        return Err(format!(
            "pid {pid}: OpenProcess: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: `raw` is a handle OpenProcess just gave us; nothing else owns it.
    let process = unsafe { OwnedHandle::from_raw_handle(raw as _) };
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: `process` is open with query rights; the four FILETIMEs are live.
    let read = unsafe {
        GetProcessTimes(
            process.as_raw_handle() as HANDLE,
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    if read == 0 {
        return Err(format!(
            "pid {pid}: GetProcessTimes: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(Pinned {
        process,
        created: count(created),
        connected,
    })
}

/// The full path of the image the process runs, through its handle.
fn image_of(process: HANDLE) -> Result<PathBuf, String> {
    let mut buffer = vec![0u16; 32_768];
    let mut length = buffer.len() as u32;
    // SAFETY: `process` is open with query rights; `buffer` holds `length` u16s.
    let read = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            buffer.as_mut_ptr(),
            &mut length,
        )
    };
    if read == 0 {
        return Err(format!(
            "QueryFullProcessImageNameW: {}",
            std::io::Error::last_os_error()
        ));
    }
    buffer.truncate(length as usize);
    Ok(PathBuf::from(std::ffi::OsString::from_wide(&buffer)))
}

fn same_path(a: &Path, b: &Path) -> bool {
    let canonical = |p: &Path| {
        std::fs::canonicalize(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .to_string_lossy()
            .to_lowercase()
    };
    canonical(a) == canonical(b)
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain([0]).collect()
}

/// The signer of `path`'s Authenticode signature, once `WinVerifyTrust` has verified it
/// to a trusted root. The file is opened sharing reads only, so it can't change while it
/// is checked, and the check reads that open file.
fn authenticode_signer(path: &Path) -> Result<String, String> {
    let name = wide(path);
    // SAFETY: `name` is NUL-terminated; the handle we get is closed by OwnedHandle.
    let raw = unsafe {
        CreateFileW(
            name.as_ptr(),
            FILE_GENERIC_READ,
            FILE_SHARE_READ,
            ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(format!(
            "cannot open it: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: `raw` is a handle CreateFileW just gave us; nothing else owns it.
    let file = unsafe { OwnedHandle::from_raw_handle(raw as _) };
    let mut info = WINTRUST_FILE_INFO {
        cbStruct: mem::size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: name.as_ptr(),
        hFile: file.as_raw_handle() as HANDLE,
        pgKnownSubject: ptr::null_mut(),
    };
    let mut data = WINTRUST_DATA {
        cbStruct: mem::size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 { pFile: &mut info },
        dwStateAction: WTD_STATEACTION_VERIFY,
        ..Default::default()
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    // SAFETY: `data` and `info` are live and filled as WinVerifyTrust expects; the state
    // it opens is closed below, whatever the result.
    let status = unsafe {
        WinVerifyTrust(
            INVALID_HANDLE_VALUE as _,
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast(),
        )
    };
    let signer = if status == 0 {
        // SAFETY: the state is open after a successful verify.
        unsafe { signer_name(data.hWVTStateData) }
    } else {
        Err(format!(
            "WinVerifyTrust: {}",
            std::io::Error::from_raw_os_error(status)
        ))
    };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    // SAFETY: closes the state the verify opened.
    unsafe {
        WinVerifyTrust(
            INVALID_HANDLE_VALUE as _,
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast(),
        )
    };
    drop(file);
    signer
}

/// # Safety
///
/// `state` must be the open state of a successful `WinVerifyTrust`.
unsafe fn signer_name(state: HANDLE) -> Result<String, String> {
    // SAFETY: the caller's promise.
    let provider = unsafe { WTHelperProvDataFromStateData(state) };
    if provider.is_null() {
        return Err("no provider data".to_owned());
    }
    // SAFETY: `provider` is live while the state is open.
    let signer = unsafe { WTHelperGetProvSignerFromChain(provider, 0, 0, 0) };
    // SAFETY: `signer`, when not null, is live while the state is open.
    if signer.is_null() || unsafe { (*signer).csCertChain == 0 || (*signer).pasCertChain.is_null() }
    {
        return Err("no signer certificate".to_owned());
    }
    // SAFETY: the chain holds at least one certificate; the first is the signer's.
    let certificate = unsafe { (*(*signer).pasCertChain).pCert };
    if certificate.is_null() {
        return Err("no signer certificate".to_owned());
    }
    let mut buffer = vec![0u16; 1024];
    // SAFETY: `certificate` is live; `buffer` holds its length in u16s.
    let length = unsafe {
        CertGetNameStringW(
            certificate,
            CERT_NAME_SIMPLE_DISPLAY_TYPE,
            0,
            ptr::null(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
        )
    };
    let end = (length as usize).saturating_sub(1).min(buffer.len());
    Ok(String::from_utf16_lossy(&buffer[..end]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_created_after_the_connection_is_a_reused_pid() {
        assert!(created_before(100, 100));
        assert!(created_before(99, 100));
        assert!(!created_before(101, 100));
    }

    #[test]
    fn a_process_of_this_test_is_pinned_and_its_image_read() {
        let pinned = pin(std::process::id(), now()).unwrap();
        assert!(created_before(pinned.created, pinned.connected));
        let image = image_of(pinned.process.as_raw_handle() as HANDLE).unwrap();
        assert!(same_path(&image, &std::env::current_exe().unwrap()));
    }
}
