//! The request frame (handoff 0.10.3, 7.2).

use std::borrow::Cow;
use std::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::check::{self, Invalid};

/// A request's `params`: always a JSON object, `{}` when the frame has none.
pub type Params = serde_json::Map<String, serde_json::Value>;

/// A request's `id`, which its response echoes: an integer from 0 to 2^53 - 1, so a
/// JavaScript client holds every one exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct RequestId(u64);

impl RequestId {
    /// The largest id: 2^53 - 1, JavaScript's `Number.MAX_SAFE_INTEGER`.
    pub const MAX: u64 = (1 << 53) - 1;

    /// The id `id`, if it is at most [`RequestId::MAX`].
    pub const fn new(id: u64) -> Option<Self> {
        if id <= Self::MAX {
            Some(Self(id))
        } else {
            None
        }
    }

    /// The id, as a number.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl From<u32> for RequestId {
    fn from(id: u32) -> Self {
        Self(u64::from(id))
    }
}

impl TryFrom<u64> for RequestId {
    type Error = Invalid;

    fn try_from(id: u64) -> Result<Self, Invalid> {
        Self::new(id)
            .ok_or_else(|| Invalid::new("id", format!("{id} is above 2^53 - 1 ({})", Self::MAX)))
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<'de> Deserialize<'de> for RequestId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let id = u64::deserialize(deserializer)?;
        Self::try_from(id).map_err(de::Error::custom)
    }
}

impl JsonSchema for RequestId {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("RequestId")
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "integer", "minimum": 0, "maximum": Self::MAX })
    }
}

/// One request on rlxd's socket. Frames are newline-delimited JSON:
///
/// ```json
/// {"id": 7, "method": "run.start", "params": {}}
/// ```
///
/// The response is an envelope with the same `id`. The first frame on a connection must
/// be `hello`; any other first frame gets the error `unauthorized`.
#[derive(Clone, Debug, PartialEq, JsonSchema)]
#[schemars(title = "Request")]
#[non_exhaustive]
pub struct Request {
    /// Chosen by the client; the response carries it back.
    pub id: RequestId,
    /// The method to call, such as `hello`, `status` or `run.start`.
    #[schemars(length(min = 1))]
    pub method: String,
    /// The method's parameters. Absent means `{}`.
    #[serde(default)]
    pub params: Params,
}

/// The serde code of `Request`'s fields, unchecked. It is private, so the only way in or out
/// of the type is its own `Serialize` and `Deserialize`, which run `validate()`.
/// (`remote = "Self"` would make these unchecked functions public, as
/// `Request::serialize` and `Request::deserialize`.)
#[derive(Serialize, Deserialize)]
#[serde(remote = "Request")]
struct UncheckedRequest {
    id: RequestId,
    method: String,
    #[serde(default)]
    params: Params,
}

