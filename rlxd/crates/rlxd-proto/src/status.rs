//! `status`'s reply: what this rlxd is, who is connected, and whether Claude Code is
//! installed and signed in (handoff 0.10.3 rev 6, 7.4 and 1.3).

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::check::{self, Invalid, drop_null, present};
use crate::{ErrorBody, PROTOCOL_VERSION};

/// `status`'s reply data.
///
/// ```json
/// {"version": "0.10.3", "pid": 4242, "platform": "macos", "protocol_version": 1,
///  "capabilities": [], "seq": 0, "clients": {"app": 0, "tui": 0, "remote": 0, "cli": 1},
///  "gate": "signed app only", "brain": {"pid": 4243, "restarts": 0, "handled": 12, "queued": 0},
///  "claude": {"installed": true, "version": "2.1.314", "signed_in": true,
///             "auth_method": "claude.ai", "account": "…", "plan": "pro"},
///  "runs": {"live": 0, "queued": 0}, "jobs": {"count": 0},
///  "paths": {"home": "…", "socket": "…", "lock": "…", "token": "…", "logs": "…"}}
/// ```
///
/// `runs` and `jobs` hold zeros until the runner and the scheduler fill them, and `seq` is
/// 0 until the event bus does.
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(title = "StatusData")]
#[non_exhaustive]
pub struct StatusData {
    /// rlxd's version, which is rlx's.
    #[schemars(length(min = 1))]
    pub version: String,
    /// rlxd's process id, from 1 to 2147483647 (2^31 - 1), so a client's `kill` of it, a
    /// C `int`, can never be `kill(-1)`.
    #[schemars(range(min = 1, max = 2147483647))]
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
    /// The development-only cargo features this rlxd was built with, by name, such as
    /// the one that lets it run in gate dev mode (5.1): a build for development and CI.
    /// Absent from a release build, which has none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(inner(length(min = 1)))]
    pub dev_features: Vec<String>,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    dev_features: Vec<String>,
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

/// The resident Python brain, `rlx brain --stdio`, which rlxd starts and keeps running
/// (1.6).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct Brain {
    /// Its process id, while one runs and speaks rlxd's protocol: from 1 to 2147483647
    /// (2^31 - 1), as rlxd's. Absent while it starts, waits to start again, or is missing.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(transform = drop_null, range(min = 1, max = 2147483647))]
    pub pid: Option<u32>,
    /// How many times rlxd started it again after it ended: 0 for the first brain.
    pub restarts: u64,
    /// How many requests it answered.
    pub handled: u64,
    /// How many requests wait for it now, in its queue: at most 64, after which a request
    /// is refused at once. Absent from a status before the queue had a bound reads as 0.
    #[serde(default)]
    pub queued: u64,
    /// Why no brain runs now, or why the last one ended: `brain_not_found`,
    /// `brain_start_failed`, `brain_incompatible` (rlx and rlxd are of releases that share
    /// no brain protocol), `brain_exited` or `brain_unresponsive`. Absent once a brain
    /// runs. A client ignores codes it does not know.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(transform = drop_null)]
    pub error: Option<ErrorBody>,
    /// While rlxd waits to start it again: how long that wait is, in milliseconds. It
    /// doubles with each start that fails, up to 30 s, so a brain that keeps dying does
    /// not spin.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(transform = drop_null)]
    pub backoff_ms: Option<u64>,
}

/// The Claude Code rlxd runs, and its sign-in, as `claude --version` and
/// `claude auth status --json` report them. rlxd reads no credential store itself.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct Claude {
    /// Whether rlxd found the program: `RLXD_CLAUDE_BIN` when it is set, else `claude`
    /// on the app's `PATH` (4.3), else on rlxd's own.
    pub installed: bool,
    /// The program rlxd starts as Claude Code, an absolute path as it was found, links
    /// kept: resolved when the app sets `PATH` and when rlxd starts, never at a run's
    /// start (4.3). Absent when it is not installed.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(transform = drop_null, length(min = 1))]
    pub path: Option<String>,
    /// Where rlxd looked for it: `app`, the app's `PATH` (4.3); `rlxd`, rlxd's own `PATH`,
    /// as the app set none; or `rlxd_claude_bin`, `RLXD_CLAUDE_BIN` in rlxd's own
    /// environment, which tests set ([`Claude::PATH_SOURCES`]). Set whether or not it was
    /// found, so the app can tell when its `PATH` was not the one searched.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(transform = drop_null, length(min = 1))]
    pub path_source: Option<String>,
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
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct Jobs {
    /// Jobs defined.
    pub count: u64,
    /// The scheduler's database, `<home>/rlxd/rlxd.db`. Absent: `ok`. `set_aside`: rlxd
    /// found one it could not read (corrupt, or from a newer rlxd), moved it aside
    /// (`set_aside` says where) and began a fresh one. `unavailable`: it could not be
    /// opened for another reason (busy, locked, no access), so the scheduler isn't running
    /// and the file is as it was (`reason` says why).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = crate::check::drop_null)]
    pub db: Option<String>,
    /// For `set_aside`: where the database it could not read was moved, with why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = crate::check::drop_null)]
    pub set_aside: Option<String>,
    /// For `unavailable`: why the database could not be opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = crate::check::drop_null)]
    pub reason: Option<String>,
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

impl Claude {
    /// `path_source` when rlxd looked on the app's `PATH`.
    pub const PATH_SOURCE_APP: &str = "app";
    /// `path_source` when the app set no `PATH`, and rlxd looked on its own.
    pub const PATH_SOURCE_RLXD: &str = "rlxd";
    /// `path_source` when `RLXD_CLAUDE_BIN` named the program.
    pub const PATH_SOURCE_RLXD_CLAUDE_BIN: &str = "rlxd_claude_bin";
    /// Every `path_source`.
    pub const PATH_SOURCES: &[&str] = &[
        Self::PATH_SOURCE_APP,
        Self::PATH_SOURCE_RLXD,
        Self::PATH_SOURCE_RLXD_CLAUDE_BIN,
    ];
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
    /// only. Refuses an empty version or platform, and a pid outside 1 to
    /// [`PID_MAX`](crate::PID_MAX).
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
            dev_features: Vec::new(),
        };
        status.validate()?;
        Ok(status)
    }

    /// Whether this status keeps every rule of `status-reply.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        check::non_empty("version", &self.version)?;
        check::pid("pid", self.pid)?;
        if let Some(pid) = self.brain.pid {
            check::pid("brain.pid", pid)?;
        }
        check::non_empty("platform", &self.platform)?;
        check::at_least("protocol_version", u64::from(self.protocol_version), 1)?;
        check::non_empty("gate", &self.gate)?;
        if let Some(acl) = &self.pipe_acl {
            check::non_empty("pipe_acl", acl)?;
        }
        for feature in &self.dev_features {
            check::non_empty("dev_features", feature)?;
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
