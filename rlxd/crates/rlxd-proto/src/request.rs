//! The request frame (handoff 0.10.3, 7.2).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A request's `id`, which its response echoes.
pub type RequestId = u64;

/// A request's `params`: always a JSON object, `{}` when the frame has none.
pub type Params = serde_json::Map<String, serde_json::Value>;

/// One request on rlxd's socket. Frames are newline-delimited JSON:
///
/// ```json
/// {"id": 7, "method": "run.start", "params": {}}
/// ```
///
/// The response is an envelope with the same `id`. The first frame on a connection must
/// be `hello`; any other first frame gets the error `unauthorized`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "Request")]
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

/// The method names this crate defines types for.
pub mod methods {
    /// The first frame on every connection: [`HelloParams`](crate::HelloParams) in,
    /// [`HelloData`](crate::HelloData) out.
    pub const HELLO: &str = "hello";
}

impl Request {
    /// A request for `method` with `params`.
    pub fn new(id: RequestId, method: impl Into<String>, params: Params) -> Self {
        Self {
            id,
            method: method.into(),
            params,
        }
    }
}