/// The method names this crate defines types for.
pub mod methods {
    /// The first frame on every connection: [`HelloParams`](crate::HelloParams) in,
    /// [`HelloData`](crate::HelloData) out.
    pub const HELLO: &str = "hello";
    /// What this rlxd is and how it is doing: no params in,
    /// [`StatusData`](crate::StatusData) out (7.4).
    pub const STATUS: &str = "status";
    /// Stops rlxd: no params in, `{pid}` out. rlxd answers, then stops taking connections,
    /// cleans up and exits.
    pub const STOP: &str = "stop";
    /// Asks the resident Python brain (1.6) to answer, through its queue like a hook:
    /// no params in, `{pid}` (the brain's) out. While no brain runs, or it does not take
    /// the request within 5 s, a `brain_*` error says why. rlxd-hook's `ping` (1.5); only a
    /// `cli` client may call it, any other gets `role_not_allowed`.
    pub const HOOK_PING: &str = "hook.ping";
    /// Pairs a device as the app gate (5.1): `{device?, gate_credential?}` in,
    /// `{gate_credential, device, route}` out. The credential is shown this once.
    pub const GATE_PAIR: &str = "gate.pair";
    /// The pairing requests waiting for a paired device or the user to answer them: no
    /// params in, `{requests: [{request_id, device, route, peer: {pid, image}, revoked,
    /// requested_at, expires_at}]}` out.
    pub const GATE_PAIR_REQUESTS: &str = "gate.pair_requests";
    /// Answers a waiting pairing request: `{request_id, approve, force?}` in,
    /// `{request_id, device, approved}` out. Approving a request for a revoked device's
    /// name needs `force`. Only a paired app, or a command-line client on the host
    /// (`rlxd gate pair`), may answer.
    pub const GATE_PAIR_ANSWER: &str = "gate.pair_answer";
    /// The paired devices: no params in, `{devices: [{device, route, created, last_seen,
    /// connections}]}` out.
    pub const GATE_DEVICES: &str = "gate.devices";
    /// Unpairs one device, or every device: `{device?}` in, `{unpaired, dropped}` out.
    /// The user runs it (`rlxd gate unpair`); it is not part of the app contract.
    pub const GATE_UNPAIR: &str = "gate.unpair";
    /// Opens the event stream (7.3): [`SubscribeParams`](crate::SubscribeParams) in, then
    /// event frames, then the reply `{seq}`, which ends the stream.
    pub const EVENTS_SUBSCRIBE: &str = "events.subscribe";
    /// Test builds only: publishes `count` events, `{count}` in and `{first, last}` (their
    /// seqs) out, so a test can fill the event buffer. A release build has no such method
    /// and answers `unknown_method`.
    pub const DEBUG_EMIT: &str = "debug.emit";
    /// Test builds only: makes a PTY's screen model panic the next time rlxd calls it,
    /// `{pty_id, at}` in with `at` one of `output`, `screen` and `resize`, `{pty_id}` out,
    /// so a test can show a panic there ends that PTY alone. A release build has no such
    /// method and answers `unknown_method`.
    pub const DEBUG_PTY_PANIC: &str = "debug.pty_panic";
    /// One app setting: [`SettingsGetParams`](crate::SettingsGetParams) in,
    /// [`Setting`](crate::Setting) out.
    pub const SETTINGS_GET: &str = "settings.get";
    /// Sets an app setting: [`SettingsSetParams`](crate::SettingsSetParams) in,
    /// [`Setting`](crate::Setting) out, and `settings.changed` on the stream.
    pub const SETTINGS_SET: &str = "settings.set";
    /// Every app setting set so far: [`SettingsListParams`](crate::SettingsListParams) in,
    /// [`SettingsList`](crate::SettingsList) out.
    pub const SETTINGS_LIST: &str = "settings.list";
    /// Deletes an app setting, back to never set:
    /// [`SettingsDeleteParams`](crate::SettingsDeleteParams) in, [`Setting`](crate::Setting)
    /// out at version 0, and `settings.deleted` on the stream.
    pub const SETTINGS_DELETE: &str = "settings.delete";
    /// Starts a process on a new PTY: [`PtySpawnParams`](crate::PtySpawnParams) in,
    /// [`PtyRef`](crate::PtyRef) out. Never gated by the app.
    pub const PTY_SPAWN: &str = "pty.spawn";
    /// Types text, presses keys or pastes into a PTY:
    /// [`PtyWriteParams`](crate::PtyWriteParams) in, [`PtyRef`](crate::PtyRef) out.
    pub const PTY_WRITE: &str = "pty.write";
    /// Resizes a PTY: [`PtyResizeParams`](crate::PtyResizeParams) in,
    /// [`PtyRef`](crate::PtyRef) out.
    pub const PTY_RESIZE: &str = "pty.resize";
    /// A PTY's screen, as `tmux capture-pane -e -p` prints it:
    /// [`PtyScreenParams`](crate::PtyScreenParams) in, [`PtyScreen`](crate::PtyScreen) out.
    pub const PTY_SCREEN: &str = "pty.screen";
    /// Streams a PTY's screen, then its output:
    /// [`PtySubscribeParams`](crate::PtySubscribeParams) in; the reply ends the stream.
    pub const PTY_SUBSCRIBE: &str = "pty.subscribe";
    /// Ends a PTY's process and every process in its group or job, and frees the PTY:
    /// [`PtyKillParams`](crate::PtyKillParams) in, [`PtyRef`](crate::PtyRef) out.
    pub const PTY_KILL: &str = "pty.kill";
    /// Every PTY of the home: no params, [`PtyList`](crate::PtyList) out.
    pub const PTY_LIST: &str = "pty.list";
    /// Starts a run (7.5): [`RunStartParams`](crate::RunStartParams) in,
    /// [`RunRef`](crate::RunRef) out, then the run's events on the `run` topic.
    pub const RUN_START: &str = "run.start";
    /// Cancels a run, queued or running, killing its process tree:
    /// [`RunRef`](crate::RunRef) in, [`RunCancelData`](crate::RunCancelData) out.
    pub const RUN_CANCEL: &str = "run.cancel";
    /// Makes a space's folder if it is not there (2.5):
    /// [`SpaceParams`](crate::SpaceParams) in, [`SpaceEnsureData`](crate::SpaceEnsureData)
    /// out.
    pub const SPACE_ENSURE: &str = "space.ensure";
    /// Goes on with a run that has no live process, as a new attempt:
    /// [`RunResumeParams`](crate::RunResumeParams) in,
    /// [`RunResumeData`](crate::RunResumeData) out.
    pub const RUN_RESUME: &str = "run.resume";
    /// One run, from its durable record: [`RunRef`](crate::RunRef) in,
    /// [`RunGetData`](crate::RunGetData) out.
    pub const RUN_GET: &str = "run.get";
    /// Every run rlxd knows: [`RunListParams`](crate::RunListParams) in,
    /// [`RunListData`](crate::RunListData) out.
    pub const RUN_LIST: &str = "run.list";
    /// A run's events from its durable log, then live until it ends:
    /// [`RunEventsParams`](crate::RunEventsParams) in, event frames, then
    /// [`RunEventsData`](crate::RunEventsData), which ends the stream.
    pub const RUN_EVENTS: &str = "run.events";
    /// Deletes a space, its runs' Claude Code sessions and rlxd's records of its runs:
    /// [`SpaceParams`](crate::SpaceParams) in, [`SpaceEraseData`](crate::SpaceEraseData)
    /// out.
    pub const SPACE_ERASE: &str = "space.erase";
    /// The user's connectors, from the latest `system/init`, or refreshed:
    /// [`ConnectorsListParams`](crate::ConnectorsListParams) in,
    /// [`ConnectorsList`](crate::ConnectorsList) out (7.7).
    pub const CONNECTORS_LIST: &str = "connectors.list";
    /// Signs a connector in again, through `claude mcp login` in an rlxd PTY:
    /// [`ConnectorsLoginParams`](crate::ConnectorsLoginParams) in,
    /// [`ConnectorsLoginData`](crate::ConnectorsLoginData) out, then `connector.login.*`
    /// events (7.7). Not for a `remote` connection.
    pub const CONNECTORS_LOGIN: &str = "connectors.login";
    /// Test builds only: `run.start` with a scheduler `job_id`, which no other client can
    /// set: [`RunStartParams`](crate::RunStartParams) plus `job_id` in,
    /// [`RunRef`](crate::RunRef) out. A release build answers `unknown_method`.
    pub const DEBUG_RUN_START: &str = "debug.run_start";
    /// The app's answer to a `run.tool_call`, from an app-role connection only (5.1):
    /// [`RunDecideParams`](crate::RunDecideParams) in,
    /// [`RunDecideData`](crate::RunDecideData) out.
    pub const RUN_DECIDE: &str = "run.decide";
    /// Test builds only: a run's PreToolUse hook request, as `rlxd-hook pre-tool-use` will
    /// send it, decided by the gate: `{run_id, attempt, key, stdin}` in, `{output}` (the
    /// hook's JSON for Claude Code) out. A release build answers `unknown_method`.
    pub const DEBUG_PRE_TOOL_USE: &str = "debug.pre_tool_use";
    /// Test builds only: drops every hook connection waiting on the gate, as a crash of
    /// the connection would (5.1): `{}` in, `{dropped}` out.
    pub const DEBUG_DROP_HOOK_CONNECTIONS: &str = "debug.drop_hook_connections";
    /// Creates or replaces a scheduler job by `job_id` (7.6):
    /// [`JobPutParams`](crate::JobPutParams) in, [`JobRef`](crate::JobRef) out.
    pub const JOB_PUT: &str = "job.put";
    /// Every job: [`JobListParams`](crate::JobListParams) in,
    /// [`JobListData`](crate::JobListData) out.
    pub const JOB_LIST: &str = "job.list";
    /// Deletes a job: [`JobRef`](crate::JobRef) in and out.
    pub const JOB_DELETE: &str = "job.delete";
    /// Pauses a job: its due fires are skipped `paused`. [`JobRef`](crate::JobRef) in and
    /// out.
    pub const JOB_PAUSE: &str = "job.pause";
    /// Resumes a paused job: [`JobRef`](crate::JobRef) in and out.
    pub const JOB_RESUME: &str = "job.resume";
    /// Fires a job now, which needs an app gate client: [`JobRef`](crate::JobRef) in,
    /// [`JobRunNowData`](crate::JobRunNowData) out.
    pub const JOB_RUN_NOW: &str = "job.run_now";
    /// Test builds only: moves the scheduler's test clock on, and answers once every
    /// decision due by then is made: [`ClockAdvanceParams`](crate::ClockAdvanceParams) in,
    /// `{now}` out. A release build answers `unknown_method`.
    pub const DEBUG_CLOCK_ADVANCE: &str = "debug.clock_advance";
    /// Test builds only: sets the scheduler's test clock, as `debug.clock_advance` moves
    /// it: [`ClockSetParams`](crate::ClockSetParams) in, `{now}` out.
    pub const DEBUG_CLOCK_SET: &str = "debug.clock_set";
}

impl Request {
    /// A request for `method` with `params`. Refuses an empty method.
    pub fn new(id: RequestId, method: impl Into<String>, params: Params) -> Result<Self, Invalid> {
        let request = Self {
            id,
            method: method.into(),
            params,
        };
        request.validate()?;
        Ok(request)
    }

    /// Whether this request keeps every rule of `request.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        check::non_empty("method", &self.method)
    }
}

impl Serialize for Request {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(serde::ser::Error::custom)?;
        UncheckedRequest::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for Request {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let request = UncheckedRequest::deserialize(deserializer)?;
        request.validate().map_err(de::Error::custom)?;
        Ok(request)
    }
}
