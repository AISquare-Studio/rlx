//! `hello`, the protocol handshake and the first frame on every connection (handoff
//! 0.10.3 rev 5, 7.2, 5.1 and 4.3).

use std::collections::BTreeMap;
use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::check::{self, Invalid, drop_null, present};
use crate::{PROTOCOL_VERSION, PROTOCOL_VERSIONS};

/// `hello`'s params: who is connecting, as what, which protocol it speaks, and what lets
/// it in.
///
/// ```json
/// {"token": "…", "protocol_version": 1, "client": "rlxapp", "client_version": "0.1.0",
///  "role": "app", "gate_credential": "…", "capabilities": [], "env": {"CLAUDE_CONFIG_DIR": "…"}}
/// ```
///
/// A `protocol_version` this rlxd does not serve gets the error `incompatible_protocol`,
/// whose `detail.supported` lists the versions it does. rlxd reads the version from the
/// raw params first (`protocol_version_of`), so a hello this type refuses, such as one
/// from a later protocol, still gets that answer. `role: app` also needs a valid
/// `gate_credential` and a peer that passes rlxd's identity check (only the credential,
/// in gate dev mode). Anything less gets the error `not_gate`, and a refused hello never
/// counts as an app client.
#[derive(Clone, PartialEq, Eq, JsonSchema)]
#[schemars(title = "HelloParams")]
#[non_exhaustive]
pub struct HelloParams {
    /// The shared client token: the contents of `<home>/rlxd/token`. It lets a client
    /// in, but does not make it the gate.
    #[schemars(length(min = 1))]
    pub token: String,
    /// The protocol version the client speaks. It is the `schema_version` of every
    /// envelope and event on the connection.
    #[schemars(range(min = 1))]
    pub protocol_version: u32,
    /// Which client this is, such as `rlxapp`, `rlx-tui` or `rlxd-hook`.
    #[schemars(length(min = 1))]
    pub client: String,
    /// The client's own version.
    pub client_version: String,
    /// What the client connects as. rlxd counts its clients by role.
    pub role: Role,
    /// The app's gate credential, separate from the shared token, which `gate.pair` mints
    /// for a peer that passes rlxd's identity check. A `role: app` hello needs it. Other
    /// roles have none to send.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, length(min = 1))]
    pub gate_credential: Option<String>,
    /// What the client can do, by name. rlxd ignores names it does not know.
    pub capabilities: Vec<String>,
    /// Variables rlxd keeps and passes to the brain and to every Claude Code child, such
    /// as `CLAUDE_CONFIG_DIR` and `CLAUDE_SECURESTORAGE_CONFIG_DIR`. Absent leaves what
    /// rlxd already has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub env: Option<BTreeMap<String, String>>,
}

/// HelloParams's fields as serde reads and writes them, unchecked. It is private, so the only
/// way in or out of a HelloParams is through HelloParams's own `Serialize` and `Deserialize`, which run
/// `validate()`. (`remote = "Self"` on HelloParams itself would make these unchecked functions
/// public, as `HelloParams::serialize` and `HelloParams::deserialize`.)
#[derive(Serialize, Deserialize)]
#[serde(remote = "HelloParams")]
struct UncheckedHello {
    token: String,
    protocol_version: u32,
    client: String,
    client_version: String,
    role: Role,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    gate_credential: Option<String>,
    capabilities: Vec<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    env: Option<BTreeMap<String, String>>,
}

/// What a client connects as. A later protocol may add roles, so a `match` on this
/// needs a wildcard arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Role {
    /// rlxapp, and only rlxapp: the tool-call gate. It needs a valid `gate_credential`
    /// and a peer that passes rlxd's identity check.
    App,
    /// rlx's terminal UI.
    Tui,
    /// rlx's Remote server.
    Remote,
    /// A command-line client, such as `rlxd call`, and `rlxd-hook`.
    Cli,
}

/// `hello`'s reply: the protocol version the connection speaks, what this rlxd is, and
/// what it can do.
///
/// ```json
/// {"protocol_version": 1, "protocol_versions": [1], "rlxd_version": "0.10.3", "capabilities": []}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
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

