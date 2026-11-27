//! The response envelope (handoff 0.10.3, 7.2).

use std::borrow::Cow;
use std::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::{Value, json};

use crate::check::{self, Invalid, drop_null, present};
use crate::marker::{False, True, V1};
use crate::{PROTOCOL_VERSIONS, RequestId};

/// A response's or an event's `data`: always a JSON object, so a field can be added
/// without raising the schema version.
pub type Data = serde_json::Map<String, serde_json::Value>;

/// One response.
///
/// Under `--json --schema-version 1`, every rlx command prints exactly one of these.
/// On rlxd's socket, each response is one of these plus the request's `id`.
///
/// ```json
/// {"schema_version": 1, "ok": true, "data": {}}
/// {"schema_version": 1, "ok": false,
///  "error": {"code": "not_a_tty", "message": "…", "hint": "…", "detail": "…"}}
/// ```
///
/// `ok` decides: `ok: true` carries `data` and no `error`, and `ok: false` carries
/// `error` and no `data`. A document with both, or with the wrong one, is refused.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[schemars(title = "Envelope")]
#[non_exhaustive]
pub struct Envelope {
    /// The schema version this document follows.
    pub schema_version: V1,
    /// On the socket, the `id` of the request this answers. Absent from the CLI's output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub id: Option<RequestId>,
    /// Whether it worked, and what it returned or why it did not.
    #[serde(flatten)]
    pub outcome: Outcome,
}

/// An envelope's `ok`, with the `data` or the `error` that goes with it.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
#[schemars(transform = one_or_the_other)]
pub enum Outcome {
    /// It worked. The process exits 0, with one exception: `rlxd here` exits 3 (rlxd is
    /// installed but not running) and 4 (unsupported) with this envelope too, because its
    /// probe worked and the exit code says which case it found.
    Ok {
        /// `true`.
        ok: True,
        /// What it returned.
        data: Data,
    },
    /// It was refused or failed. The process exits 1, 2 on a usage error, or 70 when
    /// the command is not implemented.
    Err {
        /// `false`.
        ok: False,
        /// What went wrong.
        error: ErrorBody,
    },
}

/// The schema's half of "`ok` decides": the `ok: true` branch forbids `error`, and the
/// `ok: false` branch forbids `data`.
fn one_or_the_other(schema: &mut Schema) {
    let Some(Value::Array(branches)) = schema.get_mut("anyOf") else {
        return;
    };
    for branch in branches {
        let forbidden = match branch.pointer("/properties/ok/const") {
            Some(Value::Bool(true)) => "error",
            Some(Value::Bool(false)) => "data",
            _ => continue,
        };
        if let Value::Object(branch) = branch {
            branch.insert("not".to_owned(), json!({ "required": [forbidden] }));
        }
    }
}

/// Why a request was refused or failed.
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(rename = "Error", title = "Error", transform = detail_rules)]
#[non_exhaustive]
pub struct ErrorBody {
    /// A stable, machine-readable code, such as `unauthorized`, `unknown_method`,
    /// `incompatible_protocol`, `not_gate` or `not_a_tty`. A client also accepts codes it
    /// does not know.
    #[schemars(length(min = 1))]
    pub code: String,
    /// What went wrong, for a person.
    #[schemars(length(min = 1))]
    pub message: String,
    /// What to do about it, for a person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub hint: Option<String>,
    /// More about what went wrong: a string, such as an underlying error's text, or an
    /// object whose fields the code defines, such as `incompatible_protocol`'s
    /// `{"supported": [1]}` or `denied`'s `{"by": …}`. Never `null`: absent instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub detail: Option<Detail>,
}

/// The serde code of `ErrorBody`'s fields, unchecked. It is private, so the only way in or out
/// of the type is its own `Serialize` and `Deserialize`, which run `validate()`.
/// (`remote = "Self"` would make these unchecked functions public, as
/// `ErrorBody::serialize` and `ErrorBody::deserialize`.)
#[derive(Serialize, Deserialize)]
#[serde(remote = "ErrorBody")]
struct UncheckedError {
    code: String,
    message: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    hint: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    detail: Option<Detail>,
}

/// An error's `detail`: a string, or an object whose fields its code defines.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Detail {
    /// Text, such as an underlying error's message.
    Text(String),
    /// Fields the error's code defines.
    Object(Data),
}

