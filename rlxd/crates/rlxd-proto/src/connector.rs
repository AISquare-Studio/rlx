//! Connectors (handoff 0.10.3, 2.4, 5.3's listing, 5.5 and 7.7): the user's Claude Code
//! connectors, the claude.ai MCP servers, with their auth state and their tools.
//!
//! - `connectors.list {refresh?, allow_turn?}` ([`ConnectorsListParams`]) gives
//!   [`ConnectorsList`]: the connectors of the latest `system/init` rlxd saw, from a run
//!   or a refresh. A plain list never makes a model turn. `refresh: true` needs a route
//!   with no turn (capability [`capabilities::CONNECTORS_NO_TURN`]), and until there is
//!   one it is `no_route`, unless `allow_turn: true` lets rlxd run a one-turn probe.
//! - `connectors.call {run_id, connector, tool, input}` ([`ConnectorsCallParams`]) makes
//!   one read call through Claude Code's MCP client with no model turn (capability
//!   [`capabilities::CONNECTORS_CALL`]), as a gated `watch` run, and gives
//!   [`ConnectorsCallData`]; a denial is `denied`, and with no route it is `no_route`.
//! - `connectors.login {id}` ([`ConnectorsLoginParams`]) gives `{login_id}`
//!   ([`ConnectorsLoginData`]), and the login's steps come as `connector.login.*` events.
//! - The `connector` topic carries [`connector_types`]: a connector added, removed, or
//!   in another status, from any run's `system/init` or a refresh, and the login's steps.

use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::Value;

use crate::check::{self, Invalid, drop_null, present};
use crate::envelope::Data;
use crate::{Decision, RunId};

/// The capabilities `status` and `hello` announce for connectors.
pub mod capabilities {
    /// Claude Code reports connector states before the first user message, so a run's
    /// try waits for them before it sends the prompt (5.5, route 1). Without it, a try
    /// settles, sends, and reads the `system/init` that follows (route 2).
    pub const CONNECTORS_GATE_BEFORE_TURN: &str = "connectors.gate_before_turn";
    /// A refresh of the list with no model turn (5.3).
    pub const CONNECTORS_NO_TURN: &str = "connectors.no_turn";
    /// One read call with no model turn, `connectors.call` (5.3, phase 2).
    pub const CONNECTORS_CALL: &str = "connectors.call";
}

/// The types of the `connector` topic (7.7).
pub mod connector_types {
    /// `{id}` ([`ConnectorId`](crate::ConnectorId)): a connector rlxd had not seen.
    pub const ADDED: &str = "connector.added";
    /// `{id}`: a connector the latest `system/init` no longer lists.
    pub const REMOVED: &str = "connector.removed";
    /// `{id, status, previous_status}` ([`ConnectorChanged`](crate::ConnectorChanged)):
    /// a connector in another status than the last `system/init` showed. Once a change:
    /// the same status again is no change.
    pub const CHANGED: &str = "connector.changed";
    /// `{login_id, id, url}`: the login's URL, to open in a browser.
    pub const LOGIN_OPEN_URL: &str = "connector.login.open_url";
    /// `{login_id}`: the login waits for the browser.
    pub const LOGIN_WAITING: &str = "connector.login.waiting";
    /// `{login_id, id, status}`: the login ended, the connector in `status`.
    pub const LOGIN_DONE: &str = "connector.login.done";
    /// `{login_id, code, message}`: the login failed.
    pub const LOGIN_ERROR: &str = "connector.login.error";
}

/// The most bytes a connector's `id` has.
pub const MAX_ID: usize = 256;

/// A connector's status, as Claude Code's `system/init` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ConnectorState {
    /// Connected: its tools are mounted.
    Connected,
    /// Its sign-in has lapsed: `connectors.login` signs in again.
    NeedsAuth,
    /// Still connecting.
    Pending,
    /// It could not connect.
    Failed,
}

