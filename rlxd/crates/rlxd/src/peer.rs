//! Who is on the other end of a connection, and whether it is rlx's app (handoff 0.10.3
//! rev 6, 5.1: "the peer's identity, where the OS allows it").
//!
//! rlxd pins the connecting process the moment it accepts a connection ([`Peer`]), and
//! checks its identity later, when the connection asks for the gate: a `hello {role:
//! "app"}` or a `gate.pair`. The gate credential is required as well, always; this is the
//! second proof.
//!
//! - **macOS:** the socket's `LOCAL_PEERTOKEN` audit token (never `LOCAL_PEERPID`, which a
//!   reused pid could spoof), handed to `SecCodeCopyGuestWithAttributes`, then
//!   `SecCodeCheckValidity` against [`MACOS_REQUIREMENT`], which pins Apple's anchor, the
//!   app's identifier and its team. The peer must also run with the hardened runtime and
//!   hold none of [`MACOS_REFUSED_ENTITLEMENTS`]. Raw FFI to Security.framework; rlxd never
//!   runs Apple's signing tool, and takes no Keychain crate (4.3).
//! - **Linux:** `SO_PEERCRED` must name rlxd's own uid, and `SO_PEERPIDFD` (Linux 6.5 and
//!   later) pins the process; older kernels pin it with `pidfd_open` on the peer's pid at
//!   accept, and the log says the check is weaker. Where rlx's app is installed from a
//!   package ([`LINUX_PACKAGED_BINARIES`]), `/proc/<pid>/exe` must be that binary. An
//!   AppImage has no fixed path, so without a packaged install the peer passes on its uid,
//!   on being alive and on its binary's file name being [`LINUX_APP_NAME`]: Linux's weaker
//!   guarantee, which is not dev mode. The name keeps out other clients that hold the
//!   token, such as rlx's TUI or a script, by mistake; it does not keep out same-user code
//!   that means to pass. **Pending rlxapp:** the packaged paths, the AppImage's inner
//!   binary name, and Flatpak, whose binary rlxd sees through the sandbox's own mount
//!   namespace, so it has no host path here yet.
//! - **Windows:** `GetNamedPipeClientProcessId` as soon as the pipe connects, then
//!   `OpenProcess` at once, and the process's creation time must not be later than the
//!   connection, so a reused pid can't pass. The connection's time is taken when the
//!   pipe's waker fires, before the accept loop runs; a pid freed and reused within that
//!   instant would still pass, a window too short to aim at. Through the held handle the
//!   process must still run, its image must be one of [`WINDOWS_BINARIES`], and the
//!   Authenticode signature of the very file that image is mapped from must verify
//!   (`WinVerifyTrust`, revocation checked) with [`WINDOWS_PUBLISHER`]'s full subject as
//!   its signer (`peer::windows` says how the file is bound to the image).
//!
//! **On Linux and Windows the peer check is best-effort against code running as the same
//! user.** Such code can connect, hand the connection on and exec rlx's app, or inject into
//! it. There, the gate credential, which only the app's OS secure store holds, is the
//! real boundary. On macOS the code-signing check, with the hardened runtime and no
//! injection entitlements, holds against same-user code too.
//!
//! **A device paired over SSH** would be checked by its credential alone: its
//! connection's peer is sshd, not the app, so the SSH-authenticated pairing plus that
//! device's own credential would be the boundary. SSH pairing isn't built yet (hub q1),
//! so rlxd mints no ssh-route credential, and an ssh-route device in gate.json is refused
//! (`gate::ssh_refused`): it fails closed until SSH pairing lands.
//!
//! **Tests only.** With the cargo feature `gate-test-requirement`, one environment
//! variable replaces what the peer must match: the macOS requirement, the Linux binary, or
//! the Windows publisher's full subject and binary (`<subject>|<path>`, the subject as
//! `CertNameToStrW` writes it with `CERT_X500_NAME_STR`). Release builds never enable
//! the feature, and in them no config key, variable or flag changes any of it.

use std::fmt;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
use linux as os;
#[cfg(target_os = "macos")]
use macos as os;
#[cfg(windows)]
use windows as os;

#[cfg(target_os = "macos")]
pub use macos::static_code_satisfies;