impl<'de> Deserialize<'de> for Detail {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Value::deserialize(deserializer)? {
            Value::String(text) => Ok(Self::Text(text)),
            Value::Object(fields) => Ok(Self::Object(fields)),
            other => Err(de::Error::custom(format_args!(
                "detail must be a string or an object, not {other}"
            ))),
        }
    }
}

impl JsonSchema for Detail {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Detail")
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": ["string", "object"] })
    }
}

impl From<&str> for Detail {
    fn from(text: &str) -> Self {
        Self::Text(text.to_owned())
    }
}

impl From<String> for Detail {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

impl From<Data> for Detail {
    fn from(fields: Data) -> Self {
        Self::Object(fields)
    }
}

/// The schema's half of the codes whose `detail` is required: `incompatible_protocol`
/// and `since_ahead`.
fn detail_rules(schema: &mut Schema) {
    let mut rules = Schema::default();
    incompatible_protocol_rule(&mut rules);
    let mut since_ahead = Schema::default();
    since_ahead_rule(&mut since_ahead);
    schema.insert(
        "allOf".to_owned(),
        Value::Array(vec![rules.to_value(), since_ahead.to_value()]),
    );
}

/// The schema's half of `since_ahead`: such an error must say, in `detail.head`, the last
/// seq this rlxd has given out.
fn since_ahead_rule(schema: &mut Schema) {
    schema.insert(
        "if".to_owned(),
        json!({
            "properties": {"code": {"const": codes::SINCE_AHEAD}},
            "required": ["code"],
        }),
    );
    schema.insert(
        "then".to_owned(),
        json!({
            "properties": {"detail": {
                "type": "object",
                "properties": {
                    "head": {"type": "integer", "minimum": 0, "maximum": u64::MAX},
                    "since": {"type": "integer", "minimum": 0, "maximum": u64::MAX},
                },
                "required": ["head"],
            }},
            "required": ["detail"],
        }),
    );
}

/// The schema's half of `incompatible_protocol`: such an error must say, in
/// `detail.supported`, which protocol versions rlxd does serve.
fn incompatible_protocol_rule(schema: &mut Schema) {
    schema.insert(
        "if".to_owned(),
        json!({
            "properties": {"code": {"const": codes::INCOMPATIBLE_PROTOCOL}},
            "required": ["code"],
        }),
    );
    schema.insert(
        "then".to_owned(),
        json!({
            "properties": {"detail": {
                "type": "object",
                "properties": {"supported": {
                    "type": "array",
                    "items": {"type": "integer", "minimum": 1, "maximum": u32::MAX},
                    "minItems": 1,
                }},
                "required": ["supported"],
            }},
            "required": ["detail"],
        }),
    );
}

/// The error codes the contract names. A client must also accept codes not listed here.
pub mod codes {
    /// The connection's first frame was not a `hello` with the right token.
    pub const UNAUTHORIZED: &str = "unauthorized";
    /// No method of that name.
    pub const UNKNOWN_METHOD: &str = "unknown_method";
    /// `hello` asked for a protocol version this rlxd does not serve. `detail.supported`
    /// lists the ones it does, so the client can say "update" rather than misparse.
    pub const INCOMPATIBLE_PROTOCOL: &str = "incompatible_protocol";
    /// The connection may not be the tool-call gate (handoff 0.10.3, 5.1 and 7.2). It is
    /// returned for:
    /// - a `role: app` hello with no `gate_credential` or a wrong one;
    /// - a `role: app` hello from a peer that fails the identity check, outside gate dev
    ///   mode;
    /// - `gate.pair` from such a peer, `gate.pair` for a device already paired without
    ///   that device's current `gate_credential`, and a new device's `gate.pair` that no
    ///   paired device or user approved in time;
    /// - a tool-call decision from a connection that is not the gate.
    pub const NOT_GATE: &str = "not_gate";
    /// `gate.unpair`: no device of that name is paired.
    pub const UNKNOWN_DEVICE: &str = "unknown_device";
    /// `gate.pair_answer`: no pairing request of that id is waiting.
    pub const NO_PAIR_REQUEST: &str = "no_pair_request";
    /// `gate.pair_answer`: the request is for the name of a device the user unpaired,
    /// and the approver did not say `force`.
    pub const REVOKED_DEVICE: &str = "revoked_device";
    /// A frame after `hello` that is not a request: not JSON, not an object, no `method`,
    /// params that are not an object, or a second `hello`. A frame longer than rlxd
    /// reads also gets it, and then the connection closes.
    pub const INVALID_REQUEST: &str = "invalid_request";
    /// A method failed in a way its caller cannot fix, such as a file rlxd could not
    /// read. The message says what.
    pub const INTERNAL: &str = "internal";
    /// `rlxd serve`: an rlxd already runs for this home, so this one does not start.
    pub const ALREADY_RUNNING: &str = "already_running";
    /// `rlxd serve`: rlxd could not start, such as when the socket could not be made, or
    /// a detached rlxd exited before it took a connection.
    pub const START_FAILED: &str = "start_failed";
    /// A client command (`rlxd call`, `status`, `stop`): no rlxd runs for this home. Its
    /// exit code is 3.
    pub const NOT_RUNNING: &str = "not_running";
    /// A client command: rlxd closed the connection, or failed to answer, before it sent
    /// a reply.
    pub const DISCONNECTED: &str = "disconnected";
    /// `rlxd stop`: rlxd took the request but was still running when the wait ran out.
    pub const STOP_TIMED_OUT: &str = "stop_timed_out";
    /// A request's params are not what its method takes: a field missing, of the wrong
    /// type or out of range. The message names the field.
    pub const INVALID_PARAMS: &str = "invalid_params";
    /// `events.subscribe` named a topic this rlxd does not carry. The hint lists the
    /// ones it does.
    pub const UNKNOWN_TOPIC: &str = "unknown_topic";
    /// `rlxd events --timeout`: the time ran out first. Its exit code is 4.
    pub const TIMED_OUT: &str = "timed_out";
    /// `rlxd events --until`: the stream ended before one of the types came.
    pub const STREAM_ENDED: &str = "stream_ended";
    /// `events.subscribe`'s `since` is past the last event this rlxd has given out, so it
    /// comes from another numbering of the home's events, such as one from before the
    /// mark was deleted. `detail.head` is the last seq. Re-read what you show, then
    /// subscribe again from `status`'s `seq`, or without `since`.
    pub const SINCE_AHEAD: &str = "since_ahead";
    /// `settings.set {expected_version}`: the setting's version is not that one now, as
    /// another client set it first. `detail.version` is its version now: read it again,
    /// and write from that.
    pub const CONFLICT: &str = "conflict";
    /// A setting key without a `.` that this rlxd does not know. Dotted keys,
    /// `<namespace>.<name>`, are always free to use.
    pub const UNKNOWN_SETTING: &str = "unknown_setting";
    /// `run.start`: rlxd already knows a run of that `run_id`, live or ended.
    pub const RUN_EXISTS: &str = "run_exists";
    /// `run.start`: a `check`, `parse_instruction` or `widget_author` run whose mount is
    /// not `no_tools`.
    pub const KIND_MOUNT_MISMATCH: &str = "kind_mount_mismatch";
    /// `run.start`: a `send_executor` or `watch` run without `pinned_call`.
    pub const PINNED_CALL_REQUIRED: &str = "pinned_call_required";
    /// The request asks for something this rlxd does not do yet; the message names it.
    pub const NOT_IMPLEMENTED: &str = "not_implemented";
    /// No run of that `run_id` is known.
    pub const UNKNOWN_RUN: &str = "unknown_run";
    /// rlxd could not start the run, such as when Claude Code is not installed; the
    /// message says why.
    pub const RUN_START_FAILED: &str = "run_start_failed";
    /// `run.resume`: the run is queued or running; only a run with no live process
    /// resumes.
    pub const RUN_ALIVE: &str = "run_alive";
    /// `run.resume`: the run kept no session (`persist_session: false`), so there is
    /// nothing to resume.
    pub const NO_SESSION: &str = "no_session";
    /// `run.resume`: a `send_executor` run is never resumed, since a send is never
    /// retried.
    pub const NOT_RESUMABLE: &str = "not_resumable";
    /// `space.erase`: a run in the space is queued or running. `detail.run_ids` names
    /// them.
    pub const RUNS_LIVE: &str = "runs_live";
    /// `space.erase`: a link (a junction on Windows) or a mount point is on the way to what
    /// it removes: `<home>/spaces`, the space, a Claude Code config directory's `projects`,
    /// or the space's folder there. rlxd does not go through one; `detail.paths` names
    /// them, and nothing was removed. (`<home>` and a config directory are roots, taken as
    /// they resolve, a link included.)
    pub const LINK_REFUSED: &str = "link_refused";
    /// `space.erase`: some paths could not be removed. `detail.removed` and
    /// `detail.failed[{path, error}]` say which; the runs whose files remain keep their
    /// records, so erasing again finishes the job.
    pub const ERASE_INCOMPLETE: &str = "erase_incomplete";
    /// `space.erase`: the space is being erased already, by another call; wait for it.
    pub const ERASE_IN_PROGRESS: &str = "erase_in_progress";
    /// `job.*`: rlxd holds no job of that `job_id`.
    pub const UNKNOWN_JOB: &str = "unknown_job";
    /// `job.run_now`: no app gate client is connected, and jobs fire only while one is
    /// (2.3).
    pub const APP_ABSENT: &str = "app_absent";
    /// `job.run_now`: the job's last run is still queued or running; a job never
    /// overlaps itself.
    pub const JOB_RUNNING: &str = "job_running";
    /// `job.run_now`: the job is disabled (`enabled: false`), and a disabled job never
    /// fires.
    pub const JOB_DISABLED: &str = "job_disabled";
    /// `job.run_now`: a run of the job was rate-limited, and the limit hasn't reset;
    /// `detail.resets_at` says when it does.
    pub const RATE_LIMITED: &str = "rate_limited";
    /// `run.start`: rlxd can't name the absolute path of the `rlxd-hook` beside it, which
    /// every run's gate hook runs by (never a name looked up on `PATH`). The message says
    /// why.
    pub const HOOK_NOT_FOUND: &str = "hook_not_found";
}

impl Envelope {
    /// A successful response carrying `data`.
    pub fn ok(data: Data) -> Self {
        Self {
            schema_version: V1,
            id: None,
            outcome: Outcome::Ok { ok: True, data },
        }
    }