/// HelloData's fields as serde reads and writes them, unchecked. It is private, so the only
/// way in or out of a HelloData is through HelloData's own `Serialize` and `Deserialize`, which run
/// `validate()`. (`remote = "Self"` on HelloData itself would make these unchecked functions
/// public, as `HelloData::serialize` and `HelloData::deserialize`.)
#[derive(Serialize, Deserialize)]
#[serde(remote = "HelloData")]
struct UncheckedReply {
    protocol_version: u32,
    protocol_versions: Vec<u32>,
    rlxd_version: String,
    capabilities: Vec<String>,
}

impl HelloParams {
    /// A `hello` speaking this crate's [`PROTOCOL_VERSION`], with no capabilities, gate
    /// credential or `env`. Refuses an empty token or client.
    pub fn new(
        token: impl Into<String>,
        client: impl Into<String>,
        client_version: impl Into<String>,
        role: Role,
    ) -> Result<Self, Invalid> {
        let hello = Self {
            token: token.into(),
            protocol_version: PROTOCOL_VERSION,
            client: client.into(),
            client_version: client_version.into(),
            role,
            gate_credential: None,
            capabilities: Vec::new(),
            env: None,
        };
        hello.validate()?;
        Ok(hello)
    }

    /// This `hello`, carrying the app's gate credential, for `role: app`. Refuses an
    /// empty one.
    pub fn with_gate_credential(mut self, credential: impl Into<String>) -> Result<Self, Invalid> {
        self.gate_credential = Some(credential.into());
        self.validate()?;
        Ok(self)
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

    /// Whether this `hello` keeps every rule of `hello.json`. Whether rlxd lets it in,
    /// and as what, is rlxd's to decide.
    pub fn validate(&self) -> Result<(), Invalid> {
        check::non_empty("token", &self.token)?;
        check::at_least("protocol_version", u64::from(self.protocol_version), 1)?;
        check::non_empty("client", &self.client)?;
        if let Some(credential) = &self.gate_credential {
            check::non_empty("gate_credential", credential)?;
        }
        Ok(())
    }
}

impl Serialize for HelloParams {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(serde::ser::Error::custom)?;
        UncheckedHello::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for HelloParams {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let hello = UncheckedHello::deserialize(deserializer)?;
        hello.validate().map_err(de::Error::custom)?;
        Ok(hello)
    }
}

/// Shows every field but the two secrets, so a logged `hello` leaks neither.
impl fmt::Debug for HelloParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const REDACTED: &str = "<redacted>";
        f.debug_struct("HelloParams")
            .field("token", &REDACTED)
            .field("protocol_version", &self.protocol_version)
            .field("client", &self.client)
            .field("client_version", &self.client_version)
            .field("role", &self.role)
            .field(
                "gate_credential",
                &self.gate_credential.as_ref().map(|_| REDACTED),
            )
            .field("capabilities", &self.capabilities)
            .field("env", &self.env)
            .finish()
    }
}

impl HelloData {
    /// A reply agreeing on `protocol_version`, and listing every version this crate
    /// serves ([`PROTOCOL_VERSIONS`]). Refuses version 0 and an empty `rlxd_version`.
    pub fn new(
        protocol_version: u32,
        rlxd_version: impl Into<String>,
        capabilities: Vec<String>,
    ) -> Result<Self, Invalid> {
        let reply = Self {
            protocol_version,
            protocol_versions: PROTOCOL_VERSIONS.to_vec(),
            rlxd_version: rlxd_version.into(),
            capabilities,
        };
        reply.validate()?;
        Ok(reply)
    }

    /// Whether this reply keeps every rule of `hello-reply.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        check::at_least("protocol_version", u64::from(self.protocol_version), 1)?;
        if self.protocol_versions.is_empty() {
            return Err(Invalid::new("protocol_versions", "must not be empty"));
        }
        for version in &self.protocol_versions {
            check::at_least("protocol_versions", u64::from(*version), 1)?;
        }
        check::non_empty("rlxd_version", &self.rlxd_version)
    }
}

impl Serialize for HelloData {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(serde::ser::Error::custom)?;
        UncheckedReply::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for HelloData {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let reply = UncheckedReply::deserialize(deserializer)?;
        reply.validate().map_err(de::Error::custom)?;
        Ok(reply)
    }
}
