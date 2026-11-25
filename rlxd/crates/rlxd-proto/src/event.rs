//! The event envelope, and the per-run sequence that run events carry (handoff 0.10.3
//! rev 5, 7.3).

use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::{Value, json};

use crate::Data;
use crate::check::{self, Invalid};
use crate::marker::V1;

/// One event on the stream `events.subscribe` opens.
///
/// ```json
/// {"schema_version": 1, "seq": 42, "time": "2026-11-25T12:00:00.000Z", "topic": "run",
///  "type": "run.text", "data": {"run_id": "…", "run_seq": 3, "kind": "task"}}
/// ```
///
/// Two sequences number events. `seq` is per home: every event has one, and
/// `events.subscribe {since}` resumes after it. `data.run_seq` is per run: every event
/// whose topic is `run` has one, next to `data.run_id`, so a client can replay one run
/// from a known point.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(remote = "Self")]
#[schemars(title = "Event", transform = run_scope_rule)]
#[non_exhaustive]
pub struct Event {
    /// The schema version this document follows.
    pub schema_version: V1,
    /// Strictly increasing per home, from 1, across every topic. `events.subscribe
    /// {since}` resumes after it.
    #[schemars(range(min = 1))]
    pub seq: u64,
    /// When it happened: RFC 3339, in UTC, to the millisecond (`2026-11-25T12:00:00.000Z`).
    #[schemars(extend("format" = "date-time"))]
    pub time: String,
    /// What it is about, such as `run` or `daemon`. A client ignores topics it does not know.
    #[schemars(length(min = 1))]
    pub topic: String,
    /// What happened, as `<topic>.<event>` with the event in snake_case: `run.text`,
    /// `daemon.gap`, `fleet.agent_spawned`. A client ignores types it does not know.
    #[schemars(length(min = 1))]
    pub r#type: String,
    /// The type's own fields. On the `run` topic, also `run_id` and `run_seq`.
    pub data: Data,
}

/// Where a `run`-topic event sits in its run's own stream: `data.run_id` and
/// `data.run_seq`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct RunScope {
    /// The run the event belongs to.
    pub run_id: String,
    /// Strictly increasing per run, from 1. Replaying a run from `run_seq` N gives every
    /// event of that run after N, in order.
    pub run_seq: u64,
}

/// The schema's half of the per-run sequence: an event on the `run` topic must carry
/// `data.run_id` and `data.run_seq`.
fn run_scope_rule(schema: &mut Schema) {
    schema.insert(
        "if".to_owned(),
        json!({
            "properties": {"topic": {"const": topics::RUN}},
            "required": ["topic"],
        }),
    );
    schema.insert(
        "then".to_owned(),
        json!({
            "properties": {"data": {
                "properties": {
                    "run_id": {
                        "description": "The run this event belongs to.",
                        "type": "string",
                        "minLength": 1,
                    },
                    "run_seq": {
                        "description": "Strictly increasing per run, from 1.",
                        "type": "integer",
                        "minimum": 1,
                    },
                },
                "required": ["run_id", "run_seq"],
            }},
        }),
    );
}

/// The topics of handoff 0.10.3's 7.3. A client must also accept topics not listed here.
pub mod topics {
    /// The daemon itself: `daemon.started`, `daemon.stopping`, `daemon.gap`.
    pub const DAEMON: &str = "daemon";
    /// Runs (7.5). Every event here carries `data.run_id` and `data.run_seq`.
    pub const RUN: &str = "run";
    /// Scheduler jobs (7.6).
    pub const JOB: &str = "job";
    /// Connectors (7.7).
    pub const CONNECTOR: &str = "connector";
    /// PTYs: `pty.output`, `pty.exited`.
    pub const PTY: &str = "pty";
    /// Claude Code hooks, such as `hook.session_start` and `hook.pre_tool_use`.
    pub const HOOK: &str = "hook";
    /// Fleet agents: `fleet.agent_spawned`, `fleet.agent_state`, `fleet.agent_ended`.
    pub const FLEET: &str = "fleet";
    /// The board: `board.task_changed`, `board.note_added`.
    pub const BOARD: &str = "board";
    /// Something needs the user: `needs_you.raised`, `needs_you.cleared`.
    pub const NEEDS_YOU: &str = "needs_you";
    /// The upgrade: `upgrade.started`, `upgrade.done`.
    pub const UPGRADE: &str = "upgrade";

    /// Every topic above.
    pub const ALL: &[&str] = &[
        DAEMON, RUN, JOB, CONNECTOR, PTY, HOOK, FLEET, BOARD, NEEDS_YOU, UPGRADE,
    ];
}

impl Event {
    /// An event of this schema version. For the `run` topic, use [`Event::for_run`].
    /// Refuses what `event.json` refuses: `seq` 0, a `time` that is not RFC 3339, an
    /// empty topic or type, and a `run` event without `run_id` and `run_seq`.
    pub fn new(
        seq: u64,
        time: impl Into<String>,
        topic: impl Into<String>,
        r#type: impl Into<String>,
        data: Data,
    ) -> Result<Self, Invalid> {
        let event = Self {
            schema_version: V1,
            seq,
            time: time.into(),
            topic: topic.into(),
            r#type: r#type.into(),
            data,
        };
        event.validate()?;
        Ok(event)
    }

    /// An event on the `run` topic: `data` with the run's `run_id` and `run_seq` set.
    pub fn for_run(
        seq: u64,
        time: impl Into<String>,
        r#type: impl Into<String>,
        scope: RunScope,
        mut data: Data,
    ) -> Result<Self, Invalid> {
        data.insert("run_id".to_owned(), Value::String(scope.run_id));
        data.insert("run_seq".to_owned(), Value::from(scope.run_seq));
        Self::new(seq, time, topics::RUN, r#type, data)
    }

    /// The run this event belongs to and its place in that run's stream, when it is on
    /// the `run` topic.
    pub fn run_scope(&self) -> Option<RunScope> {
        if self.topic != topics::RUN {
            return None;
        }
        let run_id = self.data.get("run_id")?.as_str()?.to_owned();
        let run_seq = self.data.get("run_seq")?.as_u64()?;
        Some(RunScope { run_id, run_seq })
    }

    /// Whether this event keeps every rule of `event.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        check::at_least("seq", self.seq, 1)?;
        check::date_time("time", &self.time)?;
        check::non_empty("topic", &self.topic)?;
        check::non_empty("type", &self.r#type)?;
        if self.topic == topics::RUN {
            match self.data.get("run_id") {
                Some(Value::String(run_id)) => check::non_empty("data.run_id", run_id)?,
                _ => {
                    return Err(Invalid::new(
                        "data.run_id",
                        "a run event needs its run's id, a string (see Event::for_run)",
                    ));
                }
            }
            match self.data.get("run_seq").and_then(Value::as_u64) {
                Some(run_seq) => check::at_least("data.run_seq", run_seq, 1)?,
                None => {
                    return Err(Invalid::new(
                        "data.run_seq",
                        "a run event needs its place in the run, an integer of at least 1",
                    ));
                }
            }
        }
        Ok(())
    }
}

impl Serialize for Event {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(serde::ser::Error::custom)?;
        Event::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for Event {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let event = Event::deserialize(deserializer)?;
        event.validate().map_err(de::Error::custom)?;
        Ok(event)
    }
}

impl RunScope {
    /// Event `run_seq` of run `run_id`.
    pub fn new(run_id: impl Into<String>, run_seq: u64) -> Self {
        Self {
            run_id: run_id.into(),
            run_seq,
        }
    }
}