impl ConnectorState {
    /// The status Claude Code names `status`: one it doesn't know is `failed`, as it can't
    /// be used.
    pub fn of(status: &str) -> Self {
        match status {
            "connected" => Self::Connected,
            "needs-auth" | "needs_auth" => Self::NeedsAuth,
            "pending" | "connecting" => Self::Pending,
            _ => Self::Failed,
        }
    }

    /// As the wire says it: `connected`, `needs-auth`, `pending` or `failed`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::NeedsAuth => "needs-auth",
            Self::Pending => "pending",
            Self::Failed => "failed",
        }
    }
}

impl std::fmt::Display for ConnectorState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One of a connector's tools.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(transform = all_required)]
#[non_exhaustive]
pub struct ConnectorTool {
    /// Its name, as a run's tool calls name it, such as
    /// `mcp__claude_ai_Slack__slack_search_public`.
    #[schemars(length(min = 1))]
    pub name: String,
    /// The tool's MCP `readOnlyHint`: `true`, `false`, or `null` when Claude Code doesn't
    /// say. Always present.
    pub read_only_hint: Option<bool>,
}

impl ConnectorTool {
    /// The tool `name`, read-only as `read_only_hint` says.
    pub fn new(name: impl Into<String>, read_only_hint: Option<bool>) -> Self {
        Self {
            name: name.into(),
            read_only_hint,
        }
    }
}

/// One connector in `connectors.list`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct Connector {
    /// Its name, as runs name it in `needs_connectors` and mounts: `claude.ai Slack`.
    #[schemars(length(min = 1, max = 256))]
    pub id: String,
    /// Where it comes from: `claude.ai` for a claude.ai connector, else `mcp`.
    #[schemars(length(min = 1))]
    pub source: String,
    /// Its status.
    pub status: ConnectorState,
    /// Its tools, as the `system/init` mounted them.
    pub tools: Vec<ConnectorTool>,
}

impl Connector {
    /// The connector `id`, from `source`, in `status`, with `tools`.
    pub fn new(
        id: impl Into<String>,
        source: impl Into<String>,
        status: ConnectorState,
        tools: Vec<ConnectorTool>,
    ) -> Self {
        Self {
            id: id.into(),
            source: source.into(),
            status,
            tools,
        }
    }
}

/// Where a list came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ListedFrom {
    /// A `system/init`: a run's, or a one-turn refresh's.
    Init,
    /// A listing with no model turn (capability `connectors.no_turn`).
    Listing,
}

/// `connectors.list`'s reply (`connectors.json`).
///
/// ```json
/// {"connectors": [{"id": "claude.ai Slack", "source": "claude.ai", "status": "connected",
///   "tools": [{"name": "mcp__claude_ai_Slack__slack_search_public", "read_only_hint": true}]}],
///  "as_of": "2026-11-27T09:00:00.000Z", "from": "init", "model_turns": 0}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "ConnectorsList", transform = all_required)]
#[non_exhaustive]
pub struct ConnectorsList {
    /// The connectors, by `id`. Empty until a run or a refresh has shown any.
    pub connectors: Vec<Connector>,
    /// When the list's `system/init` came, or `null` before any did.
    #[schemars(extend("format" = "date-time"))]
    pub as_of: Option<String>,
    /// Where the list came from.
    pub from: ListedFrom,
    /// The model turns this call made: 0 for a plain list, 1 for a one-turn refresh.
    #[schemars(range(max = 1))]
    pub model_turns: u32,
}

impl ConnectorsList {
    /// `connectors`, as of `as_of`, from `from`, having made `model_turns` turns.
    pub fn new(
        connectors: Vec<Connector>,
        as_of: Option<String>,
        from: ListedFrom,
        model_turns: u32,
    ) -> Self {
        Self {
            connectors,
            as_of,
            from,
            model_turns,
        }
    }
}

