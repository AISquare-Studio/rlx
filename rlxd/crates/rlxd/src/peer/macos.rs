//! macOS: the peer's audit token, checked by Security.framework against the compiled-in
//! code requirement, plus the hardened runtime and no injection entitlements.
#![allow(unsafe_code)]

use std::ffi::{c_char, c_void};
use std::fmt;
use std::mem;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::ptr;

use super::{Expected, MACOS_REFUSED_ENTITLEMENTS};

type CFTypeRef = *const c_void;
type CFIndex = isize;
type OSStatus = i32;
type SecCSFlags = u32;

/// `kCFStringEncodingUTF8`.
const UTF8: u32 = 0x0800_0100;
/// `kCFNumberSInt64Type`.
const SINT64: CFIndex = 4;
/// `kSecCSDefaultFlags`.
const DEFAULT_FLAGS: SecCSFlags = 0;
/// `kSecCSSigningInformation | kSecCSRequirementInformation`: the signature's flags and
/// the entitlements.
const SIGNING_AND_REQUIREMENTS: SecCSFlags = (1 << 1) | (1 << 2);
/// `kSecCodeSignatureRuntime`: the code runs with the hardened runtime.
const RUNTIME: u64 = 0x0001_0000;
/// `errSecSuccess`.
const OK: OSStatus = 0;

/// `CFDictionaryKeyCallBacks`, opaque: only its address is passed.
#[repr(C)]
struct KeyCallBacks {
    _fields: [usize; 6],
}

/// `CFDictionaryValueCallBacks`, opaque: only its address is passed.
#[repr(C)]
struct ValueCallBacks {
    _fields: [usize; 5],
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: KeyCallBacks;
    static kCFTypeDictionaryValueCallBacks: ValueCallBacks;
    fn CFRelease(cf: CFTypeRef);
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFBooleanGetTypeID() -> usize;
    fn CFBooleanGetValue(boolean: CFTypeRef) -> u8;
    fn CFNumberGetTypeID() -> usize;
    fn CFNumberGetValue(number: CFTypeRef, kind: CFIndex, value: *mut c_void) -> u8;
    fn CFDictionaryGetTypeID() -> usize;
    fn CFDataCreate(allocator: CFTypeRef, bytes: *const u8, length: CFIndex) -> CFTypeRef;
    fn CFStringCreateWithBytes(
        allocator: CFTypeRef,
        bytes: *const u8,
        length: CFIndex,
        encoding: u32,
        external: u8,
    ) -> CFTypeRef;
    fn CFStringGetLength(string: CFTypeRef) -> CFIndex;
    fn CFStringGetMaximumSizeForEncoding(length: CFIndex, encoding: u32) -> CFIndex;
    fn CFStringGetCString(
        string: CFTypeRef,
        buffer: *mut c_char,
        size: CFIndex,
        encoding: u32,
    ) -> u8;
    fn CFDictionaryCreate(
        allocator: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        count: CFIndex,
        key_callbacks: *const KeyCallBacks,
        value_callbacks: *const ValueCallBacks,
    ) -> CFTypeRef;
    fn CFDictionaryGetValue(dictionary: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    fn CFURLCreateFromFileSystemRepresentation(
        allocator: CFTypeRef,
        path: *const u8,
        length: CFIndex,
        is_directory: u8,
    ) -> CFTypeRef;
}

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    static kSecGuestAttributeAudit: CFTypeRef;
    static kSecCodeInfoFlags: CFTypeRef;
    static kSecCodeInfoEntitlementsDict: CFTypeRef;
    fn SecCodeCopyGuestWithAttributes(
        host: CFTypeRef,
        attributes: CFTypeRef,
        flags: SecCSFlags,
        guest: *mut CFTypeRef,
    ) -> OSStatus;
    fn SecRequirementCreateWithString(
        text: CFTypeRef,
        flags: SecCSFlags,
        requirement: *mut CFTypeRef,
    ) -> OSStatus;
    fn SecCodeCheckValidity(code: CFTypeRef, flags: SecCSFlags, requirement: CFTypeRef)
    -> OSStatus;
    fn SecCodeCopySigningInformation(
        code: CFTypeRef,
        flags: SecCSFlags,
        information: *mut CFTypeRef,
    ) -> OSStatus;
    fn SecStaticCodeCreateWithPath(
        path: CFTypeRef,
        flags: SecCSFlags,
        code: *mut CFTypeRef,
    ) -> OSStatus;
    fn SecStaticCodeCheckValidity(
        code: CFTypeRef,
        flags: SecCSFlags,
        requirement: CFTypeRef,
    ) -> OSStatus;
    fn SecCopyErrorMessageString(status: OSStatus, reserved: *mut c_void) -> CFTypeRef;
}

