//! `rlxd here`'s reply: what rlx is installed on this machine, whether its rlxd runs for
//! the home, which protocol versions it serves, and how a client gets in. rlx.app runs
//! it over SSH to decide how to connect (the owner's remote brief, §3).
//!
//! Its envelope is `ok` whenever the probe worked, and `rlxd here` exits 3 (not running)
//! and 4 (unsupported) with it: the one place an `ok` envelope does not mean exit 0.

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::PROTOCOL_VERSIONS;
use crate::check::{self, Invalid, drop_null, present};
use crate::marker::True;

/// `rlxd here`'s reply data.
///
/// ```json
/// {"installed": true, "running": true, "started": false, "supported": true,
///  "version": "0.10.3", "protocol_versions": [1], "pid": 4242,
///  "socket": "/home/u/.aisquare/run/rlxd.sock", "home": "/home/u/.aisquare",
///  "platform": "linux", "auth": {"token_file": "/home/u/.aisquare/rlxd/token"},
///  "rlxd": {"version": "0.10.3", "path": "/home/u/.local/bin/rlxd", "protocol_versions": [1]},
///  "rlx": {"installed": true, "version": "0.10.3", "path": "/home/u/.local/bin/rlx"}}
/// ```
///
/// When rlxd runs, `version`, `protocol_versions`, `pid` and `socket` are the running
/// rlxd's, as it reported them. When it does not, they are this rlxd's, which it serves
/// once started, and there is no `pid`.
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(title = "HereData")]
#[non_exhaustive]
pub struct HereData {
    /// Always `true`: a probe that answers is an installed rlxd. Not installed is for the
    /// caller to see: the command is missing.
    pub installed: True,
    /// Whether an rlxd runs for this home now.
    pub running: bool,
    /// Whether this probe started it (`--start`). False when it was running already.
    pub started: bool,
    /// Whether rlxd serves a protocol version the caller speaks: one of `--protocols`,
    /// else one of `rlxd.protocol_versions`. The running rlxd's versions count when it
    /// runs, else this rlxd's.
    pub supported: bool,
    /// The version of the rlxd that runs, else of this rlxd, which is rlx's version.
    /// Absent when it runs but would not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, length(min = 1))]
    pub version: Option<String>,
    /// Every protocol version rlxd serves, ascending: the running rlxd's, else what this
    /// rlxd serves once started. `hello`'s `protocol_version` must be one of them.
    #[schemars(length(min = 1), inner(range(min = 1)))]
    pub protocol_versions: Vec<u32>,
    /// The running rlxd's process id, from 1 to 2147483647 (2^31 - 1). Absent when it
    /// does not run, or when it runs but would not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, range(min = 1, max = 2147483647))]
    pub pid: Option<u32>,
    /// Where clients connect: the socket's path on macOS and Linux, the named pipe's name
    /// on Windows. The running rlxd's own, as `status` reports it, when it runs.
    #[schemars(length(min = 1))]
    pub socket: String,
    /// The home: `RLX_HOME`, else `AISQUARE_HOME`, else `~/.aisquare`.
    #[schemars(length(min = 1))]
    pub home: String,
    /// The OS, as Rust's `std::env::consts::OS` names it: `macos`, `linux` or `windows`
    /// where rlxd ships, and another name, such as `freebsd`, elsewhere.
    #[schemars(length(min = 1))]
    pub platform: String,
    /// How a client gets in.
    pub auth: Auth,
    /// This rlxd: the program that answered.
    pub rlxd: InstalledRlxd,
    /// rlx's command, found beside this rlxd or in the folders rlx installs to.
    pub rlx: InstalledRlx,
}

/// The serde code of `HereData`'s fields, unchecked. It is private, so the only way in or
/// out of the type is its own `Serialize` and `Deserialize`, which run `validate()`.
/// (`remote = "Self"` would make these unchecked functions public, as
/// `HereData::serialize` and `HereData::deserialize`.)
#[derive(Serialize, Deserialize)]
#[serde(remote = "HereData")]
struct UncheckedHere {
    installed: True,
    running: bool,
    started: bool,
    supported: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    version: Option<String>,
    protocol_versions: Vec<u32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pid: Option<u32>,
    socket: String,
    home: String,
    platform: String,
    auth: Auth,
    rlxd: InstalledRlxd,
    rlx: InstalledRlx,
}

