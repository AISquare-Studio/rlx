//! The home, and every path rlxd and its clients derive from it. This module is the one
//! place those paths are made, so a change to the home's layout is a change here.
//!
//! ```text
//! <home>/
//!   run/rlxd.sock     the socket clients connect to (macOS, Linux); 0700 dir, 0600 socket
//!   run/rlxd.lock     held while rlxd runs: one rlxd per home
//!   rlxd/token        the shared client token (0600)
//!   rlxd/gate.json    the paired gate devices, each credential's SHA-256 only (0600)
//!   rlxd/config.toml  rlxd's config, such as `gate.dev_mode` (optional)
//!   rlxd/logs/        rlxd's logs, rotated
//!   daemons/          the registry of non-UI rlx processes (4.1)
//! ```
//!
//! On Windows, clients connect to the named pipe `\\.\pipe\rlxd-<hash of home>` instead
//! ([`Home::pipe_name`]). The socket and the lock sit under `run/`, where rlx 0.10.2's
//! layout keeps sockets and locks. The rest of rlxd's state stays under `rlxd/`.
//!
//! A socket's path must fit the OS's `sockaddr_un` ([`SOCKET_PATH_MAX`]). When a home is
//! nested too deep for `run/rlxd.sock` to fit, the socket is
//! `/tmp/rlxd-<uid>/<hash of home>.sock` instead ([`Home::short_socket`]), in a folder
//! only its user may enter. [`Home::socket`] says which one a home uses, so rlxd and every
//! client agree, and `status` and `rlxd here` report it.

use std::env;
use std::ffi::OsString;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

/// The variable that names the home. It wins over [`LEGACY_HOME_ENV`].
pub const HOME_ENV: &str = "RLX_HOME";

/// The variable that named the home before rlx 0.10.2, still honoured. Tests and the
/// 0.10.3 sandbox set it.
pub const LEGACY_HOME_ENV: &str = "AISQUARE_HOME";

/// The home's folder under the user's home directory when neither variable is set, as
/// `aisquare.core.paths.aisquare_home` makes it.
pub const DEFAULT_HOME_DIR: &str = ".aisquare";

/// The longest socket path the OS takes, in bytes: `sockaddr_un`'s `sun_path` holds 108
/// on Linux and 104 on macOS and the BSDs, its terminating NUL included.
pub const SOCKET_PATH_MAX: usize = if cfg!(any(target_os = "linux", target_os = "android")) {
    107
} else {
    103
};

/// An rlx home: the folder holding a user's rlx config and state, and one rlxd's files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Home {
    root: PathBuf,
}

/// No home could be named: neither variable is set, and the OS names no home directory
/// for the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoHome;

impl fmt::Display for NoHome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "no rlx home: set {HOME_ENV}, or give the user a home directory"
        )
    }
}

impl std::error::Error for NoHome {}

impl Home {
    /// The home this process's environment names, resolved as Python's
    /// `aisquare.core.paths.aisquare_home` resolves it, with `RLX_HOME` first:
    ///
    /// - `RLX_HOME`, when it is set and not empty;
    /// - else `AISQUARE_HOME`, likewise;
    /// - else `.aisquare` in the user's home directory (`$HOME`, or `%USERPROFILE%` on
    ///   Windows).
    ///
    /// A variable's value is taken as written, but a leading `~` is the user's home
    /// directory, as `Path.expanduser` reads it. `~name`, another user's home, is left as
    /// written. Nothing is created.
    pub fn from_env() -> Result<Self, NoHome> {
        Self::from_vars(
            env::var_os(HOME_ENV),
            env::var_os(LEGACY_HOME_ENV),
            env::home_dir(),
        )
    }

    /// [`Home::from_env`], given what the environment holds.
    fn from_vars(
        rlx_home: Option<OsString>,
        legacy_home: Option<OsString>,
        user_home: Option<PathBuf>,
    ) -> Result<Self, NoHome> {
        let user_home = user_home.filter(|dir| !dir.as_os_str().is_empty());
        let named = [rlx_home, legacy_home]
            .into_iter()
            .flatten()
            .find(|value| !value.is_empty());
        let root = match named {
            Some(value) => expand_user(PathBuf::from(value), user_home.as_deref())?,
            None => user_home.ok_or(NoHome)?.join(DEFAULT_HOME_DIR),
        };
        Ok(Self { root })
    }

