//! Windows: the pipe client's process, opened the moment the pipe connected and held for
//! the whole check, its creation time, its image, and the Authenticode signer of the very
//! file that image was mapped from.
//!
//! **The file checked is the one the process runs.** The image's path is the one the
//! process was started from, which Windows keeps, so a same-user process could run an
//! unsigned file from the expected path, rename it away (a running image can be renamed)
//! and put the signed file there before the check. So rlxd:
//!
//! 1. opens the file at that path sharing reads only, so nothing renames, replaces or
//!    deletes it while rlxd holds it;
//! 2. reads, through the held process handle, the name of the file its main image is
//!    mapped from now (`K32GetMappedFileNameW` on the image base), opens that too, sharing
//!    reads only, and requires both to be one file (volume serial and file index);
//! 3. reads the mapped name again and requires it unchanged, so the image didn't move
//!    while it was opened;
//! 4. verifies the Authenticode signature of that held file (`WinVerifyTrust` with its
//!    handle), and requires the signer's full subject and issuer to be rlx's publisher's.
//!
//! **Revocation** is checked across the chain, the root excepted, from what Windows has
//! cached only, so a check never waits on the network. It hard-fails only on a revoked
//! certificate. When the status can't be known (offline, nothing cached), it is allowed
//! and logged: a soft fail.
//!
//! **What this can't see,** on top of the same-user limits in `peer`'s doc:
//! - **Process herpaderping:** a file rewritten after its image section was made, so the
//!   process runs code that is no longer in the file checked here;
//! - **DLL planting:** a DLL put beside a per-user install, which the signed app then
//!   loads into itself.
//!
//! **An elevated app is refused:** rlxd runs as the user, so it can't open an elevated
//! process for the reads this check needs, and the check fails closed.
#![allow(unsafe_code)]

use std::fmt;
use std::mem;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::ptr;