    /// A refused or failed response.
    pub fn err(error: ErrorBody) -> Self {
        Self {
            schema_version: V1,
            id: None,
            outcome: Outcome::Err { ok: False, error },
        }
    }

    /// This response, as the answer to the request `id`.
    #[must_use]
    pub fn with_id(mut self, id: RequestId) -> Self {
        self.id = Some(id);
        self
    }

    /// Whether it worked.
    pub fn is_ok(&self) -> bool {
        matches!(self.outcome, Outcome::Ok { .. })
    }

    /// The `data` when it worked, or the `error` when it did not.
    pub fn into_result(self) -> Result<Data, ErrorBody> {
        match self.outcome {
            Outcome::Ok { data, .. } => Ok(data),
            Outcome::Err { error, .. } => Err(error),
        }
    }

    /// Whether this envelope keeps every rule of `envelope.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        match &self.outcome {
            Outcome::Ok { .. } => Ok(()),
            Outcome::Err { error, .. } => error.validate().map_err(|e| e.within("error")),
        }
    }
}

impl From<Result<Data, ErrorBody>> for Envelope {
    fn from(result: Result<Data, ErrorBody>) -> Self {
        match result {
            Ok(data) => Self::ok(data),
            Err(error) => Self::err(error),
        }
    }
}