/// Expands to rlx's app's Apple team ID: its Developer ID certificate's OU. Until that
/// certificate exists this is a placeholder that no Apple-issued certificate carries, so
/// outside gate dev mode no macOS peer passes. rlxapp's first signed release sets it.
macro_rules! team_id {
    () => {
        "RLXAPP-TEAM-ID-UNSET"
    };
}

/// rlx's app's Apple team ID, as [`MACOS_REQUIREMENT`] names it.
pub const MACOS_TEAM_ID: &str = team_id!();

/// The code requirement a macOS gate peer must satisfy, compiled in. Without
/// `anchor apple generic`, a self-signed certificate with this identifier and OU would
/// pass. Once rlxapp's Developer ID certificate exists, this becomes its full designated
/// requirement.
pub const MACOS_REQUIREMENT: &str = concat!(
    "anchor apple generic and identifier \"studio.aisquare.rlx\" and certificate leaf[subject.OU] = \"",
    team_id!(),
    "\""
);

/// The entitlements a macOS gate peer must not hold: each lets a debugger or an injected
/// library run inside a process that passes the requirement.
pub const MACOS_REFUSED_ENTITLEMENTS: &[&str] = &[
    "com.apple.security.get-task-allow",
    "com.apple.security.cs.disable-library-validation",
    "com.apple.security.cs.allow-dyld-environment-variables",
];

/// The file name of rlx's app's Linux binary, in a package or an AppImage. Pending
/// rlxapp's packaging.
pub const LINUX_APP_NAME: &str = "rlx-app";

/// Where rlx's app's Linux packages (.deb, .rpm) install its binary. When one of these
/// exists, a Linux gate peer must be it. Pending rlxapp's packaging, as is a Flatpak
/// path (see the module's doc).
pub const LINUX_PACKAGED_BINARIES: &[&str] = &["/usr/bin/rlx-app", "/usr/lib/rlx-app/rlx-app"];

/// Where rlx's app's Windows installers put its executable: per user, then per machine.
/// `%NAME%` is that environment variable of rlxd's. rlxapp's installer confirms them.
pub const WINDOWS_BINARIES: &[&str] = &[
    r"%LOCALAPPDATA%\Programs\rlx\rlx.exe",
    r"%ProgramFiles%\rlx\rlx.exe",
];

/// The full subject of the certificate rlx's app's Windows executable is signed with,
/// as `CertNameToStrW` writes it with `CERT_X500_NAME_STR` (such as `CN=…, O=…, C=…`).
/// `None` until rlxapp's certificate exists: then no Windows peer passes outside dev
/// mode, whatever certificate it carries.
pub const WINDOWS_PUBLISHER: Option<&str> = None;

/// The process at the other end of one connection, pinned when rlxd accepted it.
pub struct Peer {
    inner: os::Peer,
}

impl Peer {
    /// The peer of a Unix socket connection, pinned now.
    #[cfg(unix)]
    pub fn of_socket(fd: std::os::fd::BorrowedFd<'_>) -> Self {
        Self {
            inner: os::Peer::of_socket(fd),
        }
    }

    /// The client of a named pipe instance that just connected, pinned now.
    /// `connected_at` is when the connection completed, as a `FILETIME` count.
    #[cfg(windows)]
    pub fn of_pipe(pipe: std::os::windows::io::BorrowedHandle<'_>, connected_at: u64) -> Self {
        Self {
            inner: os::Peer::of_pipe(pipe, connected_at),
        }
    }

    /// Whether the peer runs as the user rlxd runs as. Checked for every gate request,
    /// in dev mode too.
    pub fn same_user(&self) -> Result<(), String> {
        self.inner.same_user()
    }

    /// Whether the peer is rlx's app, as this OS can tell (see the module's doc). `Ok`
    /// says how it passed, for the log; `Err` says why not. It can take a few
    /// milliseconds: call it off the async runtime. Each call checks afresh, so a check
    /// made again after a wait sees the peer as it is then.
    pub fn verify_identity(&self) -> Result<String, String> {
        self.inner.verify_identity(&expected())
    }

    /// The peer's pid, as pinned, when the OS gave one. For people to read: the identity
    /// check never trusts a bare pid.
    pub fn pid(&self) -> Option<u32> {
        self.inner.pid()
    }

    /// The binary the peer runs, as the OS names it now, when it can. For people to
    /// read, as [`Peer::pid`].
    pub fn image(&self) -> Option<String> {
        self.inner.image()
    }
}