/// A Core Foundation object this code owns, released on drop.
struct Owned(CFTypeRef);

impl Owned {
    /// `object`, owned, or `None` when it is null.
    fn new(object: CFTypeRef) -> Option<Self> {
        (!object.is_null()).then_some(Self(object))
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a non-null object this code got from a Create or Copy call,
        // released once, here.
        unsafe { CFRelease(self.0) }
    }
}

/// An `audit_token_t`: eight 32-bit words, the pid at index 5.
type AuditToken = [u32; 8];

/// The peer of one Unix socket connection.
pub struct Peer {
    token: Result<AuditToken, String>,
    uid: Result<u32, String>,
}

impl Peer {
    pub fn of_socket(fd: BorrowedFd<'_>) -> Self {
        let fd = fd.as_raw_fd();
        let mut token: AuditToken = [0; 8];
        let mut length = mem::size_of::<AuditToken>() as libc::socklen_t;
        // SAFETY: `fd` is an open socket, and `token` is as large as `length` says.
        let read = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_LOCAL,
                libc::LOCAL_PEERTOKEN,
                token.as_mut_ptr().cast(),
                &mut length,
            )
        };
        let token = if read == 0 && length as usize == mem::size_of::<AuditToken>() {
            Ok(token)
        } else {
            Err(format!(
                "LOCAL_PEERTOKEN: {}",
                std::io::Error::last_os_error()
            ))
        };
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: `fd` is an open socket, and both pointers are to live integers.
        let uid = if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } == 0 {
            Ok(uid)
        } else {
            Err(format!("getpeereid: {}", std::io::Error::last_os_error()))
        };
        Self { token, uid }
    }

    pub fn same_user(&self) -> Result<(), String> {
        let uid = self.uid.clone()?;
        // SAFETY: geteuid cannot fail.
        let mine = unsafe { libc::geteuid() };
        if uid == mine {
            Ok(())
        } else {
            Err(format!("the peer runs as uid {uid}, not rlxd's {mine}"))
        }
    }

    pub fn verify_identity(&self, expected: &Expected) -> Result<String, String> {
        let token = self.token.clone()?;
        let pid = token[5];
        let requirement = requirement(&expected.requirement)?;
        let guest = guest_of(&token)
            .map_err(|e| format!("LOCAL_PEERTOKEN of pid {pid}: no code for it: {e}"))?;
        // SAFETY: both are live objects of the types the call takes.
        let status = unsafe { SecCodeCheckValidity(guest.0, DEFAULT_FLAGS, requirement.0) };
        if status != OK {
            return Err(format!(
                "LOCAL_PEERTOKEN of pid {pid}: its code does not satisfy the gate's \
                 requirement: {}",
                message(status)
            ));
        }
        hardened(&guest).map_err(|e| format!("LOCAL_PEERTOKEN of pid {pid}: {e}"))?;
        Ok(format!(
            "LOCAL_PEERTOKEN audit token of pid {pid}: signed to the gate's requirement, \
             hardened runtime, no injection entitlements"
        ))
    }
}

