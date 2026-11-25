//! The event envelope (handoff 0.10.3, 7.3).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Data, SCHEMA_VERSION};

/// One event on the stream `events.subscribe` opens.
///
/// ```json
/// {"schema_version": 1, "seq": 42, "time": "2026-11-25T12:00:00.000Z", "topic": "run", "type": "run.text", "data": {}}
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "Event")]
pub struct Event {
    /// The schema version this document follows.
    #[schemars(extend("const" = 1))]
    pub schema_version: u32,
    /// Strictly increasing per home, from 1. `events.subscribe {since}` resumes after it.
    #[schemars(range(min = 1))]
    pub seq: u64,
    /// When it happened: RFC 3339, in UTC, to the millisecond (`2026-11-25T12:00:00.000Z`).
    #[schemars(extend("format" = "date-time"))]
    pub time: String,
    /// What it is about, such as `run` or `daemon`. A client ignores topics it does not know.
    #[schemars(length(min = 1))]
    pub topic: String,
    /// What happened, named under its topic: `run.text`, `daemon.gap`,
    /// `fleet.agent.spawned`. A client ignores types it does not know.
    #[schemars(length(min = 1))]
    pub r#type: String,
    /// The type's own fields.
    pub data: Data,
}

/// The topics of handoff 0.10.3's 7.3. A client must also accept topics not listed here.
pub mod topics {
    /// The daemon itself: `started`, `stopping`, `gap`.
    pub const DAEMON: &str = "daemon";
    /// Runs (7.5).
    pub const RUN: &str = "run";
    /// Scheduler jobs (7.6).
    pub const JOB: &str = "job";
    /// Connectors (7.7).
    pub const CONNECTOR: &str = "connector";
    /// PTYs: `output`, `exited`.
    pub const PTY: &str = "pty";
    /// Claude Code hooks: `session_start`, `prompt_submit`, `session_end`, `pre_tool_use`.
    pub const HOOK: &str = "hook";
    /// Fleet agents: `agent.spawned`, `agent.state`, `agent.ended`.
    pub const FLEET: &str = "fleet";
    /// The board: `task.changed`, `note.added`.
    pub const BOARD: &str = "board";
    /// Something needs the user, for runs and fleet agents: `raised`, `cleared`.
    pub const NEEDS_YOU: &str = "needs_you";
    /// The upgrade: `started`, `done`.
    pub const UPGRADE: &str = "upgrade";

    /// Every topic above.
    pub const ALL: &[&str] = &[
        DAEMON, RUN, JOB, CONNECTOR, PTY, HOOK, FLEET, BOARD, NEEDS_YOU, UPGRADE,
    ];
}

impl Event {
    /// An event of this schema version.
    pub fn new(
        seq: u64,
        time: impl Into<String>,
        topic: impl Into<String>,
        r#type: impl Into<String>,
        data: Data,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            seq,
            time: time.into(),
            topic: topic.into(),
            r#type: r#type.into(),
            data,
        }
    }
}