    /// The home at `root`.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The home's folder.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<home>/run/`: sockets and locks (0700).
    pub fn run_dir(&self) -> PathBuf {
        self.root.join("run")
    }

    /// `<home>/rlxd/`: rlxd's own state (0700).
    pub fn rlxd_dir(&self) -> PathBuf {
        self.root.join("rlxd")
    }

    /// Where clients connect on macOS and Linux: [`Home::socket_in_home`] when its path
    /// fits the OS's limit, else [`Home::short_socket`] ([`Home::uses_short_socket`]).
    pub fn socket(&self) -> PathBuf {
        #[cfg(unix)]
        if self.uses_short_socket() {
            return self.short_socket();
        }
        self.socket_in_home()
    }

    /// `<home>/run/rlxd.sock`: the socket's place in the home, which [`Home::socket`] is
    /// unless its path is too long.
    ///
    /// On macOS and Linux it is spelled as every spelling of the home resolves it, so
    /// rlxd binds and every client connects to one path, and they measure the same length:
    /// under the canonical path of `run/` (links, `.` and `..` resolved), else of the home
    /// while `run/` isn't there, else the path made absolute while neither is. On Windows,
    /// as written.
    pub fn socket_in_home(&self) -> PathBuf {
        let run = self.run_dir();
        if cfg!(windows) {
            return run.join("rlxd.sock");
        }
        let dir = std::fs::canonicalize(&run)
            .or_else(|_| std::fs::canonicalize(&self.root).map(|root| root.join("run")))
            .unwrap_or_else(|_| std::path::absolute(&run).unwrap_or(run));
        dir.join("rlxd.sock")
    }

    /// Whether the socket in the home is too long for the OS, so clients connect to
    /// [`Home::short_socket`]: [`Home::socket_in_home`] is longer than
    /// [`SOCKET_PATH_MAX`] bytes. It is resolved first, so every spelling of a home
    /// decides alike. Never on Windows, where clients connect to a named pipe.
    pub fn uses_short_socket(&self) -> bool {
        !cfg!(windows) && self.socket_in_home().as_os_str().len() > SOCKET_PATH_MAX
    }

    /// `/tmp/rlxd-<uid>/<hash>.sock`: where clients connect when the home's own socket
    /// path is too long ([`Home::uses_short_socket`]). The hash is the first 16 hex digits
    /// of the SHA-256 of the home's canonical path, its bytes as they are, so two
    /// spellings of one home share it and two homes never do, UTF-8 or not. About 40
    /// bytes, well inside every OS's limit.
    #[cfg(unix)]
    pub fn short_socket(&self) -> PathBuf {
        use std::os::unix::ffi::OsStrExt;
        let path = canonical(&self.root);
        let name = hex16(&Sha256::digest(path.as_os_str().as_bytes()));
        Self::short_socket_dir().join(format!("{name}.sock"))
    }

    /// `/tmp/rlxd-<uid>/`: the short sockets' folder, one per user, 0700. It is `/tmp`
    /// and not `$TMPDIR` or `$XDG_RUNTIME_DIR` because every process of the user must
    /// name the same folder, whatever its environment holds: a desktop session, a
    /// non-login SSH session and `env -i` alike. And logind removes
    /// `$XDG_RUNTIME_DIR` at the user's last logout, which an rlxd started over SSH
    /// outlives.
    #[cfg(unix)]
    pub fn short_socket_dir() -> PathBuf {
        PathBuf::from(format!("/tmp/rlxd-{}", crate::unix::uid()))
    }

    /// `\\.\pipe\rlxd-<hash>`: where clients connect on Windows.
    ///
    /// The hash is the first 16 hex digits of the SHA-256 of the home's path, as UTF-8.
    /// The path is the home's canonical one when it exists, without Windows' `\\?\`
    /// prefix (Python's `str(Path(home).resolve())`), so two spellings of one home name
    /// one pipe. A home that does not exist is hashed as written: no rlxd serves it.
    pub fn pipe_name(&self) -> String {
        format!(r"\\.\pipe\rlxd-{}", self.hash())
    }

