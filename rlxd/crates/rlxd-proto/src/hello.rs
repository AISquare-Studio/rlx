//! `hello`, the protocol handshake and the first frame on every connection (handoff
//! 0.10.3, 7.2 and 4.3; rlxapp's Engine change spec, section 3).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{PROTOCOL_VERSION, PROTOCOL_VERSIONS};

/// `hello`'s params: who is connecting, as what, which protocol it speaks, and the
/// token that lets it in.
///
/// ```json
/// {"token": "…", "protocol_version": 1, "client": "rlxapp", "client_version": "0.1.0",
///  "role": "app", "capabilities": [], "env": {"CLAUDE_CONFIG_DIR": "…"}}
/// ```
///
/// A `protocol_version` this rlxd does not serve gets the error `incompatible_protocol`,
/// whose `detail.supported` lists the versions it does.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "HelloParams")]
#[non_exhaustive]
pub struct HelloParams {
    /// The contents of `<home>/rlxd/token`.
    #[schemars(length(min = 1))]
    pub token: String,
    /// The protocol version the client speaks. It is the `schema_version` of every
    /// envelope and event on the connection.
    #[schemars(range(min = 1))]
    pub protocol_version: u32,
    /// Which client this is, such as `rlxapp`, `rlx` or `remote`.
    #[schemars(length(min = 1))]
    pub client: String,
    /// The client's own version.
    pub client_version: String,
    /// What the client connects as. rlxd counts its clients by role.
    pub role: Role,
    /// What the client can do, by name. rlxd ignores names it does not know.
    pub capabilities: Vec<String>,
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

/// `hello`'s reply: the protocol version the connection speaks, what this rlxd is, and
/// what it can do.
///
/// ```json
/// {"protocol_version": 1, "protocol_versions": [1], "rlxd_version": "0.10.3", "capabilities": []}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "HelloData")]
#[non_exhaustive]
pub struct HelloData {
    /// The protocol version this connection speaks from here on.
    #[schemars(range(min = 1))]
    pub protocol_version: u32,
    /// Every protocol version this rlxd serves.
    #[schemars(length(min = 1), inner(range(min = 1)))]
    pub protocol_versions: Vec<u32>,
    /// rlxd's version, which is rlx's.
    #[schemars(length(min = 1))]
    pub rlxd_version: String,
    /// What this rlxd can do, by name. A client ignores names it does not know.
    pub capabilities: Vec<String>,
}

impl HelloParams {
    /// A `hello` speaking this crate's [`PROTOCOL_VERSION`], with no capabilities and no
    /// `env`.
    pub fn new(
        token: impl Into<String>,
        client: impl Into<String>,
        client_version: impl Into<String>,
        role: Role,
    ) -> Self {
        Self {
            token: token.into(),
            protocol_version: PROTOCOL_VERSION,
            client: client.into(),
            client_version: client_version.into(),
            role,
            capabilities: Vec::new(),
            env: None,
        }
    }

    /// This `hello`, saying what the client can do.
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: Vec<String>) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// This `hello`, carrying `env` for rlxd to keep.
    #[must_use]
    pub fn with_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.env = Some(env);
        self
    }
}

impl HelloData {
    /// A reply agreeing on `protocol_version`, and listing every version this crate
    /// serves ([`PROTOCOL_VERSIONS`]).
    pub fn new(
        protocol_version: u32,
        rlxd_version: impl Into<String>,
        capabilities: Vec<String>,
    ) -> Self {
        Self {
            protocol_version,
            protocol_versions: PROTOCOL_VERSIONS.to_vec(),
            rlxd_version: rlxd_version.into(),
            capabilities,
        }
    }
}