impl fmt::Display for Peer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.token {
            Ok(token) => write!(f, "pid {}", token[5]),
            Err(_) => f.write_str("an unknown pid"),
        }
    }
}

/// Whether the code at `path`, on disk, satisfies `requirement`
/// (`SecStaticCodeCheckValidity`). rlxd's tests use it as a control.
pub fn static_code_satisfies(path: &std::path::Path, requirement: &str) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: `bytes` is valid for its length; the URL is released by `Owned`.
    let url = Owned::new(unsafe {
        CFURLCreateFromFileSystemRepresentation(
            ptr::null(),
            bytes.as_ptr(),
            bytes.len() as CFIndex,
            0,
        )
    })
    .ok_or("no URL for the path")?;
    let requirement = self::requirement(requirement)?;
    let mut code: CFTypeRef = ptr::null();
    // SAFETY: `url` is a live CFURL, and `code` receives an object we then own.
    let status = unsafe { SecStaticCodeCreateWithPath(url.0, DEFAULT_FLAGS, &mut code) };
    if status != OK {
        return Err(message(status));
    }
    let code = Owned::new(code).ok_or("no code object")?;
    // SAFETY: both are live objects of the types the call takes.
    match unsafe { SecStaticCodeCheckValidity(code.0, DEFAULT_FLAGS, requirement.0) } {
        OK => Ok(()),
        status => Err(message(status)),
    }
}

fn cf_string(text: &str) -> Result<Owned, String> {
    // SAFETY: `text` is valid UTF-8 for its length.
    Owned::new(unsafe {
        CFStringCreateWithBytes(ptr::null(), text.as_ptr(), text.len() as CFIndex, UTF8, 0)
    })
    .ok_or_else(|| "cannot make a CFString".to_owned())
}

fn requirement(text: &str) -> Result<Owned, String> {
    let text = cf_string(text)?;
    let mut requirement: CFTypeRef = ptr::null();
    // SAFETY: `text` is a live CFString; `requirement` receives an object we then own.
    let status = unsafe { SecRequirementCreateWithString(text.0, DEFAULT_FLAGS, &mut requirement) };
    if status != OK {
        return Err(format!(
            "the gate's requirement does not compile: {}",
            message(status)
        ));
    }
    Owned::new(requirement).ok_or_else(|| "no requirement object".to_owned())
}

