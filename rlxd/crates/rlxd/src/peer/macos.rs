//! macOS: the peer's audit token, checked by Security.framework against the compiled-in
//! code requirement, plus the hardened runtime and no injection entitlements.
//!
//! `LOCAL_PEERTOKEN` names the last process to act on the client's end of the socket, not
//! the one that connected. So same-user code can connect, then let a signed, hardened
//! binary act on its socket last, and pass as that binary: the check is best-effort
//! against same-user code here too (`peer`'s doc). The cheap case is refused: a peer that
//! holds the connection on its standard input, output or error (fds 0 to 2), which is how
//! such code would lend a binary its socket without the binary's help.
#![allow(unsafe_code)]

use std::ffi::{c_char, c_void};
use std::fmt;
use std::mem;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
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

/// An `audit_token_t`: eight 32-bit words, the pid at index 5 and the pid version at 7.
type AuditToken = [u32; 8];

/// The audit token's pid word.
const PID: usize = 5;
/// The audit token's pid version word, which changes when the pid names a new process
/// (and on exec).
const PID_VERSION: usize = 7;

/// The peer of one Unix socket connection.
pub struct Peer {
    token: Result<AuditToken, String>,
    uid: Result<u32, String>,
    /// The socket, kept open to read its peer token again at each check.
    socket: Option<OwnedFd>,
    /// When rlxd accepted the connection, as seconds and microseconds since the epoch:
    /// a process at the token's pid that started later is not the peer.
    accepted: (u64, u64),
}

/// Now, as seconds and microseconds since the epoch.
fn now() -> (u64, u64) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (now.as_secs(), u64::from(now.subsec_micros()))
}

/// When process `pid` started, as seconds and microseconds since the epoch.
fn started(pid: u32) -> Option<(u64, u64)> {
    let pid = libc::c_int::try_from(pid).ok()?;
    // SAFETY: proc_bsdinfo is plain integers and byte arrays, for which zero is valid.
    let mut info: libc::proc_bsdinfo = unsafe { mem::zeroed() };
    let size = libc::c_int::try_from(mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: `info` is a writable proc_bsdinfo of `size` bytes, as PROC_PIDTBSDINFO
    // fills.
    let written =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    (written == size).then_some((info.pbi_start_tvsec, info.pbi_start_tvusec))
}

/// The socket's `LOCAL_PEERTOKEN`.
fn peer_token(fd: RawFd) -> Result<AuditToken, String> {
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
    if read == 0 && length as usize == mem::size_of::<AuditToken>() {
        Ok(token)
    } else {
        Err(format!(
            "LOCAL_PEERTOKEN: {}",
            std::io::Error::last_os_error()
        ))
    }
}

impl Peer {
    pub fn of_socket(fd: BorrowedFd<'_>) -> Self {
        let socket = fd.try_clone_to_owned().ok();
        let fd = fd.as_raw_fd();
        let token = peer_token(fd);
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: `fd` is an open socket, and both pointers are to live integers.
        let uid = if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } == 0 {
            Ok(uid)
        } else {
            Err(format!("getpeereid: {}", std::io::Error::last_os_error()))
        };
        Self {
            token,
            uid,
            socket,
            accepted: now(),
        }
    }

    /// [`super::Peer::while_pinned`]: the process at the token's pid must have started
    /// before rlxd accepted the connection, before `check` and after it.
    ///
    /// A pid and a start time, not a held handle: macOS's live pid version is in no public
    /// API. Two windows stay open. A peer that exits between its `connect` and rlxd's
    /// `accept`, its pid reused within that instant, passes as the new process; and both
    /// times are the wall clock, so a process started after the clock was stepped back
    /// past the accept passes too. Neither is in reach: macOS hands out pids in order up to
    /// 99,999, so no one can aim a reused pid at a backlog window of under a millisecond,
    /// and a pid that passes the gate must also lie inside the run, whose processes hold
    /// the key already.
    pub fn while_pinned(&self, check: impl FnOnce(u32) -> bool) -> bool {
        let Some(pid) = self.pid() else {
            return false;
        };
        let the_peer = || started(pid).is_some_and(|start| start <= self.accepted);
        the_peer() && check(pid) && the_peer()
    }

    pub fn pid(&self) -> Option<u32> {
        self.token.as_ref().ok().map(|token| token[PID])
    }

    pub fn image(&self) -> Option<String> {
        let pid = i32::try_from(self.pid()?).ok()?;
        let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: `buffer` holds as many bytes as the call is told.
        let length =
            unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
        let length = usize::try_from(length).ok().filter(|n| *n > 0)?;
        Some(String::from_utf8_lossy(&buffer[..length]).into_owned())
    }

    /// The token pinned at accept, after checking the socket still names that very
    /// process: the same pid and pid version.
    fn pinned_token(&self) -> Result<AuditToken, String> {
        let pinned = self.token.clone()?;
        let socket = self
            .socket
            .as_ref()
            .ok_or("LOCAL_PEERTOKEN: the socket could not be kept to check again")?;
        let now = peer_token(socket.as_raw_fd())?;
        if now[PID] != pinned[PID] || now[PID_VERSION] != pinned[PID_VERSION] {
            return Err(format!(
                "LOCAL_PEERTOKEN: the socket's peer changed since it connected (pid {} \
                 version {}, now pid {} version {})",
                pinned[PID], pinned[PID_VERSION], now[PID], now[PID_VERSION]
            ));
        }
        Ok(pinned)
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
        let token = self.pinned_token()?;
        let pid = token[PID];
        let socket = self
            .socket
            .as_ref()
            .ok_or("LOCAL_PEERTOKEN: the socket could not be kept to check again")?;
        if let Some(fd) = stdio_end(pid, socket.as_raw_fd())? {
            return Err(format!(
                "LOCAL_PEERTOKEN of pid {pid}: it holds this connection on its fd {fd}, a \
                 standard stream: a process that was only lent the connection is not the \
                 app"
            ));
        }
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
            Ok(token) => write!(f, "pid {}", token[PID]),
            Err(_) => f.write_str("an unknown pid"),
        }
    }
}

