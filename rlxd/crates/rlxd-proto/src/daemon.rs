//! The daemon record: one non-UI rlx process in `<home>/daemons/<id>.json`, shared by
//! Python's registry (`core/daemons.py`) and rlxd (handoff 0.10.3, 4.1).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "DaemonRecord")]
#[non_exhaustive]
pub struct DaemonRecord {
    /// 12 lowercase hex characters, and the file's name: `<id>.json`.
    #[schemars(regex(pattern = r"^[0-9a-f]{12}$"))]
    pub id: String,
    /// What the process is, such as `rlxd` or `watch`. A reader keeps kinds it does not know.
    #[schemars(length(min = 1))]
    pub kind: String,
    /// Its process id.
    #[schemars(range(min = 1))]
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
    /// The absolute directory it runs in, when known. Absent and `null` say the same.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
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
    /// A record with no `cwd`.
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
    ) -> Self {
        Self {
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
        }
    }

    /// This record, run from `cwd`.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }
}
