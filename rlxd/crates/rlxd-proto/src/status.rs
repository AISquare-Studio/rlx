//! `status`'s reply: what this rlxd is, who is connected, and whether Claude Code is
//! installed and signed in (handoff 0.10.3 rev 6, 7.4 and 1.3).

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::PROTOCOL_VERSION;
use crate::check::{self, Invalid, drop_null, present};

/// `status`'s reply data.
///
/// ```json
/// {"version": "0.10.3", "pid": 4242, "platform": "macos", "protocol_version": 1,
///  "capabilities": [], "seq": 0, "clients": {"app": 0, "tui": 0, "remote": 0, "cli": 1},
///  "gate": "signed app only", "brain": {"restarts": 0, "handled": 0},
///  "claude": {"installed": true, "version": "2.1.314", "signed_in": true,
///             "auth_method": "claude.ai", "account": "…", "plan": "pro"},
///  "runs": {"live": 0, "queued": 0}, "jobs": {"count": 0},
///  "paths": {"home": "…", "socket": "…", "lock": "…", "token": "…", "logs": "…"}}
/// ```
///
/// `brain`, `runs` and `jobs` hold zeros until the brain, the runner and the scheduler
/// fill them, and `seq` is 0 until the event bus does.
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(title = "StatusData")]
#[non_exhaustive]
pub struct StatusData {
    /// rlxd's version, which is rlx's.
    #[schemars(length(min = 1))]
    pub version: String,
    /// rlxd's process id.
    #[schemars(range(min = 1))]
    pub pid: u32,
    /// The OS rlxd runs on: `macos`, `linux` or `windows`.
    #[schemars(length(min = 1))]
    pub platform: String,
    /// The protocol version of the connection that asked.
    #[schemars(range(min = 1))]
    pub protocol_version: u32,
    /// What this rlxd can do, by name, as `hello`'s reply lists it. A client ignores names
    /// it does not know.
    pub capabilities: Vec<String>,
    /// The `seq` of the latest event on this home's event stream, or 0 before the first.
    pub seq: u64,
    /// The connections open now, by the role their `hello` was let in as. A refused
    /// `hello` is never counted.
    pub clients: Clients,
    /// Who may be the tool-call gate (5.1): `signed app only`, or `dev mode (unsigned
    /// app accepted)` while rlxd runs in gate dev mode.
    #[schemars(length(min = 1))]
    pub gate: String,
    /// The resident Python brain (1.6).
    pub brain: Brain,
    /// The Claude Code rlxd runs: `RLXD_CLAUDE_BIN`, or `claude` on `PATH`.
    pub claude: Claude,
    /// Headless runs (7.5).
    pub runs: Runs,
    /// Scheduler jobs (7.6).
    pub jobs: Jobs,
    /// Where this rlxd keeps what it serves and writes.
    pub paths: Paths,
    /// Windows only: whom the named pipe's DACL admits. `current_user` says it holds one
    /// entry, for the user rlxd runs as, which rlxd read back from the live pipe. Absent
    /// on macOS and Linux, where the socket's mode, 0600, does that.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, length(min = 1))]
    pub pipe_acl: Option<String>,
}

/// The serde code of `StatusData`'s fields, unchecked. It is private, so the only way in or
/// out of the type is its own `Serialize` and `Deserialize`, which run `validate()`.
/// (`remote = "Self"` would make these unchecked functions public, as
/// `StatusData::serialize` and `StatusData::deserialize`.)
#[derive(Serialize, Deserialize)]
#[serde(remote = "StatusData")]
struct UncheckedStatus {
    version: String,
    pid: u32,
    platform: String,
    protocol_version: u32,
    capabilities: Vec<String>,
    seq: u64,
    clients: Clients,
    gate: String,
    brain: Brain,
    claude: Claude,
    runs: Runs,
    jobs: Jobs,
    paths: Paths,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pipe_acl: Option<String>,
}

/// The connections open now, by `hello` role.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct Clients {
    /// rlxapp, the gate: connections whose `hello {role: "app"}` passed the gate checks.
    pub app: u32,
    /// rlx's terminal UI.
    pub tui: u32,
    /// rlx's Remote server.
    pub remote: u32,
    /// Command-line clients, such as `rlxd call` and `rlxd-hook`, the one asking included.
    pub cli: u32,
}

/// The resident Python brain.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct Brain {
    /// Its process id, while one runs.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(transform = drop_null)]
    pub pid: Option<u32>,
    /// How many times rlxd started it again.
    pub restarts: u64,
    /// How many requests it handled.
    pub handled: u64,
}