/// The running code `token` names, as a guest of the kernel.
fn guest_of(token: &AuditToken) -> Result<Owned, String> {
    // SAFETY: `token` is 32 readable bytes.
    let data = Owned::new(unsafe {
        CFDataCreate(
            ptr::null(),
            token.as_ptr().cast(),
            mem::size_of::<AuditToken>() as CFIndex,
        )
    })
    .ok_or("cannot make a CFData")?;
    // SAFETY: the key is Security.framework's constant, and `data` is live.
    let (key, value) = unsafe { (kSecGuestAttributeAudit, data.0) };
    // SAFETY: one key and one value, both live; the callbacks are CF's own.
    let attributes = Owned::new(unsafe {
        CFDictionaryCreate(
            ptr::null(),
            &key,
            &value,
            1,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    })
    .ok_or("cannot make a CFDictionary")?;
    let mut guest: CFTypeRef = ptr::null();
    // SAFETY: a null host means the kernel; `guest` receives an object we then own.
    let status = unsafe {
        SecCodeCopyGuestWithAttributes(ptr::null(), attributes.0, DEFAULT_FLAGS, &mut guest)
    };
    if status != OK {
        return Err(message(status));
    }
    Owned::new(guest).ok_or_else(|| "no code object".to_owned())
}

/// Whether `code` runs with the hardened runtime and holds none of the refused
/// entitlements.
fn hardened(code: &Owned) -> Result<(), String> {
    let mut information: CFTypeRef = ptr::null();
    // SAFETY: `code` is a live SecCode; `information` receives a dictionary we then own.
    let status = unsafe {
        SecCodeCopySigningInformation(code.0, SIGNING_AND_REQUIREMENTS, &mut information)
    };
    if status != OK {
        return Err(format!(
            "its signing information is unreadable: {}",
            message(status)
        ));
    }
    let information = Owned::new(information).ok_or("no signing information")?;
    // SAFETY: the key is Security.framework's constant; the value, if any, is borrowed
    // from `information`, which outlives it here.
    let flags = unsafe { CFDictionaryGetValue(information.0, kSecCodeInfoFlags) };
    let mut value: i64 = 0;
    // SAFETY: `flags` is checked to be a CFNumber before it is read into an i64.
    let read = !flags.is_null()
        && unsafe { CFGetTypeID(flags) == CFNumberGetTypeID() }
        && unsafe { CFNumberGetValue(flags, SINT64, (&mut value as *mut i64).cast()) } != 0;
    if !read {
        return Err("its signature's flags are unreadable".to_owned());
    }
    if (value as u64) & RUNTIME == 0 {
        return Err("it does not run with the hardened runtime".to_owned());
    }
    // SAFETY: as for the flags.
    let entitlements = unsafe { CFDictionaryGetValue(information.0, kSecCodeInfoEntitlementsDict) };
    if entitlements.is_null() {
        return Ok(());
    }
    // SAFETY: `entitlements` is a live object borrowed from `information`.
    if unsafe { CFGetTypeID(entitlements) != CFDictionaryGetTypeID() } {
        return Err("its entitlements are unreadable".to_owned());
    }
    for name in MACOS_REFUSED_ENTITLEMENTS {
        let key = cf_string(name)?;
        // SAFETY: `entitlements` is a live dictionary and `key` a live CFString.
        let granted = unsafe { CFDictionaryGetValue(entitlements, key.0) };
        if granted.is_null() {
            continue;
        }
        // SAFETY: `granted` is a live object borrowed from the dictionary.
        let is_false = unsafe {
            CFGetTypeID(granted) == CFBooleanGetTypeID() && CFBooleanGetValue(granted) == 0
        };
        if !is_false {
            return Err(format!("it holds the entitlement {name}"));
        }
    }
    Ok(())
}

/// What Security.framework says `status` means.
fn message(status: OSStatus) -> String {
    // SAFETY: the call takes any status, and returns a CFString we own, or null.
    let Some(text) = Owned::new(unsafe { SecCopyErrorMessageString(status, ptr::null_mut()) })
    else {
        return format!("OSStatus {status}");
    };
    // SAFETY: `text` is a live CFString.
    let length = unsafe { CFStringGetLength(text.0) };
    // SAFETY: a pure computation.
    let size = unsafe { CFStringGetMaximumSizeForEncoding(length, UTF8) } + 1;
    let mut buffer = vec![0u8; usize::try_from(size).unwrap_or(1).max(1)];
    // SAFETY: `buffer` holds `size` bytes, room for the string and its NUL.
    let copied = unsafe { CFStringGetCString(text.0, buffer.as_mut_ptr().cast(), size, UTF8) } != 0;
    if !copied {
        return format!("OSStatus {status}");
    }
    let end = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
    format!("{} ({status})", String::from_utf8_lossy(&buffer[..end]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_compiled_in_requirement_compiles() {
        requirement(super::super::MACOS_REQUIREMENT).unwrap();
        assert!(requirement("not a requirement (").is_err());
    }

    #[test]
    fn this_test_binary_does_not_satisfy_the_compiled_in_requirement() {
        let me = std::env::current_exe().unwrap();
        assert!(static_code_satisfies(&me, super::super::MACOS_REQUIREMENT).is_err());
    }

    #[test]
    fn an_unknown_status_still_reads() {
        assert!(!message(-67050).is_empty());
    }
}
