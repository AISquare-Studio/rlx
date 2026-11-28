//! The event envelope, and the run fields that run events carry (handoff 0.10.3 rev 5,
//! 7.3 and 7.5).

use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::{Value, json};

use crate::Data;
use crate::check::{self, Invalid, SNAKE};
use crate::marker::V1;

/// One event on the stream `events.subscribe` opens.
///
/// ```json
/// {"schema_version": 1, "seq": 42, "time": "2026-11-25T12:00:00.000Z", "topic": "run",
///  "type": "run.text", "data": {"run_id": "…", "run_seq": 3, "kind": "task", "attempt": 1}}
/// ```
///
/// Two sequences number events. `seq` is per home: every event has one, and
/// `events.subscribe {since}` resumes after it. `data.run_seq` is per run: every event
/// whose topic is `run` has one, with the run's `run_id`, `kind`, `attempt` and
/// `job_id?`, so a client can replay one run from a known point.
#[derive(Clone, Debug, PartialEq, JsonSchema)]
#[schemars(title = "Event", transform = event_rules)]
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
    /// What it is about, in snake_case, such as `run` or `needs_you`. A client ignores
    /// topics it does not know.
    #[schemars(regex(pattern = r"^[a-z][a-z0-9_]*$"))]
    pub topic: String,
    /// What happened: `<topic>.<event>`, the event in snake_case, such as `run.text`,
    /// `fleet.agent_spawned` or `connector.login.open_url`. It starts with this event's
    /// own topic. A client ignores types it does not know.
    #[schemars(regex(pattern = r"^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)+$"))]
    pub r#type: String,
    /// The type's own fields. On the `run` topic, also `run_id`, `run_seq`, `kind`,
    /// `attempt` and `job_id?`.
    pub data: Data,
}

/// The serde code of `Event`'s fields, unchecked. It is private, so the only way in or out
/// of the type is its own `Serialize` and `Deserialize`, which run `validate()`.
/// (`remote = "Self"` would make these unchecked functions public, as
/// `Event::serialize` and `Event::deserialize`.)
#[derive(Serialize, Deserialize)]
#[serde(remote = "Event")]
struct UncheckedEvent {
    schema_version: V1,
    seq: u64,
    time: String,
    topic: String,
    r#type: String,
    data: Data,
}

/// The run fields every `run`-topic event carries in its `data` (7.5): which run, its
/// place in that run's own stream, and which attempt of it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct RunScope {
    /// The run the event belongs to.
    pub run_id: String,
    /// Strictly increasing per run, from 1. Replaying a run from `run_seq` N gives every
    /// event of that run after N, in order.
    pub run_seq: u64,
    /// The run's kind, such as `task` or `widget_refresh`.
    pub kind: String,
    /// Which attempt of the run, from 1. `run.resume` starts the next one.
    pub attempt: u64,
    /// The scheduler job that fired the run, if one did.
    pub job_id: Option<String>,
}

/// The schema's half of the event rules:
/// - on each topic this version lists (7.3's, and 7.8's CLI ones), `type` starts with
///   that topic;
/// - an event on the `run` topic carries `run_id`, `run_seq`, `kind` and `attempt`, and
///   may carry `job_id`.
fn event_rules(schema: &mut Schema) {
    let on =
        |topic: &str| json!({"properties": {"topic": {"const": topic}}, "required": ["topic"]});
    let mut rules: Vec<Value> = topics::ALL
        .iter()
        .chain(topics::CLI)
        .map(|topic| {
            json!({
                "if": on(topic),
                "then": {"properties": {"type": {
                    "pattern": format!("^{topic}\\.{SNAKE}(\\.{SNAKE})*$"),
                }}},
            })
        })
        .collect();
    rules.push(json!({
        "if": on(topics::RUN),
        "then": {"properties": {"data": {
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
                    "maximum": u64::MAX,
                },
                "kind": {
                    "description": "The run's kind, such as task or widget_refresh.",
                    "type": "string",
                    "minLength": 1,
                },
                "attempt": {
                    "description": "Which attempt of the run, from 1.",
                    "type": "integer",
                    "minimum": 1,
                    "maximum": u64::MAX,
                },
                "job_id": {
                    "description": "The scheduler job that fired the run, if one did.",
                    "type": "string",
                    "minLength": 1,
                },
            },
            "required": ["run_id", "run_seq", "kind", "attempt"],
        }}},
    }));
    schema.insert("allOf".to_owned(), Value::Array(rules));
}

