//! `hello`, the first frame on every connection (handoff 0.10.3, 7.2 and 4.3).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `hello`'s params: who is connecting, as what, and the token that lets them.
///
/// ```json
/// {"token": "…", "client": "rlxapp", "client_version": "0.1.0", "role": "app", "env": {"CLAUDE_CONFIG_DIR": "…"}}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "HelloParams")]
#[non_exhaustive]
pub struct HelloParams {
    /// The contents of `<home>/rlxd/token`.
    #[schemars(length(min = 1))]
    pub token: String,
    /// Which client this is, such as `rlxapp`, `rlx` or `remote`.
    #[schemars(length(min = 1))]
    pub client: String,
    /// The client's own version.
    pub client_version: String,
    /// What the client connects as. rlxd counts its clients by role.
    pub role: Role,
    /// Variables rlxd keeps and passes to the brain and to every Claude Code child, such
    /// as `CLAUDE_CONFIG_DIR` and `CLAUDE_SECURESTORAGE_CONFIG_DIR`. Absent leaves what
    /// rlxd already has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "BTreeMap<String, String>")]
    pub env: Option<BTreeMap<String, String>>,
}

/// What a client connects as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// rlxapp. The tool-call gate and `client_connected` jobs need one connected.
    App,
    /// rlx's terminal UI.
    Tui,
    /// rlx's Remote server.
    Remote,
    /// A command-line client, such as `rlxd call`.
    Cli,
}

/// `hello`'s reply: what this rlxd is and what it can do.
///
/// ```json
/// {"daemon_version": "0.10.3", "schema_versions": [1], "capabilities": []}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "HelloData")]
#[non_exhaustive]
pub struct HelloData {
    /// rlxd's version, which is rlx's.
    pub daemon_version: String,
    /// Every schema version this rlxd serves.
    #[schemars(length(min = 1))]
    pub schema_versions: Vec<u32>,
    /// What this rlxd can do, by name. A client ignores names it does not know.
    pub capabilities: Vec<String>,
}

impl HelloParams {
    /// A `hello` with no `env`.
    pub fn new(
        token: impl Into<String>,
        client: impl Into<String>,
        client_version: impl Into<String>,
        role: Role,
    ) -> Self {
        Self {
            token: token.into(),
            client: client.into(),
            client_version: client_version.into(),
            role,
            env: None,
        }
    }

    /// This `hello`, carrying `env` for rlxd to keep.
    #[must_use]
    pub fn with_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.env = Some(env);
        self
    }
}

impl HelloData {
    /// A reply.
    pub fn new(
        daemon_version: impl Into<String>,
        schema_versions: Vec<u32>,
        capabilities: Vec<String>,
    ) -> Self {
        Self {
            daemon_version: daemon_version.into(),
            schema_versions,
            capabilities,
        }
    }
}