impl<'de> Deserialize<'de> for Envelope {
    /// Reads `ok` first, then requires the field that goes with it and refuses the one
    /// that does not, so each error names the field at fault.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Fields {
            schema_version: V1,
            #[serde(default, deserialize_with = "present")]
            id: Option<RequestId>,
            ok: bool,
            #[serde(default, deserialize_with = "present")]
            data: Option<Value>,
            #[serde(default, deserialize_with = "present")]
            error: Option<Value>,
        }
        let fields = Fields::deserialize(deserializer)?;
        let refuse = |field: &str, problem: &str| -> D::Error {
            de::Error::custom(Invalid::new(field, problem))
        };
        let outcome = match (fields.ok, fields.data, fields.error) {
            (true, _, Some(_)) => return Err(refuse("error", "must be absent when ok is true")),
            (true, None, None) => return Err(refuse("data", "is required when ok is true")),
            (true, Some(Value::Object(data)), None) => Outcome::Ok { ok: True, data },
            (true, Some(_), None) => return Err(refuse("data", "must be an object")),
            (false, Some(_), _) => return Err(refuse("data", "must be absent when ok is false")),
            (false, None, None) => return Err(refuse("error", "is required when ok is false")),
            (false, None, Some(error)) => Outcome::Err {
                ok: False,
                error: ErrorBody::deserialize(error)
                    .map_err(|e| de::Error::custom(format_args!("error: {e}")))?,
            },
        };
        Ok(Self {
            schema_version: fields.schema_version,
            id: fields.id,
            outcome,
        })
    }
}