/// `connectors.list`'s params: `{refresh?, allow_turn?}`, both `false` when absent.
#[derive(Clone, Debug, Default, PartialEq, Eq, JsonSchema)]
#[schemars(title = "ConnectorsListParams")]
#[non_exhaustive]
pub struct ConnectorsListParams {
    /// Read the connectors from Claude Code now, rather than the latest `system/init`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub refresh: Option<bool>,
    /// With `refresh`, and no route without a turn: run a one-turn probe for it. For
    /// `app`, `cli` and `tui`; `remote` gets `role_not_allowed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub allow_turn: Option<bool>,
}

impl ConnectorsListParams {
    /// Whether to refresh.
    pub fn refresh(&self) -> bool {
        self.refresh.unwrap_or(false)
    }

    /// Whether a refresh may make a model turn.
    pub fn allow_turn(&self) -> bool {
        self.allow_turn.unwrap_or(false)
    }

    /// Whether these params keep every rule of `connectors-list.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "ConnectorsListParams")]
struct UncheckedList {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    refresh: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    allow_turn: Option<bool>,
}

/// `connectors.login`'s params: `{id}`, the connector to sign in again.
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(title = "ConnectorsLoginParams")]
#[non_exhaustive]
pub struct ConnectorsLoginParams {
    /// The connector, such as `claude.ai Slack`. Never one that begins with `-`, which
    /// `claude mcp login` would read as an option.
    #[schemars(length(min = 1, max = 256), regex(pattern = r"^[^-]"))]
    pub id: String,
}

impl ConnectorsLoginParams {
    /// A login of the connector `id`.
    pub fn new(id: impl Into<String>) -> Result<Self, Invalid> {
        let params = Self { id: id.into() };
        params.validate()?;
        Ok(params)
    }

    /// Whether these params keep every rule of `connectors-login.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        check_id("id", &self.id)?;
        if self.id.starts_with('-') {
            return Err(Invalid::new(
                "id",
                "begins with '-', which `claude mcp login` would read as an option",
            ));
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "ConnectorsLoginParams")]
struct UncheckedLogin {
    id: String,
}

/// Every property required, `null` ones included: a field that may be `null` is still
/// always there.
fn all_required(schema: &mut Schema) {
    let names: Vec<Value> = schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| properties.keys().cloned().map(Value::String).collect())
        .unwrap_or_default();
    schema.insert("required".to_owned(), Value::Array(names));
}

/// A connector's `id`: 1 to 256 bytes, with no control character.
pub fn check_id(field: &str, id: &str) -> Result<(), Invalid> {
    check::non_empty(field, id)?;
    check::at_most(field, id.len() as u64, MAX_ID as u64)?;
    if id.chars().any(char::is_control) {
        return Err(Invalid::new(field, "holds a control character"));
    }
    Ok(())
}

macro_rules! checked_serde {
    ($type:ident, $unchecked:ident) => {
        impl Serialize for $type {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                self.validate().map_err(serde::ser::Error::custom)?;
                $unchecked::serialize(self, serializer)
            }
        }

        /// Reads an object only, as the schema does.
        impl<'de> Deserialize<'de> for $type {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let object = Data::deserialize(deserializer)?;
                let value =
                    $unchecked::deserialize(Value::Object(object)).map_err(de::Error::custom)?;
                value.validate().map_err(de::Error::custom)?;
                Ok(value)
            }
        }
    };
}

/// The longest tool name `connectors.call` takes, in bytes.
pub const MAX_TOOL: usize = 512;

/// `connectors.call`'s params (5.3): `{run_id, connector, tool, input}`, one call of the
/// connector's `tool` with `input`, made as the `watch` run `run_id` with no model turn.
#[derive(Clone, Debug, PartialEq, JsonSchema)]
#[schemars(title = "ConnectorsCallParams")]
#[non_exhaustive]
pub struct ConnectorsCallParams {
    /// The run the call is made as: a new lower-case UUID, the app's to pick.
    pub run_id: RunId,
    /// The connector, such as `claude.ai Slack`: the run's mount.
    #[schemars(length(min = 1, max = 256))]
    pub connector: String,
    /// The tool, as Claude Code names it: `mcp__<connector as Claude Code writes it>__…`.
    #[schemars(length(min = 6, max = 512), regex(pattern = r"^mcp__"))]
    pub tool: String,
    /// The tool's input: the run's pinned call is its RFC 8785 SHA-256, and it is the very
    /// input the call is made with.
    pub input: serde_json::Map<String, Value>,
}