    /// The first 16 hex digits of the SHA-256 of the home's canonical path, as UTF-8, as
    /// Python hashes it: the pipe's name carries it.
    fn hash(&self) -> String {
        let path = canonical(&self.root);
        hex16(&Sha256::digest(path.to_string_lossy().as_bytes()))
    }

    /// Where clients connect on this OS, as `status` reports it: the socket's path, or
    /// the pipe's name on Windows.
    pub fn endpoint(&self) -> String {
        if cfg!(windows) {
            self.pipe_name()
        } else {
            self.socket().display().to_string()
        }
    }

    /// `<home>/run/rlxd.lock`: held while rlxd runs.
    pub fn lock(&self) -> PathBuf {
        self.run_dir().join("rlxd.lock")
    }

    /// `<home>/rlxd/token`: the shared client token (0600).
    pub fn token(&self) -> PathBuf {
        self.rlxd_dir().join("token")
    }

    /// `<home>/rlxd/gate.json`: the devices paired as the app gate, with the SHA-256 of
    /// each one's gate credential, never the credential itself (0600, handoff 0.10.3, 1.3
    /// and 5.1).
    pub fn gate(&self) -> PathBuf {
        self.rlxd_dir().join("gate.json")
    }

    /// `<home>/rlxd/config.toml`: rlxd's config. It is optional, and read when rlxd starts.
    pub fn config(&self) -> PathBuf {
        self.rlxd_dir().join("config.toml")
    }

    /// `<home>/rlxd/logs/`.
    pub fn logs(&self) -> PathBuf {
        self.rlxd_dir().join("logs")
    }

    /// `<home>/rlxd/logs/rlxd.log`: the current log. Older ones are `rlxd.log.1` and up.
    pub fn log_file(&self) -> PathBuf {
        self.logs().join("rlxd.log")
    }

    /// `<home>/daemons/`: the registry of non-UI rlx processes (handoff 0.10.3, 4.1).
    pub fn daemons(&self) -> PathBuf {
        self.root.join("daemons")
    }

    /// `<home>/test-gate-credential`: where a test client keeps the gate credential its
    /// first `--as-app` call paired (handoff 0.10.3, "How to read the checks"). Only
    /// tests use it.
    pub fn test_gate_credential(&self) -> PathBuf {
        self.root.join("test-gate-credential")
    }
}

