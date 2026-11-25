//! The response envelope (handoff 0.10.3, 7.2).

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::marker::{False, True};
use crate::{RequestId, SCHEMA_VERSION};

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
/// {"schema_version": 1, "ok": false, "error": {"code": "not_a_tty", "message": "…", "hint": "…", "detail": "…"}}
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "Envelope")]
#[non_exhaustive]
pub struct Envelope {
    /// The schema version this document follows.
    #[schemars(extend("const" = 1))]
    pub schema_version: u32,
    /// On the socket, the `id` of the request this answers. Absent from the CLI's output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "RequestId")]
    pub id: Option<RequestId>,
    /// Whether it worked, and what it returned or why it did not.
    #[serde(flatten)]
    pub outcome: Outcome,
}

/// An envelope's `ok`, with the `data` or the `error` that goes with it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
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

/// Why a request was refused or failed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "Error", title = "Error")]
#[non_exhaustive]
pub struct ErrorBody {
    /// A stable, machine-readable code, such as `unauthorized` or `not_a_tty`.
    #[schemars(length(min = 1))]
    pub code: String,
    /// What went wrong, for a person.
    #[schemars(length(min = 1))]
    pub message: String,
    /// What to do about it, for a person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub hint: Option<String>,
    /// More about what went wrong, such as an underlying error's text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub detail: Option<String>,
}

/// The error codes the contract names. A client must also accept codes not listed here.
pub mod codes {
    /// The connection's first frame was not a `hello` with the right token.
    pub const UNAUTHORIZED: &str = "unauthorized";
    /// No method of that name.
    pub const UNKNOWN_METHOD: &str = "unknown_method";
}

impl Envelope {
    /// A successful response carrying `data`.
    pub fn ok(data: Data) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            id: None,
            outcome: Outcome::Ok { ok: True, data },
        }
    }

    /// A refused or failed response.
    pub fn err(error: ErrorBody) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
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
}

impl From<Result<Data, ErrorBody>> for Envelope {
    fn from(result: Result<Data, ErrorBody>) -> Self {
        match result {
            Ok(data) => Self::ok(data),
            Err(error) => Self::err(error),
        }
    }
}

impl ErrorBody {
    /// An error with a code and a message, and no hint or detail.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            hint: None,
            detail: None,
        }
    }

    /// This error, with what to do about it.
    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// This error, with more about what went wrong.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

impl fmt::Display for ErrorBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ErrorBody {}