impl ConnectorsCallParams {
    /// A call of `connector`'s `tool` with `input`, as the run `run_id`.
    pub fn new(
        run_id: RunId,
        connector: impl Into<String>,
        tool: impl Into<String>,
        input: serde_json::Map<String, Value>,
    ) -> Result<Self, Invalid> {
        let params = Self {
            run_id,
            connector: connector.into(),
            tool: tool.into(),
            input,
        };
        params.validate()?;
        Ok(params)
    }

    /// Whether these params keep every rule of `connectors-call.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        check_id("connector", &self.connector)?;
        check::non_empty("tool", &self.tool)?;
        check::at_most("tool", self.tool.len() as u64, MAX_TOOL as u64)?;
        if !self.tool.starts_with("mcp__") || self.tool.len() < 6 {
            return Err(Invalid::new("tool", "is not an MCP tool's name (mcp__…)"));
        }
        if self.tool.chars().any(char::is_control) {
            return Err(Invalid::new("tool", "holds a control character"));
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "ConnectorsCallParams")]
struct UncheckedCall {
    run_id: RunId,
    connector: String,
    tool: String,
    input: serde_json::Map<String, Value>,
}

/// `connectors.call`'s reply (5.3): `{run_id, decision, content?, is_error?, model_turns}`.
/// A denied call is an error (`denied`, with `detail.by`), never this reply, so `decision`
/// is `allow`; `content` is the tool's result exactly as Claude Code gave it, up to 1 MiB,
/// and `is_error` whether the tool said it failed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "ConnectorsCallData")]
#[non_exhaustive]
pub struct ConnectorsCallData {
    /// The call's run.
    pub run_id: RunId,
    /// The gate's decision.
    pub decision: Decision,
    /// The tool's result content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub content: Option<Value>,
    /// Whether the tool said it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub is_error: Option<bool>,
    /// Always 0: the call is made with no model turn.
    #[schemars(range(max = 0))]
    pub model_turns: u32,
}

impl ConnectorsCallData {
    /// Run `run_id`'s call, decided `decision`, which gave `content` (`is_error` when the
    /// tool said it failed).
    pub fn new(
        run_id: RunId,
        decision: Decision,
        content: Option<Value>,
        is_error: Option<bool>,
    ) -> Self {
        Self {
            run_id,
            decision,
            content,
            is_error,
            model_turns: 0,
        }
    }
}

checked_serde!(ConnectorsListParams, UncheckedList);
checked_serde!(ConnectorsLoginParams, UncheckedLogin);
checked_serde!(ConnectorsCallParams, UncheckedCall);

/// `connectors.login`'s reply: `{login_id}`, which its events carry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "ConnectorsLoginData")]
#[non_exhaustive]
pub struct ConnectorsLoginData {
    /// The login, as its `connector.login.*` events name it.
    #[schemars(length(min = 1))]
    pub login_id: String,
}

impl ConnectorsLoginData {
    /// The login `login_id`.
    pub fn new(login_id: impl Into<String>) -> Self {
        Self {
            login_id: login_id.into(),
        }
    }
}

/// `connector.added` and `connector.removed`: `{id}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "ConnectorId")]
#[non_exhaustive]
pub struct ConnectorId {
    /// The connector.
    #[schemars(length(min = 1))]
    pub id: String,
}

impl ConnectorId {
    /// The connector `id`.
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }
}