/// The first 8 bytes of `digest`, as 16 hex digits.
fn hex16(digest: &[u8]) -> String {
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// `path` with a leading `~` replaced by the user's home directory. `~name` and every
/// other path are left as written.
fn expand_user(path: PathBuf, user_home: Option<&Path>) -> Result<PathBuf, NoHome> {
    let mut components = path.components();
    match components.next() {
        Some(Component::Normal(first)) if first == "~" => {
            Ok(user_home.ok_or(NoHome)?.join(components.as_path()))
        }
        _ => Ok(path),
    }
}

/// `path`'s canonical form when it exists, without Windows' verbatim prefix: `\\?\C:\x`
/// is `C:\x`, and `\\?\UNC\server\share` is `\\server\share`. As written otherwise.
fn canonical(path: &Path) -> PathBuf {
    let Ok(resolved) = std::fs::canonicalize(path) else {
        return path.to_path_buf();
    };
    match without_verbatim_prefix(&resolved.to_string_lossy()) {
        Some(plain) => PathBuf::from(plain),
        None => resolved,
    }
}

/// `text` without Windows' verbatim prefix, or `None` when it has none.
fn without_verbatim_prefix(text: &str) -> Option<String> {
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        Some(format!(r"\\{unc}"))
    } else {
        text.strip_prefix(r"\\?\").map(str::to_owned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(value: &str) -> Option<OsString> {
        Some(OsString::from(value))
    }

    #[test]
    fn rlx_home_wins_then_aisquare_home_then_the_default() {
        let user = Some(PathBuf::from("/u"));
        let both = Home::from_vars(os("/a"), os("/b"), user.clone()).unwrap();
        assert_eq!(both.root(), Path::new("/a"));
        let legacy = Home::from_vars(None, os("/b"), user.clone()).unwrap();
        assert_eq!(legacy.root(), Path::new("/b"));
        let default = Home::from_vars(None, None, user.clone()).unwrap();
        assert_eq!(default.root(), Path::new("/u").join(".aisquare"));
    }

    #[test]
    fn an_empty_variable_counts_as_unset() {
        let user = Some(PathBuf::from("/u"));
        let home = Home::from_vars(os(""), os("/b"), user.clone()).unwrap();
        assert_eq!(home.root(), Path::new("/b"));
        let home = Home::from_vars(os(""), os(""), user).unwrap();
        assert_eq!(home.root(), Path::new("/u").join(".aisquare"));
    }

    #[test]
    fn a_leading_tilde_is_the_users_home_and_nothing_else_is_expanded() {
        let user = Some(PathBuf::from("/u"));
        let home = Home::from_vars(os("~/h"), None, user.clone()).unwrap();
        assert_eq!(home.root(), Path::new("/u").join("h"));
        let home = Home::from_vars(os("~"), None, user.clone()).unwrap();
        assert_eq!(home.root(), Path::new("/u"));
        for verbatim in ["~other/h", "rel/h", "/x/~/h"] {
            let home = Home::from_vars(os(verbatim), None, user.clone()).unwrap();
            assert_eq!(home.root(), Path::new(verbatim));
        }
    }

    #[test]
    fn with_no_users_home_only_a_named_home_resolves() {
        assert_eq!(Home::from_vars(None, None, None), Err(NoHome));
        assert_eq!(Home::from_vars(os("~/h"), None, None), Err(NoHome));
        assert_eq!(
            Home::from_vars(None, None, Some(PathBuf::new())),
            Err(NoHome)
        );
        let home = Home::from_vars(None, os("/b"), None).unwrap();
        assert_eq!(home.root(), Path::new("/b"));
    }

    #[test]
    fn every_path_sits_where_the_layout_says() {
        let home = Home::at("/h");
        let at = |parts: &[&str]| parts.iter().fold(PathBuf::from("/h"), |p, x| p.join(x));
        assert_eq!(home.socket(), at(&["run", "rlxd.sock"]));
        assert_eq!(home.socket_in_home(), at(&["run", "rlxd.sock"]));
        assert_eq!(home.lock(), at(&["run", "rlxd.lock"]));
        assert_eq!(home.token(), at(&["rlxd", "token"]));
        assert_eq!(home.gate(), at(&["rlxd", "gate.json"]));
        assert_eq!(home.config(), at(&["rlxd", "config.toml"]));
        assert_eq!(home.logs(), at(&["rlxd", "logs"]));
        assert_eq!(home.log_file(), at(&["rlxd", "logs", "rlxd.log"]));
        assert_eq!(home.daemons(), at(&["daemons"]));
        assert_eq!(home.test_gate_credential(), at(&["test-gate-credential"]));
    }

    /// A home whose `run/rlxd.sock` is `len` bytes long.
    fn home_whose_socket_is(len: usize) -> Home {
        let suffix = "/run/rlxd.sock".len();
        Home::at(format!("/{}", "h".repeat(len - 1 - suffix)))
    }

    #[cfg(unix)]
    #[test]
    fn the_socket_moves_to_tmp_only_when_the_homes_is_too_long() {
        let fits = home_whose_socket_is(SOCKET_PATH_MAX);
        assert_eq!(fits.socket_in_home().as_os_str().len(), SOCKET_PATH_MAX);
        assert!(!fits.uses_short_socket());
        assert_eq!(fits.socket(), fits.socket_in_home());
        assert_eq!(fits.endpoint(), fits.socket_in_home().display().to_string());

        let over = home_whose_socket_is(SOCKET_PATH_MAX + 1);
        assert!(over.uses_short_socket());
        let short = over.socket();
        assert_eq!(short, over.short_socket());
        assert_eq!(over.endpoint(), short.display().to_string());
        let dir = PathBuf::from(format!("/tmp/rlxd-{}", crate::unix::uid()));
        assert_eq!(Home::short_socket_dir(), dir);
        assert_eq!(short.parent(), Some(dir.as_path()));
        let hash = &over.pipe_name()[r"\\.\pipe\rlxd-".len()..];
        assert_eq!(short.file_name().unwrap(), format!("{hash}.sock").as_str());
        assert!(
            short.as_os_str().len() <= SOCKET_PATH_MAX,
            "{}",
            short.display()
        );
        let other = home_whose_socket_is(SOCKET_PATH_MAX + 2);
        assert_ne!(other.socket(), short, "one short socket per home");
    }

    #[cfg(unix)]
    #[test]
    fn two_spellings_of_one_deep_home_share_its_short_socket() {
        let dir = env::temp_dir().join(format!("rlxd-home-{}", std::process::id()));
        let deep = dir.join("d".repeat(SOCKET_PATH_MAX));
        std::fs::create_dir_all(&deep).unwrap();
        let home = Home::at(&deep);
        let spelled = Home::at(deep.join("."));
        assert!(home.uses_short_socket() && spelled.uses_short_socket());
        assert_eq!(home.socket(), spelled.socket());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn every_spelling_of_a_home_decides_and_names_its_socket_alike() {
        use std::os::unix::fs::symlink;
        let base = std::fs::canonicalize(env::temp_dir())
            .unwrap()
            .join(format!("rlxd-spell-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        // A home whose own socket fits, reached through a link too long for it: both
        // spellings name the resolved run/rlxd.sock.
        let near = base.join("h");
        std::fs::create_dir_all(near.join("run")).unwrap();
        let long_link = base.join("l".repeat(SOCKET_PATH_MAX));
        symlink(&near, &long_link).unwrap();
        let spelled = long_link.join("run").join("rlxd.sock");
        assert!(spelled.as_os_str().len() > SOCKET_PATH_MAX);
        for home in [
            Home::at(&near),
            Home::at(&long_link),
            Home::at(near.join(".")),
        ] {
            assert!(!home.uses_short_socket(), "{:?}", home.root());
            assert_eq!(home.socket(), near.join("run").join("rlxd.sock"));
        }
        // A home too deep for its socket, reached through a short link: both spellings
        // name one short socket.
        let deep = base.join("d".repeat(SOCKET_PATH_MAX));
        std::fs::create_dir_all(&deep).unwrap();
        let short_link = base.join("s");
        symlink(&deep, &short_link).unwrap();
        assert!(short_link.join("run/rlxd.sock").as_os_str().len() <= SOCKET_PATH_MAX);
        let (real, linked) = (Home::at(&deep), Home::at(&short_link));
        assert!(real.uses_short_socket() && linked.uses_short_socket());
        assert_eq!(real.socket(), linked.socket());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_short_socket_hashes_the_paths_bytes_as_they_are() {
        use std::os::unix::ffi::OsStrExt;
        // Two paths that differ only in bytes that aren't UTF-8 read alike lossily.
        let a = Home::at(std::ffi::OsStr::from_bytes(b"/x/\xff"));
        let b = Home::at(std::ffi::OsStr::from_bytes(b"/x/\xfe"));
        assert_eq!(
            a.pipe_name(),
            b.pipe_name(),
            "the pipe's name is the UTF-8 form's"
        );
        assert_ne!(a.short_socket(), b.short_socket());
    }

    #[cfg(windows)]
    #[test]
    fn windows_never_uses_a_short_socket() {
        let deep = home_whose_socket_is(SOCKET_PATH_MAX + 50);
        assert!(!deep.uses_short_socket());
        assert_eq!(deep.socket(), deep.socket_in_home());
    }

    #[test]
    fn a_pipe_name_is_per_home_and_the_same_for_two_spellings_of_one() {
        let dir = env::temp_dir();
        let a = Home::at(&dir).pipe_name();
        assert!(a.starts_with(r"\\.\pipe\rlxd-"), "{a}");
        assert_eq!(a.len(), r"\\.\pipe\rlxd-".len() + 16, "{a}");
        assert_eq!(Home::at(dir.join(".")).pipe_name(), a);
        assert_ne!(Home::at(dir.join("another")).pipe_name(), a);
    }

    #[test]
    fn a_verbatim_prefix_is_not_part_of_the_hashed_path() {
        // What canonicalize returns on Windows, as Python's resolve() writes it.
        let plain = |text| without_verbatim_prefix(text);
        assert_eq!(
            plain(r"\\?\C:\Users\u\.aisquare").unwrap(),
            r"C:\Users\u\.aisquare"
        );
        assert_eq!(plain(r"\\?\UNC\srv\share\h").unwrap(), r"\\srv\share\h");
        assert_eq!(plain(r"C:\Users\u"), None);
        assert_eq!(plain("/tmp/h"), None);
    }
}