/// The Claude Code rlxd runs, and its sign-in, as `claude --version` and
/// `claude auth status --json` report them. rlxd reads no credential store itself.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct Claude {
    /// Whether rlxd found the program: `RLXD_CLAUDE_BIN` when it is set, else `claude`
    /// on `PATH`.
    pub installed: bool,
    /// Its version, such as `2.1.314`, when `claude --version` said one.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(transform = drop_null)]
    pub version: Option<String>,
    /// Whether it is signed in. False when it is not installed, or would not say.
    pub signed_in: bool,
    /// How it is signed in, such as `claude.ai`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(transform = drop_null)]
    pub auth_method: Option<String>,
    /// The account it is signed in as: its email address.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(transform = drop_null)]
    pub account: Option<String>,
    /// The account's plan, such as `pro` or `max`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(transform = drop_null)]
    pub plan: Option<String>,
}

/// Headless runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct Runs {
    /// Runs going now.
    pub live: u64,
    /// Runs waiting to start.
    pub queued: u64,
}

/// Scheduler jobs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct Jobs {
    /// Jobs defined.
    pub count: u64,
}

/// Where this rlxd keeps what it serves and writes, as it resolved them from the home.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct Paths {
    /// The home: `RLX_HOME`, else `AISQUARE_HOME`, else `~/.aisquare`.
    pub home: String,
    /// Where clients connect: the socket's path on macOS and Linux, the named pipe's
    /// name on Windows.
    pub socket: String,
    /// The lock file rlxd holds while it runs, one rlxd per home.
    pub lock: String,
    /// The file holding the shared client token.
    pub token: String,
    /// The directory of rlxd's logs.
    pub logs: String,
}

impl StatusData {
    /// `gate` outside gate dev mode: only a signed rlxapp may be the gate.
    pub const GATE_SIGNED_APP_ONLY: &str = "signed app only";
    /// `gate` in gate dev mode, for unsigned dev and CI builds.
    pub const GATE_DEV_MODE: &str = "dev mode (unsigned app accepted)";
    /// `pipe_acl` when the pipe's DACL holds one entry, for the user rlxd runs as.
    pub const PIPE_ACL_CURRENT_USER: &str = "current_user";

    /// A status speaking this crate's [`PROTOCOL_VERSION`], with no capabilities, no
    /// clients, nothing running, Claude Code not found and the gate for signed apps
    /// only. Refuses an empty version or platform, and pid 0.
    pub fn new(
        version: impl Into<String>,
        pid: u32,
        platform: impl Into<String>,
        paths: Paths,
    ) -> Result<Self, Invalid> {
        let status = Self {
            version: version.into(),
            pid,
            platform: platform.into(),
            protocol_version: PROTOCOL_VERSION,
            capabilities: Vec::new(),
            seq: 0,
            clients: Clients::default(),
            gate: Self::GATE_SIGNED_APP_ONLY.to_owned(),
            brain: Brain::default(),
            claude: Claude::default(),
            runs: Runs::default(),
            jobs: Jobs::default(),
            paths,
            pipe_acl: None,
        };
        status.validate()?;
        Ok(status)
    }

    /// Whether this status keeps every rule of `status-reply.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        check::non_empty("version", &self.version)?;
        check::at_least("pid", u64::from(self.pid), 1)?;
        check::non_empty("platform", &self.platform)?;
        check::at_least("protocol_version", u64::from(self.protocol_version), 1)?;
        check::non_empty("gate", &self.gate)?;
        if let Some(acl) = &self.pipe_acl {
            check::non_empty("pipe_acl", acl)?;
        }
        Ok(())
    }
}

impl Serialize for StatusData {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(serde::ser::Error::custom)?;
        UncheckedStatus::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for StatusData {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let status = UncheckedStatus::deserialize(deserializer)?;
        status.validate().map_err(de::Error::custom)?;
        Ok(status)
    }
}

impl Paths {
    /// Where an rlxd keeps its files: the home, where clients connect, the lock, the
    /// token and the logs.
    pub fn new(
        home: impl Into<String>,
        socket: impl Into<String>,
        lock: impl Into<String>,
        token: impl Into<String>,
        logs: impl Into<String>,
    ) -> Self {
        Self {
            home: home.into(),
            socket: socket.into(),
            lock: lock.into(),
            token: token.into(),
            logs: logs.into(),
        }
    }
}