/// How a client gets in: `hello`'s `token` is the contents of `token_file`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct Auth {
    /// The file holding the shared client token. rlxd makes it the first time it starts.
    #[schemars(length(min = 1))]
    pub token_file: String,
    /// The token itself, only when asked for with `--show-token` and the file exists. It
    /// is a secret: whoever holds it can connect. Never log it, and fetch it again for
    /// each session rather than store it (rlxd's README, "The shared token over SSH").
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(transform = drop_null, length(min = 1))]
    pub token: Option<String>,
}

/// The rlxd that answered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct InstalledRlxd {
    /// Its version, which is rlx's.
    #[schemars(length(min = 1))]
    pub version: String,
    /// Its path.
    #[schemars(length(min = 1))]
    pub path: String,
    /// Every protocol version it serves, ascending.
    #[schemars(length(min = 1), inner(range(min = 1)))]
    pub protocol_versions: Vec<u32>,
}

/// rlx's command.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct InstalledRlx {
    /// Whether it was found.
    pub installed: bool,
    /// Its version, as `rlx --version` said it. Absent when it is not installed or would
    /// not say.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(transform = drop_null, length(min = 1))]
    pub version: Option<String>,
    /// Its path, when it was found.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(transform = drop_null, length(min = 1))]
    pub path: Option<String>,
}

impl HereData {
    /// What a probe says when rlxd does not run: `rlxd`'s version and protocol versions,
    /// no pid, nothing started, and supported. Refuses what `here-reply.json` refuses.
    pub fn new(
        rlxd: InstalledRlxd,
        rlx: InstalledRlx,
        home: impl Into<String>,
        socket: impl Into<String>,
        platform: impl Into<String>,
        auth: Auth,
    ) -> Result<Self, Invalid> {
        let here = Self {
            installed: True,
            running: false,
            started: false,
            supported: true,
            version: Some(rlxd.version.clone()),
            protocol_versions: rlxd.protocol_versions.clone(),
            pid: None,
            socket: socket.into(),
            home: home.into(),
            platform: platform.into(),
            auth,
            rlxd,
            rlx,
        };
        here.validate()?;
        Ok(here)
    }

    /// Whether this reply keeps every rule of `here-reply.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        if let Some(version) = &self.version {
            check::non_empty("version", version)?;
        }
        versions("protocol_versions", &self.protocol_versions)?;
        if let Some(pid) = self.pid {
            check::pid("pid", pid)?;
        }
        check::non_empty("socket", &self.socket)?;
        check::non_empty("home", &self.home)?;
        check::non_empty("platform", &self.platform)?;
        check::non_empty("auth.token_file", &self.auth.token_file)?;
        if let Some(token) = &self.auth.token {
            check::non_empty("auth.token", token)?;
        }
        check::non_empty("rlxd.version", &self.rlxd.version)?;
        check::non_empty("rlxd.path", &self.rlxd.path)?;
        versions("rlxd.protocol_versions", &self.rlxd.protocol_versions)?;
        if let Some(version) = &self.rlx.version {
            check::non_empty("rlx.version", version)?;
        }
        if let Some(path) = &self.rlx.path {
            check::non_empty("rlx.path", path)?;
        }
        Ok(())
    }
}

/// A list of protocol versions: not empty, none of them 0.
fn versions(field: &str, versions: &[u32]) -> Result<(), Invalid> {
    if versions.is_empty() {
        return Err(Invalid::new(field, "must not be empty"));
    }
    for version in versions {
        check::at_least(field, u64::from(*version), 1)?;
    }
    Ok(())
}

impl Serialize for HereData {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(serde::ser::Error::custom)?;
        UncheckedHere::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for HereData {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let here = UncheckedHere::deserialize(deserializer)?;
        here.validate().map_err(de::Error::custom)?;
        Ok(here)
    }
}

impl Auth {
    /// Gets in with the token in `token_file`; the token itself is not shown.
    pub fn new(token_file: impl Into<String>) -> Self {
        Self {
            token_file: token_file.into(),
            token: None,
        }
    }
}

impl InstalledRlxd {
    /// The rlxd at `path`, of `version`, serving every version this crate does
    /// ([`PROTOCOL_VERSIONS`]).
    pub fn new(version: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            version: version.into(),
            path: path.into(),
            protocol_versions: PROTOCOL_VERSIONS.to_vec(),
        }
    }
}

impl InstalledRlx {
    /// rlx's command at `path`, of `version` when it said one.
    pub fn found(path: impl Into<String>, version: Option<String>) -> Self {
        Self {
            installed: true,
            version,
            path: Some(path.into()),
        }
    }

    /// rlx's command, not found.
    pub fn missing() -> Self {
        Self::default()
    }
}