/// `connector.changed`: `{id, status, previous_status}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "ConnectorChanged")]
#[non_exhaustive]
pub struct ConnectorChanged {
    /// The connector.
    #[schemars(length(min = 1))]
    pub id: String,
    /// Its status now.
    pub status: ConnectorState,
    /// Its status before.
    pub previous_status: ConnectorState,
}

impl ConnectorChanged {
    /// `id` is now `status`, after `previous_status`.
    pub fn new(
        id: impl Into<String>,
        status: ConnectorState,
        previous_status: ConnectorState,
    ) -> Self {
        Self {
            id: id.into(),
            status,
            previous_status,
        }
    }
}

/// `connector.login.open_url`: `{login_id, id, url}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "ConnectorLoginOpenUrl")]
#[non_exhaustive]
pub struct ConnectorLoginOpenUrl {
    /// The login.
    #[schemars(length(min = 1))]
    pub login_id: String,
    /// The connector.
    #[schemars(length(min = 1))]
    pub id: String,
    /// The URL to open, always `https://`.
    #[schemars(regex(pattern = "^https://"))]
    pub url: String,
}

impl ConnectorLoginOpenUrl {
    /// Login `login_id` of `id` asks for `url` to be opened.
    pub fn new(login_id: impl Into<String>, id: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            login_id: login_id.into(),
            id: id.into(),
            url: url.into(),
        }
    }
}

/// `connector.login.waiting`: `{login_id}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "ConnectorLoginWaiting")]
#[non_exhaustive]
pub struct ConnectorLoginWaiting {
    /// The login.
    #[schemars(length(min = 1))]
    pub login_id: String,
}

impl ConnectorLoginWaiting {
    /// Login `login_id` waits.
    pub fn new(login_id: impl Into<String>) -> Self {
        Self {
            login_id: login_id.into(),
        }
    }
}

/// `connector.login.done`: `{login_id, id, status}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "ConnectorLoginDone")]
#[non_exhaustive]
pub struct ConnectorLoginDone {
    /// The login.
    #[schemars(length(min = 1))]
    pub login_id: String,
    /// The connector.
    #[schemars(length(min = 1))]
    pub id: String,
    /// Its status after the login.
    pub status: ConnectorState,
}

impl ConnectorLoginDone {
    /// Login `login_id` of `id` ended with it in `status`.
    pub fn new(login_id: impl Into<String>, id: impl Into<String>, status: ConnectorState) -> Self {
        Self {
            login_id: login_id.into(),
            id: id.into(),
            status,
        }
    }
}

/// Why a login failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LoginErrorCode {
    /// Claude Code was not found.
    ClaudeNotFound,
    /// It printed no URL to open.
    NoUrl,
    /// The URL it printed is not `https://`: rlxd doesn't pass it on.
    NotHttps,
    /// It ended without signing in.
    Failed,
    /// It took longer than rlxd waits.
    TimedOut,
}

/// `connector.login.error`: `{login_id, code, message}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "ConnectorLoginError")]
#[non_exhaustive]
pub struct ConnectorLoginError {
    /// The login.
    #[schemars(length(min = 1))]
    pub login_id: String,
    /// Why it failed.
    pub code: LoginErrorCode,
    /// What happened, for a person.
    pub message: String,
}

impl ConnectorLoginError {
    /// Login `login_id` failed for `code`, saying `message`.
    pub fn new(
        login_id: impl Into<String>,
        code: LoginErrorCode,
        message: impl Into<String>,
    ) -> Self {
        Self {
            login_id: login_id.into(),
            code,
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_login_id_is_a_connector_id_that_is_no_option() {
        assert!(ConnectorsLoginParams::new("claude.ai Slack").is_ok());
        for id in ["", "-x", "--help", "a\nb", &"a".repeat(MAX_ID + 1)] {
            assert!(ConnectorsLoginParams::new(id).is_err(), "{id:?}");
        }
        let read: Result<ConnectorsLoginParams, _> =
            serde_json::from_value(serde_json::json!({"id": "--help"}));
        assert!(read.is_err());
    }
}