use windows_sys::Win32::Foundation::{
    FILETIME, HANDLE, HMODULE, INVALID_HANDLE_VALUE, STILL_ACTIVE,
};
use windows_sys::Win32::Security::Cryptography::{
    CERT_X500_NAME_STR, CertNameToStrW, X509_ASN_ENCODING,
};
use windows_sys::Win32::Security::WinTrust::{
    WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO,
    WTD_CACHE_ONLY_URL_RETRIEVAL, WTD_CHOICE_FILE, WTD_REVOCATION_CHECK_CHAIN_EXCLUDE_ROOT,
    WTD_REVOKE_NONE, WTD_REVOKE_WHOLECHAIN, WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY,
    WTD_UI_NONE, WTHelperGetProvSignerFromChain, WTHelperProvDataFromStateData, WinVerifyTrust,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_ID_INFO, FILE_READ_ATTRIBUTES,
    FILE_SHARE_READ, FileIdInfo, GetFileInformationByHandleEx, OPEN_EXISTING,
};
use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;
use windows_sys::Win32::System::ProcessStatus::{K32EnumProcessModules, K32GetMappedFileNameW};
use windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32,
    PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    QueryFullProcessImageNameW,
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

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub fn image(&self) -> Option<String> {
        let pinned = self.pinned.as_ref().ok()?;
        let image = image_of(pinned.process.as_raw_handle() as HANDLE).ok()?;
        Some(image.display().to_string())
    }

    pub fn verify_identity(&self, expected: &Expected) -> Result<String, String> {
        let pinned = self.pinned.as_ref().map_err(Clone::clone)?;
        let pid = self.pid.unwrap_or_default();
        if !created_before(pinned.created, pinned.connected) {
            return Err(format!(
                "pid {pid} was created after the pipe connected: the pid was reused"
            ));
        }
        let process = pinned.process.as_raw_handle() as HANDLE;
        let mut code = 0u32;
        // SAFETY: `process` is a process handle we hold open; `code` is a live u32.
        if unsafe { GetExitCodeProcess(process, &mut code) } == 0 {
            return Err(format!(
                "pid {pid}: GetExitCodeProcess: {}",
                std::io::Error::last_os_error()
            ));
        }
        if code != STILL_ACTIVE as u32 {
            return Err(format!("pid {pid} has exited"));
        }
        let Some((subject, issuer)) = expected.publisher.as_ref() else {
            return Err(
                "no Windows publisher is pinned for rlx's app yet, so no peer passes".to_owned(),
            );
        };
        let image = image_of(process).map_err(|e| format!("pid {pid}: {e}"))?;
        let binaries = expected.binaries.as_deref().unwrap_or_default();
        if !binaries.iter().any(|b| same_path(b, &image)) {
            return Err(format!(
                "pid {pid} runs {}, which is not rlx's app",
                image.display()
            ));
        }
        let file = held_image(process, &image).map_err(|e| format!("pid {pid}: {e}"))?;
        let signer = authenticode_signer(&file, &image).map_err(|e| {
            format!(
                "pid {pid}: {}'s signature does not verify: {e}",
                image.display()
            )
        })?;
        if signer.subject != *subject || signer.issuer != *issuer {
            return Err(format!(
                "pid {pid}: {} is signed by {:?}, issued by {:?}, not rlx's publisher",
                image.display(),
                signer.subject,
                signer.issuer
            ));
        }
        Ok(format!(
            "GetNamedPipeClientProcessId: pid {pid}, held open since the connection, runs {}, \
             signed by {:?}, issued by {:?}{}",
            image.display(),
            signer.subject,
            signer.issuer,
            if signer.revocation_known {
                ""
            } else {
                "; its revocation status is unknown (nothing cached), allowed"
            }
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
    // Reading the image's mapped file name needs query and read rights beyond the
    // limited ones.
    let rights = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_QUERY_INFORMATION | PROCESS_VM_READ;
    // SAFETY: the call takes any pid, and returns a new handle we then own, or null.
    let raw = unsafe { OpenProcess(rights, 0, pid) };
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

/// The full path of the image the process was started from, through its handle.
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

/// The NT name (`\Device\…`) of the file the process's main image is mapped from now.
fn mapped_image_name(process: HANDLE) -> Result<Vec<u16>, String> {
    let mut module: HMODULE = ptr::null_mut();
    let mut needed = 0u32;
    // SAFETY: room for one module handle, the first: the main image.
    let listed = unsafe {
        K32EnumProcessModules(
            process,
            &mut module,
            mem::size_of::<HMODULE>() as u32,
            &mut needed,
        )
    };
    if listed == 0 || module.is_null() {
        return Err(format!(
            "its main image can't be found: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut buffer = vec![0u16; 32_768];
    // SAFETY: `module` is the image's base in that process; `buffer` holds its length.
    let length = unsafe {
        K32GetMappedFileNameW(
            process,
            module.cast_const(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
        )
    };
    if length == 0 {
        return Err(format!(
            "its image's file name can't be read: {}",
            std::io::Error::last_os_error()
        ));
    }
    buffer.truncate(length as usize);
    Ok(buffer)
}

/// Opens `path` for `access`, sharing reads only, so nobody renames, replaces or deletes
/// the file while it is held.
fn open_held(path: &[u16], access: u32) -> Result<OwnedHandle, String> {
    let name: Vec<u16> = path.iter().copied().chain([0]).collect();
    // SAFETY: `name` is NUL-terminated; the handle we get is closed by OwnedHandle.
    let raw = unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            FILE_SHARE_READ,
            ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(format!(
            "cannot open {}: {}",
            String::from_utf16_lossy(path),
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: `raw` is a handle CreateFileW just gave us; nothing else owns it.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw as _) })
}

/// The file's 64-bit volume serial number and 128-bit file id (`FileIdInfo`, which ReFS
/// needs): which file it is.
fn identity(file: &OwnedHandle) -> Result<(u64, [u8; 16]), String> {
    let mut info = FILE_ID_INFO::default();
    // SAFETY: `file` is open; `info` is a live FILE_ID_INFO of the size given.
    let read = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if read == 0 {
        return Err(format!(
            "GetFileInformationByHandleEx: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok((info.VolumeSerialNumber, info.FileId.Identifier))
}

/// The file at `image`, held, once it is shown to be the file `process`'s main image is
/// mapped from (see the module's doc).
fn held_image(process: HANDLE, image: &Path) -> Result<OwnedHandle, String> {
    let path: Vec<u16> = image.as_os_str().encode_wide().collect();
    let file = open_held(&path, FILE_GENERIC_READ)?;
    let mapped = mapped_image_name(process)?;
    let global: Vec<u16> = r"\\?\GLOBALROOT"
        .encode_utf16()
        .chain(mapped.iter().copied())
        .collect();
    let running = open_held(&global, FILE_READ_ATTRIBUTES)?;
    let moved = mapped_image_name(process)? != mapped;
    if moved || identity(&file)? != identity(&running)? {
        return Err(format!(
            "it runs {}, not the file now at {}: its image was moved or replaced",
            String::from_utf16_lossy(&mapped),
            image.display()
        ));
    }
    Ok(file)
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

/// Who signed a file: the signer certificate's full subject and issuer, and whether its
/// revocation status was known.
struct Signer {
    subject: String,
    issuer: String,
    revocation_known: bool,
}

/// `WinVerifyTrust`'s answers when a certificate's revocation status can't be known:
/// `CERT_E_REVOCATION_FAILURE`, `CRYPT_E_NO_REVOCATION_CHECK` and
/// `CRYPT_E_REVOCATION_OFFLINE`. `CRYPT_E_REVOKED` is not among them.
const REVOCATION_UNKNOWN: [u32; 3] = [0x800B_010E, 0x8009_2012, 0x8009_2013];

/// Who signed `file` (whose path is `path`), once `WinVerifyTrust` has verified its
/// Authenticode signature to a trusted root. Revocation is checked across the chain, the
/// root excepted, from the cache; an unknown status is a soft fail (see the module's
/// doc), a revoked certificate a hard one.
fn authenticode_signer(file: &OwnedHandle, path: &Path) -> Result<Signer, String> {
    match verify_trust(file, path, true) {
        Err(status) if REVOCATION_UNKNOWN.contains(&(status as u32)) => {
            verify_trust(file, path, false).map_err(trust_error)
        }
        result => result.map_err(trust_error),
    }
}

fn trust_error(status: i32) -> String {
    format!(
        "WinVerifyTrust: {}",
        std::io::Error::from_raw_os_error(status)
    )
}

/// One `WinVerifyTrust` of `file`, checking revocation or not: the signer, or its status.
fn verify_trust(file: &OwnedHandle, path: &Path, revocation: bool) -> Result<Signer, i32> {
    let name: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut info = WINTRUST_FILE_INFO {
        cbStruct: mem::size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: name.as_ptr(),
        hFile: file.as_raw_handle() as HANDLE,
        pgKnownSubject: ptr::null_mut(),
    };
    let (checks, flags) = if revocation {
        (
            WTD_REVOKE_WHOLECHAIN,
            WTD_REVOCATION_CHECK_CHAIN_EXCLUDE_ROOT | WTD_CACHE_ONLY_URL_RETRIEVAL,
        )
    } else {
        (WTD_REVOKE_NONE, WTD_CACHE_ONLY_URL_RETRIEVAL)
    };
    let mut data = WINTRUST_DATA {
        cbStruct: mem::size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: checks,
        dwProvFlags: flags,
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
        unsafe { signer_names(data.hWVTStateData) }.map(|(subject, issuer)| Signer {
            subject,
            issuer,
            revocation_known: revocation,
        })
    } else {
        Err(status)
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
    signer
}

/// `name`, an encoded X.500 name, as `CertNameToStrW` writes it (`CERT_X500_NAME_STR`).
fn name_string(name: &windows_sys::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB) -> String {
    let mut buffer = vec![0u16; 2048];
    // SAFETY: `name` is live; `buffer` holds its length in u16s.
    let length = unsafe {
        CertNameToStrW(
            X509_ASN_ENCODING,
            name,
            CERT_X500_NAME_STR,
            buffer.as_mut_ptr(),
            buffer.len() as u32,
        )
    };
    let end = (length as usize).saturating_sub(1).min(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

/// # Safety
///
/// `state` must be the open state of a successful `WinVerifyTrust`.
unsafe fn signer_names(state: HANDLE) -> Result<(String, String), i32> {
    /// `TRUST_E_NOSIGNATURE`, for a signer that can't be read.
    const NO_SIGNER: i32 = 0x800B_0100_u32 as i32;
    // SAFETY: the caller's promise.
    let provider = unsafe { WTHelperProvDataFromStateData(state) };
    if provider.is_null() {
        return Err(NO_SIGNER);
    }
    // SAFETY: `provider` is live while the state is open.
    let signer = unsafe { WTHelperGetProvSignerFromChain(provider, 0, 0, 0) };
    // SAFETY: `signer`, when not null, is live while the state is open.
    if signer.is_null() || unsafe { (*signer).csCertChain == 0 || (*signer).pasCertChain.is_null() }
    {
        return Err(NO_SIGNER);
    }
    // SAFETY: the chain holds at least one certificate; the first is the signer's.
    let certificate = unsafe { (*(*signer).pasCertChain).pCert };
    // SAFETY: `certificate`, when not null, is live while the state is open.
    if certificate.is_null() || unsafe { (*certificate).pCertInfo.is_null() } {
        return Err(NO_SIGNER);
    }
    // SAFETY: the certificate's info is live while the state is open.
    let info = unsafe { &*(*certificate).pCertInfo };
    Ok((name_string(&info.Subject), name_string(&info.Issuer)))
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
        let process = pinned.process.as_raw_handle() as HANDLE;
        let image = image_of(process).unwrap();
        assert!(same_path(&image, &std::env::current_exe().unwrap()));
        held_image(process, &image).expect("this test's own image is the one it runs");
    }
}