/// The topics of handoff 0.10.3's 7.3. A client must also accept topics not listed here.
pub mod topics {
    /// The daemon itself: `daemon.started`, `daemon.stopping`, `daemon.gap`.
    pub const DAEMON: &str = "daemon";
    /// Runs (7.5). Every event here carries `run_id`, `run_seq`, `kind`, `attempt` and
    /// `job_id?` in its `data`.
    pub const RUN: &str = "run";
    /// Scheduler jobs (7.6).
    pub const JOB: &str = "job";
    /// Connectors (7.7), such as `connector.changed` and `connector.login.open_url`.
    pub const CONNECTOR: &str = "connector";
    /// PTYs: `pty.output`, `pty.exited`.
    pub const PTY: &str = "pty";
    /// Claude Code hooks, such as `hook.session_start` and `hook.stop`.
    pub const HOOK: &str = "hook";
    /// Fleet agents: `fleet.agent_spawned`, `fleet.agent_state`, `fleet.agent_ended`.
    pub const FLEET: &str = "fleet";
    /// The board: `board.task_changed`, `board.note_added`.
    pub const BOARD: &str = "board";
    /// Something needs the user: `needs_you.raised`, `needs_you.cleared`.
    pub const NEEDS_YOU: &str = "needs_you";
    /// The upgrade: `upgrade.started`, `upgrade.done`.
    pub const UPGRADE: &str = "upgrade";
    /// App settings kept in the rlx home (the multi-device brief, §4): `settings.changed`.
    pub const SETTINGS: &str = "settings";

    /// Every topic above: the stream's, 7.3's.
    pub const ALL: &[&str] = &[
        DAEMON, RUN, JOB, CONNECTOR, PTY, HOOK, FLEET, BOARD, NEEDS_YOU, UPGRADE, SETTINGS,
    ];

    /// The installer's JSON lines (7.8): `install.step`, `install.done`, `install.error`.
    pub const INSTALL: &str = "install";
    /// Sign-in's JSON lines (7.8): `signin.open_url`, `signin.waiting`,
    /// `signin.signed_in`, `signin.error`.
    pub const SIGNIN: &str = "signin";

    /// The topics only the CLI's `--json-lines` prints (7.8), as event envelopes too.
    /// `upgrade`, which the stream also carries, is in [`ALL`].
    pub const CLI: &[&str] = &[INSTALL, SIGNIN];
}

/// The types rlxd itself sends on the `daemon` topic (7.3), on `settings`, on `pty` and
/// on `hook`. A client must also accept types not listed here.
pub mod types {
    /// `{version, pid}`: rlxd started. The first event of each rlxd's life.
    pub const DAEMON_STARTED: &str = "daemon.started";
    /// `{reason}`: rlxd is stopping, such as `stop` (asked to) or `sigterm`. The last event
    /// of its life, unless it was killed.
    pub const DAEMON_STOPPING: &str = "daemon.stopping";
    /// `{from, to}`: the stream skipped the events numbered `from` to `to`, which rlxd no
    /// longer holds. The client re-reads what it shows. It can come first or anywhere
    /// later in a stream, whatever its topics. The gap's own `seq` is `to`, so the
    /// stream's seqs still increase and resuming from it skips nothing more.
    pub const DAEMON_GAP: &str = "daemon.gap";
    /// `{key, version, by_device?}` ([`SettingsChanged`](crate::SettingsChanged)): a
    /// setting was set. Read its value with `settings.get`.
    pub const SETTINGS_CHANGED: &str = "settings.changed";
    /// `{key, version, by_device?}`: a setting was deleted, from `version`; it is back to
    /// never set, and its default applies.
    pub const SETTINGS_DELETED: &str = "settings.deleted";
    /// `{pty_id, bytes_b64}` ([`PtyOutput`](crate::PtyOutput)): a PTY's process wrote
    /// these bytes. rlxd keeps less of this output than of other events, apart from them.
    pub const PTY_OUTPUT: &str = "pty.output";
    /// `{pty_id, status, signal?}` ([`PtyExited`](crate::PtyExited)): a PTY's process
    /// ended. The PTY keeps its last screen until `pty.kill`.
    pub const PTY_EXITED: &str = "pty.exited";
    /// The screen ([`PtyScreen`](crate::PtyScreen)), first in a `pty.subscribe` stream
    /// and again after output the client missed. Only that stream carries it: rlxd keeps
    /// none, and its `seq` is the last event the screen takes in.
    pub const PTY_SNAPSHOT: &str = "pty.snapshot";
    /// The `hook` topic's types (1.5, 7.3), one per hook rlxd-hook hands rlxd, each with
    /// [`HookEventData`](crate::HookEventData): `{session_id, cwd, run_id?}`.
    pub const HOOK_SESSION_START: &str = "hook.session_start";
    /// See [`HOOK_SESSION_START`].
    pub const HOOK_USER_PROMPT_SUBMIT: &str = "hook.user_prompt_submit";
    /// See [`HOOK_SESSION_START`].
    pub const HOOK_SESSION_END: &str = "hook.session_end";
    /// See [`HOOK_SESSION_START`].
    pub const HOOK_STOP: &str = "hook.stop";
    /// See [`HOOK_SESSION_START`].
    pub const HOOK_STOP_FAILURE: &str = "hook.stop_failure";
    /// See [`HOOK_SESSION_START`]. (`pre-tool-use` has none: the gate answers it and
    /// publishes nothing, 5.1.)
    pub const HOOK_NOTIFICATION: &str = "hook.notification";
}