/// `proc_pidfdinfo`'s `PROC_PIDFDSOCKETINFO`, and its `struct socket_fdinfo`: its size,
/// and the offsets of the fields read here (`<sys/proc_info.h>`, the same on arm64 and
/// x86_64).
const PROC_PIDFDSOCKETINFO: i32 = 3;
const SOCKET_FDINFO_SIZE: usize = 792;
/// `psi.soi_so` and `psi.soi_pcb`: the socket, and its protocol block.
const SOI_SO: usize = 160;
const SOI_PCB: usize = 168;
/// `psi.soi_kind`, `SOCKINFO_UN` for a Unix socket.
const SOI_KIND: usize = 256;
const SOCKINFO_UN: i32 = 3;
/// `psi.soi_proto.pri_un.unsi_conn_so` and `unsi_conn_pcb`: the socket it is connected to.
const UNSI_CONN_SO: usize = 264;
const UNSI_CONN_PCB: usize = 272;

/// `fd` of process `pid`, as `PROC_PIDFDSOCKETINFO` describes it, when it is a socket.
fn socket_info(pid: i32, fd: i32) -> Option<Vec<u8>> {
    let mut info = vec![0u8; SOCKET_FDINFO_SIZE];
    // SAFETY: `info` holds SOCKET_FDINFO_SIZE bytes, as the call is told.
    let size = unsafe {
        libc::proc_pidfdinfo(
            pid,
            fd,
            PROC_PIDFDSOCKETINFO,
            info.as_mut_ptr().cast(),
            SOCKET_FDINFO_SIZE as i32,
        )
    };
    (usize::try_from(size).ok() == Some(SOCKET_FDINFO_SIZE)).then_some(info)
}

fn read_u64(info: &[u8], at: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&info[at..at + 8]);
    u64::from_ne_bytes(bytes)
}

fn read_i32(info: &[u8], at: usize) -> i32 {
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&info[at..at + 4]);
    i32::from_ne_bytes(bytes)
}

/// The fd, 0 to 2, on which process `pid` holds the other end of `socket`, rlxd's end of
/// a connection: whether the connection is that process's standard stream.
fn stdio_end(pid: u32, socket: RawFd) -> Result<Option<i32>, String> {
    // SAFETY: getpid cannot fail.
    let me = unsafe { libc::getpid() };
    let ours = socket_info(me, socket).ok_or("rlxd's own socket can't be described")?;
    let (so, pcb) = (read_u64(&ours, SOI_SO), read_u64(&ours, SOI_PCB));
    let pid = i32::try_from(pid).map_err(|_| format!("pid {pid} is out of range"))?;
    for fd in 0..3 {
        let Some(theirs) = socket_info(pid, fd) else {
            continue;
        };
        let connected = read_i32(&theirs, SOI_KIND) == SOCKINFO_UN
            && (read_u64(&theirs, UNSI_CONN_SO) == so || read_u64(&theirs, UNSI_CONN_PCB) == pcb);
        if connected {
            return Ok(Some(fd));
        }
    }
    Ok(None)
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
    fn a_process_holding_the_connection_on_its_stdin_is_found() {
        use std::os::fd::OwnedFd;
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::process::{Command, Stdio};

        let dir = std::env::temp_dir().join(format!("rlxd-stdio-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s");
        let listener = UnixListener::bind(&path).unwrap();
        let client = UnixStream::connect(&path).unwrap();
        let (server, _) = listener.accept().unwrap();
        let mut lent = Command::new("/bin/sleep")
            .arg("10")
            .stdin(Stdio::from(OwnedFd::from(client.try_clone().unwrap())))
            .spawn()
            .unwrap();
        let mut other = Command::new("/bin/sleep")
            .arg("10")
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let found = stdio_end(lent.id(), server.as_raw_fd());
        let not = stdio_end(other.id(), server.as_raw_fd());
        let _ = lent.kill();
        let _ = other.kill();
        let _ = lent.wait();
        let _ = other.wait();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(found, Ok(Some(0)));
        assert_eq!(not, Ok(None));
        assert_eq!(stdio_end(std::process::id(), server.as_raw_fd()), Ok(None));
    }

    #[test]
    fn an_unknown_status_still_reads() {
        assert!(!message(-67050).is_empty());
    }
}