/// Now, as a `FILETIME` count, for [`Peer::of_pipe`]'s `connected_at`.
#[cfg(windows)]
pub fn connected_now() -> u64 {
    os::now()
}

impl fmt::Display for Peer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

/// What a gate peer must match: the compiled-in identity, or in a test build, the
/// override.
pub(crate) struct Expected {
    /// macOS: the requirement's text.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub requirement: String,
    /// Linux and Windows: the binaries the peer may be. `None` on Linux when rlx's app is
    /// not installed from a package, so that only the binary's file name is checked.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub binaries: Option<Vec<std::path::PathBuf>>,
    /// Windows: the signer's full subject, or `None` when none is pinned.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub publisher: Option<String>,
}

#[cfg(feature = "gate-test-requirement")]
const TEST_OVERRIDE: &str = "RLXD_TEST_GATE_REQUIREMENT";

/// The test override, in a test build when it is set.
fn test_override() -> Option<String> {
    #[cfg(feature = "gate-test-requirement")]
    {
        std::env::var(TEST_OVERRIDE).ok().filter(|v| !v.is_empty())
    }
    #[cfg(not(feature = "gate-test-requirement"))]
    {
        None
    }
}

/// Logs, at start, that this is a test build whose gate identity can be overridden.
pub fn log_test_override() {
    #[cfg(feature = "gate-test-requirement")]
    match test_override() {
        Some(value) => crate::log::warning!(
            "a test build: {TEST_OVERRIDE} replaces the gate peer's identity with {value:?}"
        ),
        None => crate::log::warning!(
            "a test build: {TEST_OVERRIDE} is unset, so the compiled-in gate identity holds"
        ),
    }
}

fn expected() -> Expected {
    let test = test_override();
    let requirement = match (&test, cfg!(target_os = "macos")) {
        (Some(value), true) => value.clone(),
        _ => MACOS_REQUIREMENT.to_owned(),
    };
    let (publisher, binaries) = if cfg!(windows) {
        match test.as_deref().and_then(|v| v.split_once('|')) {
            Some((publisher, path)) => (Some(publisher.to_owned()), Some(vec![path.into()])),
            None => (
                WINDOWS_PUBLISHER.map(str::to_owned),
                Some(WINDOWS_BINARIES.iter().filter_map(|p| expand(p)).collect()),
            ),
        }
    } else if cfg!(target_os = "linux") {
        let binaries = match test {
            Some(path) => Some(vec![path.into()]),
            None => {
                let installed: Vec<std::path::PathBuf> = LINUX_PACKAGED_BINARIES
                    .iter()
                    .map(std::path::PathBuf::from)
                    .filter(|path| path.exists())
                    .collect();
                (!installed.is_empty()).then_some(installed)
            }
        };
        (None, binaries)
    } else {
        (None, None)
    };
    Expected {
        requirement,
        binaries,
        publisher,
    }
}

/// `path` with each `%NAME%` replaced by that variable, or `None` when one is unset.
fn expand(path: &str) -> Option<std::path::PathBuf> {
    let mut out = String::new();
    let mut rest = path;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let end = after.find('%')?;
        let value = std::env::var(&after[..end])
            .ok()
            .filter(|v| !v.is_empty())?;
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Some(out.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_macos_requirement_pins_apples_anchor_the_identifier_and_the_team() {
        assert!(MACOS_REQUIREMENT.starts_with("anchor apple generic and "));
        assert!(MACOS_REQUIREMENT.contains("identifier \"studio.aisquare.rlx\""));
        assert!(MACOS_REQUIREMENT.ends_with(&format!(
            "certificate leaf[subject.OU] = \"{MACOS_TEAM_ID}\""
        )));
    }

    #[test]
    fn a_path_expands_its_variables_or_is_none() {
        // SAFETY of the test: PATH is set in every environment that runs it.
        let path = std::env::var("PATH").unwrap();
        assert_eq!(expand("%PATH%/x"), Some(format!("{path}/x").into()));
        assert_eq!(expand("plain"), Some("plain".into()));
        assert_eq!(expand("%RLXD_NO_SUCH_VARIABLE%/x"), None);
        assert_eq!(expand("%unclosed"), None);
    }
}