impl Event {
    /// An event of this schema version. For the `run` topic, use [`Event::for_run`].
    /// Refuses what `event.json` refuses: `seq` 0, a `time` that is not RFC 3339, a topic
    /// that is not snake_case, a type that is not `<topic>.<event>`, and a `run` event
    /// without its run fields.
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

    /// An event on the `run` topic: `data` with the run fields of `scope` set.
    pub fn for_run(
        seq: u64,
        time: impl Into<String>,
        r#type: impl Into<String>,
        scope: RunScope,
        mut data: Data,
    ) -> Result<Self, Invalid> {
        data.insert("run_id".to_owned(), Value::String(scope.run_id));
        data.insert("run_seq".to_owned(), Value::from(scope.run_seq));
        data.insert("kind".to_owned(), Value::String(scope.kind));
        data.insert("attempt".to_owned(), Value::from(scope.attempt));
        match scope.job_id {
            Some(job_id) => data.insert("job_id".to_owned(), Value::String(job_id)),
            None => data.remove("job_id"),
        };
        Self::new(seq, time, topics::RUN, r#type, data)
    }

    /// The run fields of this event, when it is on the `run` topic.
    pub fn run_scope(&self) -> Option<RunScope> {
        if self.topic != topics::RUN {
            return None;
        }
        let text = |field: &str| Some(self.data.get(field)?.as_str()?.to_owned());
        let number = |field: &str| self.data.get(field)?.as_u64();
        Some(RunScope {
            run_id: text("run_id")?,
            run_seq: number("run_seq")?,
            kind: text("kind")?,
            attempt: number("attempt")?,
            job_id: text("job_id"),
        })
    }

    /// Whether this event keeps every rule of `event.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        check::at_least("seq", self.seq, 1)?;
        check::date_time("time", &self.time)?;
        check::topic("topic", &self.topic)?;
        check::event_type("type", &self.r#type, &self.topic)?;
        if self.topic == topics::RUN {
            self.validate_run_fields()?;
        }
        Ok(())
    }

    fn validate_run_fields(&self) -> Result<(), Invalid> {
        for (field, what) in [("run_id", "its run's id"), ("kind", "its run's kind")] {
            let path = format!("data.{field}");
            match self.data.get(field) {
                Some(Value::String(value)) => check::non_empty(&path, value)?,
                _ => {
                    return Err(Invalid::new(
                        path,
                        format!("a run event needs {what}, a string (see Event::for_run)"),
                    ));
                }
            }
        }
        for (field, what) in [
            ("run_seq", "its place in the run"),
            ("attempt", "its attempt"),
        ] {
            let path = format!("data.{field}");
            match self.data.get(field).and_then(Value::as_u64) {
                Some(value) => check::at_least(&path, value, 1)?,
                None => {
                    return Err(Invalid::new(
                        path,
                        format!("a run event needs {what}, an integer of at least 1"),
                    ));
                }
            }
        }
        match self.data.get("job_id") {
            None => Ok(()),
            Some(Value::String(job_id)) => check::non_empty("data.job_id", job_id),
            Some(_) => Err(Invalid::new(
                "data.job_id",
                "must be a string, or absent when no job fired the run",
            )),
        }
    }
}

impl Serialize for Event {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(serde::ser::Error::custom)?;
        UncheckedEvent::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for Event {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let event = UncheckedEvent::deserialize(deserializer)?;
        event.validate().map_err(de::Error::custom)?;
        Ok(event)
    }
}

impl RunScope {
    /// Event `run_seq` of attempt `attempt` of run `run_id`, of kind `kind`, fired by no
    /// job.
    pub fn new(
        run_id: impl Into<String>,
        run_seq: u64,
        kind: impl Into<String>,
        attempt: u64,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            run_seq,
            kind: kind.into(),
            attempt,
            job_id: None,
        }
    }

    /// These run fields, for a run that job `job_id` fired.
    #[must_use]
    pub fn with_job(mut self, job_id: impl Into<String>) -> Self {
        self.job_id = Some(job_id.into());
        self
    }
}
