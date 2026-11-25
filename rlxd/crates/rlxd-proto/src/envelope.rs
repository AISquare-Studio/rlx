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
    /// It worked. The process exits 0.
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
#[schemars(rename = "Error", title = "Error", transform = incompatible_protocol_rule)]
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
    /// - `gate.pair` from such a peer;
    /// - a tool-call decision from a connection that is not the gate.
    pub const NOT_GATE: &str = "not_gate";
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
    /// or message, and `incompatible_protocol`, which needs its `detail`: build that one
    /// with [`ErrorBody::incompatible_protocol`].
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

    /// Whether this error keeps every rule of `envelope.json`'s `Error`.
    pub fn validate(&self) -> Result<(), Invalid> {
        check::non_empty("code", &self.code)?;
        check::non_empty("message", &self.message)?;
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