impl ErrorBody {
    /// An error with a code and a message, and no hint or detail. Refuses an empty code
    /// or message, and the codes that need their `detail`: build those with
    /// [`ErrorBody::incompatible_protocol`] and [`ErrorBody::since_ahead`].
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Result<Self, Invalid> {
        let error = Self {
            code: code.into(),
            message: message.into(),
            hint: None,
            detail: None,
        };
        error.validate()?;
        Ok(error)
    }

    /// This error, with what to do about it.
    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// This error, with more about what went wrong: a string, or an object. Refuses a
    /// detail the code does not allow.
    pub fn with_detail(mut self, detail: impl Into<Detail>) -> Result<Self, Invalid> {
        self.detail = Some(detail.into());
        self.validate()?;
        Ok(self)
    }

    /// The reply to a `hello` whose `protocol_version` this crate does not serve. Takes
    /// the version as it was sent, so one that no [`HelloParams`](crate::HelloParams) can
    /// hold, such as `2^32` or `-1`, is still named in the message. Read it with
    /// [`protocol_version_of`](crate::protocol_version_of), or from the raw params.
    pub fn incompatible_protocol(requested: impl fmt::Display) -> Self {
        let mut detail = Data::new();
        detail.insert("supported".to_owned(), json!(PROTOCOL_VERSIONS));
        Self {
            code: codes::INCOMPATIBLE_PROTOCOL.to_owned(),
            message: format!("protocol version {requested} is not one this rlxd serves"),
            hint: Some(
                "Update rlx or the app, so that both speak one protocol version.".to_owned(),
            ),
            detail: Some(Detail::Object(detail)),
        }
    }

    /// The reply to an `events.subscribe` whose `since` is past `head`, the last seq this
    /// rlxd has given out.
    pub fn since_ahead(since: u64, head: u64) -> Self {
        let mut detail = Data::new();
        detail.insert("since".to_owned(), json!(since));
        detail.insert("head".to_owned(), json!(head));
        Self {
            code: codes::SINCE_AHEAD.to_owned(),
            message: format!(
                "since {since} is past this home's last event, {head}: it comes from another \
                 numbering of its events"
            ),
            hint: Some(
                "Re-read what you show, then subscribe from status's seq, or without since."
                    .to_owned(),
            ),
            detail: Some(Detail::Object(detail)),
        }
    }

    /// Whether this error keeps every rule of `envelope.json`'s `Error`.
    pub fn validate(&self) -> Result<(), Invalid> {
        check::non_empty("code", &self.code)?;
        check::non_empty("message", &self.message)?;
        if self.code == codes::SINCE_AHEAD {
            let fields = match &self.detail {
                Some(Detail::Object(fields)) => Some(fields),
                _ => None,
            };
            let number = |name: &str| fields.and_then(|f| f.get(name)).map(Value::as_u64);
            let valid =
                matches!(number("head"), Some(Some(_))) && !matches!(number("since"), Some(None));
            if !valid {
                return Err(Invalid::new(
                    "detail.head",
                    "since_ahead needs the last seq rlxd has given out, an integer, and \
                     since, when present, is one too",
                ));
            }
        }
        if self.code == codes::INCOMPATIBLE_PROTOCOL {
            let supported = match &self.detail {
                Some(Detail::Object(fields)) => fields.get("supported"),
                _ => None,
            };
            let versions = supported
                .and_then(Value::as_array)
                .filter(|v| !v.is_empty());
            let valid = versions.is_some_and(|versions| {
                let version = |v: &Value| v.as_u64().and_then(|v| u32::try_from(v).ok());
                versions.iter().all(|v| version(v).is_some_and(|v| v >= 1))
            });
            if !valid {
                return Err(Invalid::new(
                    "detail.supported",
                    "incompatible_protocol needs the versions rlxd serves, a non-empty list \
                     of protocol versions, integers from 1 to 2^32 - 1",
                ));
            }
        }
        Ok(())
    }
}

impl Serialize for ErrorBody {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(serde::ser::Error::custom)?;
        UncheckedError::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for ErrorBody {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let error = UncheckedError::deserialize(deserializer)?;
        error.validate().map_err(de::Error::custom)?;
        Ok(error)
    }
}

impl fmt::Display for ErrorBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ErrorBody {}
