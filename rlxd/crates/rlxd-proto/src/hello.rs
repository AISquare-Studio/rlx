//! `hello`, the first frame on every connection (handoff 0.10.3, 7.2 and 4.3).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `hello`'s params: who is connecting, and the token that lets them.
///
/// ```json
/// {"token": "…", "client": "rlxapp", "client_version": "0.1.0", "env": {"CLAUDE_CONFIG_DIR": "…"}}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "HelloParams")]
pub struct HelloParams {
    /// The contents of `<home>/rlxd/token`.
    #[schemars(length(min = 1))]
    pub token: String,
    /// Which client this is, such as `rlxapp`, `rlx` or `remote`.
    #[schemars(length(min = 1))]
    pub client: String,
    /// The client's own version.
    pub client_version: String,
    /// Variables rlxd keeps and passes to the brain and to every Claude Code child, such
    /// as `CLAUDE_CONFIG_DIR` and `CLAUDE_SECURESTORAGE_CONFIG_DIR`. Absent leaves what
    /// rlxd already has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "BTreeMap<String, String>")]
    pub env: Option<BTreeMap<String, String>>,
}

/// `hello`'s reply: what this rlxd is and what it can do.
///
/// ```json
/// {"daemon_version": "0.10.3", "schema_versions": [1], "capabilities": []}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "HelloData")]
pub struct HelloData {
    /// rlxd's version, which is rlx's.
    pub daemon_version: String,
    /// Every schema version this rlxd serves.
    #[schemars(length(min = 1))]
    pub schema_versions: Vec<u32>,
    /// What this rlxd can do, by name. A client ignores names it does not know.
    pub capabilities: Vec<String>,
}
