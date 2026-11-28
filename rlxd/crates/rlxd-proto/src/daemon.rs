//! The daemon record: one non-UI rlx process in `<home>/daemons/<id>.json`, shared by
//! Python's registry (`core/daemons.py`) and rlxd (handoff 0.10.3, 4.1).

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::check::{self, Invalid};

/// One record in `<home>/daemons/`. Python's registry and rlxd both write and read
/// these. A record is live while its `<id>.lock` is held. A reader keeps the fields and
/// kinds it does not know.
///
/// ```json
/// {"id": "0123456789ab", "kind": "watch", "pid": 4242, "host": "mac.local",
///  "version": "0.10.1", "started_at": "2026-11-25T18:00:00.123456+00:00",
///  "heartbeat_at": "2026-11-25T18:00:05.123456+00:00", "argv": ["rlx", "watch"],
///  "restartable": true, "cwd": "/Users/me/project"}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(title = "DaemonRecord")]
#[non_exhaustive]
pub struct DaemonRecord {
    /// 12 lowercase hex characters, and the file's name: `<id>.json`.
    #[schemars(regex(pattern = r"^[0-9a-f]{12}$"))]
    pub id: String,
    /// What the process is, such as `rlxd` or `watch`. A reader keeps kinds it does not know.
    #[schemars(length(min = 1))]
    pub kind: String,
    /// Its process id, from 1 to 2147483647 (2^31 - 1), as Python's
    /// `core.registry.PID_MAX` bounds it: `os.kill` takes a C `int`, so a reader takes a
    /// record with a larger pid for no pid at all.
    #[schemars(range(min = 1, max = 2147483647))]
    pub pid: u32,
    /// The host it runs on.
    pub host: String,
    /// The rlx version it runs.
    pub version: String,
    /// When it started: RFC 3339 with an offset. Python writes `+00:00`; `Z` reads the same.
    #[schemars(extend("format" = "date-time"))]
    pub started_at: String,
    /// Its last heartbeat, every 5 s: RFC 3339 with an offset.
    #[schemars(extend("format" = "date-time"))]
    pub heartbeat_at: String,
    /// The command line that started it, and would restart it.
    #[schemars(length(min = 1))]
    pub argv: Vec<String>,
    /// Whether the upgrade may stop it and start it again from `argv`.
    pub restartable: bool,
    /// The directory it runs in, when known. Absent and `null` say the same.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// The serde code of `DaemonRecord`'s fields, unchecked. It is private, so the only way in or out
/// of the type is its own `Serialize` and `Deserialize`, which run `validate()`.
/// (`remote = "Self"` would make these unchecked functions public, as
/// `DaemonRecord::serialize` and `DaemonRecord::deserialize`.)
#[derive(Serialize, Deserialize)]
#[serde(remote = "DaemonRecord")]
struct UncheckedRecord {
    id: String,
    kind: String,
    pid: u32,
    host: String,
    version: String,
    started_at: String,
    heartbeat_at: String,
    argv: Vec<String>,
    restartable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
}

/// The kinds of handoff 0.10.3's 4.1. A reader must also keep kinds not listed here.
pub mod kinds {
    /// rlxd itself.
    pub const RLXD: &str = "rlxd";
    /// The resident Python brain (1.6).
    pub const BRAIN: &str = "brain";
    /// `rlx watch --detach`.
    pub const WATCH: &str = "watch";
    /// `rlx serve`.
    pub const SERVE: &str = "serve";
    /// rlx's Remote server.
    pub const REMOTE: &str = "remote";
    /// `rlx board -w`.
    pub const BOARD_WATCH: &str = "board_watch";

    /// Every kind above.
    pub const ALL: &[&str] = &[RLXD, BRAIN, WATCH, SERVE, REMOTE, BOARD_WATCH];
}

impl DaemonRecord {
    /// The largest pid a record may say: [`PID_MAX`](crate::PID_MAX), which is Python's
    /// `core.registry.PID_MAX`.
    pub const PID_MAX: u32 = crate::PID_MAX;

    /// A record with no `cwd`. Refuses what `daemon-record.json` refuses: an `id` that is
    /// not 12 lowercase hex characters, an empty kind, a pid outside 1 to
    /// [`PID_MAX`](Self::PID_MAX), times that are not RFC 3339, and an empty `argv`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        kind: impl Into<String>,
        pid: u32,
        host: impl Into<String>,
        version: impl Into<String>,
        started_at: impl Into<String>,
        heartbeat_at: impl Into<String>,
        argv: Vec<String>,
        restartable: bool,
    ) -> Result<Self, Invalid> {
        let record = Self {
            id: id.into(),
            kind: kind.into(),
            pid,
            host: host.into(),
            version: version.into(),
            started_at: started_at.into(),
            heartbeat_at: heartbeat_at.into(),
            argv,
            restartable,
            cwd: None,
        };
        record.validate()?;
        Ok(record)
    }

    /// This record, run from `cwd`.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }
}

impl DaemonRecord {
    /// Whether this record keeps every rule of `daemon-record.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        let hex = |c: u8| c.is_ascii_digit() || (b'a'..=b'f').contains(&c);
        if self.id.len() != 12 || !self.id.bytes().all(hex) {
            return Err(Invalid::new(
                "id",
                format!("{:?} is not 12 lowercase hex characters", self.id),
            ));
        }
        check::non_empty("kind", &self.kind)?;
        check::pid("pid", self.pid)?;
        check::date_time("started_at", &self.started_at)?;
        check::date_time("heartbeat_at", &self.heartbeat_at)?;
        if self.argv.is_empty() {
            return Err(Invalid::new("argv", "must not be empty"));
        }
        Ok(())
    }
}

impl Serialize for DaemonRecord {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(serde::ser::Error::custom)?;
        UncheckedRecord::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for DaemonRecord {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let record = UncheckedRecord::deserialize(deserializer)?;
        record.validate().map_err(de::Error::custom)?;
        Ok(record)
    }
}
