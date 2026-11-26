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
