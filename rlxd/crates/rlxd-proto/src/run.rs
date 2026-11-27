//! The runner (handoff 0.10.3, 2.1, 5.4 and 7.5): `run.start`, `run.cancel`, and the
//! data of every run event, `run-event-list.json`.
//!
//! A run is one headless Claude Code task that rlxd starts, streams and keeps a durable
//! log of. The app picks its `run_id`, a UUID that is also Claude Code's session id.
//! Every event of a run is on the `run` topic and carries `run_id`, `run_seq`, `kind`,
//! `attempt` and `job_id?` ([`RunScope`](crate::RunScope)); the types here are the rest
//! of each event's `data`.

use std::borrow::Cow;
use std::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::{Value, json};

use crate::Data;
use crate::check::{self, Invalid, drop_null, present};
use crate::event::Event;

/// The run event types of 7.5 that this version describes. A client ignores types it
/// does not know.
pub mod run_types {
    /// Over its kind's pool, a run waits: `{position}`.
    pub const QUEUED: &str = "run.queued";
    /// Claude Code started: `system/init`'s facts.
    pub const STARTED: &str = "run.started";
    /// Assistant text, or a partial chunk of it.
    pub const TEXT: &str = "run.text";
    /// A tool's result, as Claude Code gave it.
    pub const TOOL_RESULT: &str = "run.tool_result";
    /// Claude Code was rate limited, or warned it soon will be.
    pub const RATE_LIMITED: &str = "run.rate_limited";
    /// The run ended needing the user.
    pub const NEEDS_YOU: &str = "run.needs_you";
    /// The run ended with a result.
    pub const DONE: &str = "run.done";
    /// The run ended without one.
    pub const FAILED: &str = "run.failed";
    /// On the `needs_you` topic: something needs the user.
    pub const NEEDS_YOU_RAISED: &str = "needs_you.raised";
    /// On the `needs_you` topic: it no longer does.
    pub const NEEDS_YOU_CLEARED: &str = "needs_you.cleared";
}

/// A run's id: a UUID, lower-case and hyphenated, as `uuidgen | tr A-Z a-z` prints one.
/// The app picks it, and rlxd passes it to Claude Code as `--session-id`. It names the
/// run's folder too, so it takes no other form: upper case would name the same folder
/// twice on a file system that ignores case.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RunId(String);

impl RunId {
    /// The pattern a run id matches.
    pub const PATTERN: &str = "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$";

    /// `text` as a run id, or why it is not one.
    pub fn new(text: impl Into<String>) -> Result<Self, Invalid> {
        let text = text.into();
        let groups: Vec<&str> = text.split('-').collect();
        let shaped = groups.iter().map(|g| g.len()).eq([8, 4, 4, 4, 12])
            && groups.iter().all(|g| {
                g.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            });
        if !shaped {
            return Err(Invalid::new(
                "run_id",
                format!(
                    "must be a UUID in lower case, such as \
                     0f8e2d6c-3b1a-4c5d-9e7f-1a2b3c4d5e6f, not {text:?}"
                ),
            ));
        }
        Ok(Self(text))
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for RunId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RunId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

impl JsonSchema for RunId {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("RunId")
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "A UUID in lower case: the run's id, and Claude Code's session id.",
            "type": "string",
            "pattern": Self::PATTERN,
        })
    }
}

/// What a run is for (7.5). Each kind has its own pool, and rlxd's gate rules for it
/// (5.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    /// A task the user asked for. The only kind whose session persists by default.
    Task,
    /// Sends one pinned call. Never resumed: a send is never retried.
    SendExecutor,
    /// Refreshes a widget's data.
    WidgetRefresh,
    /// One pinned read call for a scheduled watch.
    Watch,
    /// rlxapp's send-time `Check`: no tools.
    Check,
    /// Parses an instruction: no tools.
    ParseInstruction,
    /// Writes a widget: no tools.
    WidgetAuthor,
}

impl RunKind {
    /// Every kind.
    pub const ALL: [Self; 7] = [
        Self::Task,
        Self::SendExecutor,
        Self::WidgetRefresh,
        Self::Watch,
        Self::Check,
        Self::ParseInstruction,
        Self::WidgetAuthor,
    ];

    /// The kind as the wire writes it, such as `widget_refresh`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::SendExecutor => "send_executor",
            Self::WidgetRefresh => "widget_refresh",
            Self::Watch => "watch",
            Self::Check => "check",
            Self::ParseInstruction => "parse_instruction",
            Self::WidgetAuthor => "widget_author",
        }
    }

    /// How many runs of this kind run at once; more wait as `run.queued`.
    pub fn pool(self) -> usize {
        match self {
            Self::WidgetRefresh | Self::Watch => 1,
            _ => 2,
        }
    }

    /// Whether its session persists when `run.start` doesn't say: only a task's.
    pub fn persists_by_default(self) -> bool {
        self == Self::Task
    }

    /// Whether it takes only `mount: {kind: "no_tools"}`.
    pub fn needs_no_tools(self) -> bool {
        matches!(
            self,
            Self::Check | Self::ParseInstruction | Self::WidgetAuthor
        )
    }

    /// Whether it needs `pinned_call`.
    pub fn needs_pinned_call(self) -> bool {
        matches!(self, Self::SendExecutor | Self::Watch)
    }
}

impl fmt::Display for RunKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a run stands (7.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// Waiting for a place in its kind's pool.
    Queued,
    /// Its Claude Code is running.
    Running,
    /// It ended needing the user; `run.resume` goes on with it.
    NeedsYou,
    /// It ended with a result.
    Done,
    /// It ended without one: `run.failed`'s `reason` says why.
    Failed,
}

impl RunState {
    /// The state as the wire writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::NeedsYou => "needs_you",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    /// Whether the run has a process, or is waiting for one: queued or running.
    pub fn is_live(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}

impl fmt::Display for RunState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which tools a run may reach (7.5).
///
/// - `{"kind": "no_tools"}`: none. rlxd passes `--tools "" --strict-mcp-config`, so
///   neither built-in tools nor connectors are mounted.
/// - `{"kind": "connectors", "connectors": ["claude.ai Slack"]}`: built-in tools are
///   dropped (`--tools ""`); the gate (5.1) decides which connector tools may be called.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Mount {
    /// No tools at all.
    NoTools,
    /// Only these connectors' tools, each call decided by the gate.
    Connectors {
        /// The connectors, by the name `system/init` gives them, such as `claude.ai
        /// Slack`.
        connectors: Vec<String>,
    },
}

/// The one call a `send_executor` or `watch` run may make (5.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PinnedCall {
    /// The tool, such as `mcp__claude_ai_Slack__slack_send_message`.
    #[schemars(length(min = 1))]
    pub tool: String,
    /// The SHA-256, in lower-case hex, of the call's input as RFC 8785 canonical JSON.
    #[schemars(regex(pattern = r"^[0-9a-f]{64}$"))]
    pub input_sha256: String,
}

/// When a finished run is `needs_you` rather than `done` (7.5): its final output, read at
/// `pointer`, equals `equals`. The question is read at `question_pointer`. The output is
/// `structured_output` when the run has one, else the result's text read as JSON.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NeedsYouWhen {
    /// A JSON pointer (RFC 6901) into the output, such as `/status`.
    #[schemars(regex(pattern = r"^(/.*)?$"))]
    pub pointer: String,
    /// The value there that means the run needs the user.
    pub equals: Value,
    /// A JSON pointer to the question to show, such as `/question`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, regex(pattern = r"^(/.*)?$"))]
    pub question_pointer: Option<String>,
}

/// `run.start`'s params (7.5). It returns `{run_id}` (`run-ref.json`).
///
/// ```json
/// {"run_id": "0f8e2d6c-3b1a-4c5d-9e7f-1a2b3c4d5e6f", "kind": "task", "prompt": "…",
///  "mount": {"kind": "no_tools"}, "persist_session": true, "space": "default",
///  "partial": true, "timeout_s": 900, "deadline_ms": 120000}
/// ```
///
/// rlxd refuses, by error code:
/// - `run_exists`: a `run_id` it already knows;
/// - `kind_mount_mismatch`: a `check`, `parse_instruction` or `widget_author` run whose
///   mount is not `no_tools`;
/// - `pinned_call_required`: a `send_executor` or `watch` run without `pinned_call`;
/// - `invalid_params`: anything else these rules or the schema refuse.
#[derive(Clone, Debug, PartialEq, JsonSchema)]
#[schemars(title = "RunStartParams")]
#[non_exhaustive]
pub struct RunStartParams {
    /// The run's id, which the app picks: a lower-case UUID, also Claude Code's session
    /// id.
    pub run_id: RunId,
    /// What the run is for.
    pub kind: RunKind,
    /// The prompt, sent as the run's first stream-json user message.
    #[serde(default)]
    pub prompt: String,
    /// Which tools the run may reach.
    pub mount: Mount,
    /// The connectors that must be `connected` before the first turn (5.5): rlxd makes
    /// up to 3 tries, and ends the run `run.failed {reason: "connectors"}` when they run
    /// out, or at once when one is `needs-auth` or `failed`.
    #[serde(default)]
    pub needs_connectors: Vec<String>,
    /// A JSON Schema the final output must match (`--json-schema`); `run.done` then
    /// carries `structured_output`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub output_schema: Option<Data>,
    /// Whether Claude Code keeps the session, so the run can be resumed. Absent: `true`
    /// for a `task`, `false` for every other kind (`--no-session-persistence`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub persist_session: Option<bool>,
    /// The space the run works in (`space.ensure`). Absent: `default`.
    #[serde(default = "default_space")]
    #[schemars(
        regex(pattern = r"^[a-z0-9][a-z0-9_-]*(\.[a-z0-9_-]+)*$"),
        length(max = 64)
    )]
    pub space: String,
    /// Text appended to Claude Code's system prompt (`--append-system-prompt`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub append_system_prompt: Option<String>,
    /// Stream partial text as it comes (`--include-partial-messages`): `run.text` events
    /// with `partial: true`.
    #[serde(default)]
    pub partial: bool,
    /// End the run `failed {reason: "timeout"}` once its process has run this many
    /// seconds. Absent: no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, range(min = 1, max = 604_800))]
    pub timeout_s: Option<u64>,
    /// How long the gate waits for the app's answer to each tool call, in milliseconds
    /// (5.1). The hook's own timeout is this plus 10 s.
    #[serde(default = "default_deadline_ms")]
    #[schemars(range(min = 1, max = 3_600_000))]
    pub deadline_ms: u64,
    /// The one call a `send_executor` or `watch` run may make. `null` is none.
    #[serde(default)]
    pub pinned_call: Option<PinnedCall>,
    /// When the finished run is `needs_you` rather than `done`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub needs_you_when: Option<NeedsYouWhen>,
}

/// The serde code of `RunStartParams`'s fields, unchecked. It is private, so the only way
/// in or out of the type is its own `Serialize` and `Deserialize`, which run `validate()`.
#[derive(Serialize, Deserialize)]
#[serde(remote = "RunStartParams")]
struct UncheckedRunStart {
    run_id: RunId,
    kind: RunKind,
    #[serde(default)]
    prompt: String,
    mount: Mount,
    #[serde(default)]
    needs_connectors: Vec<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    output_schema: Option<Data>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    persist_session: Option<bool>,
    #[serde(default = "default_space")]
    space: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    append_system_prompt: Option<String>,
    #[serde(default)]
    partial: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    timeout_s: Option<u64>,
    #[serde(default = "default_deadline_ms")]
    deadline_ms: u64,
    #[serde(default)]
    pinned_call: Option<PinnedCall>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    needs_you_when: Option<NeedsYouWhen>,
}

fn default_space() -> String {
    crate::space::DEFAULT_SPACE.to_owned()
}

/// The gate's default deadline for one tool call: two minutes (5.1).
pub const DEFAULT_DEADLINE_MS: u64 = 120_000;

fn default_deadline_ms() -> u64 {
    DEFAULT_DEADLINE_MS
}

impl RunStartParams {
    /// A `kind` run `run_id` of `prompt`, in the default space, with no tools.
    pub fn new(run_id: RunId, kind: RunKind, prompt: impl Into<String>) -> Self {
        Self {
            run_id,
            kind,
            prompt: prompt.into(),
            mount: Mount::NoTools,
            needs_connectors: Vec::new(),
            output_schema: None,
            persist_session: None,
            space: default_space(),
            append_system_prompt: None,
            partial: false,
            timeout_s: None,
            deadline_ms: DEFAULT_DEADLINE_MS,
            pinned_call: None,
            needs_you_when: None,
        }
    }

    /// Whether the session persists: `persist_session`, else the kind's default.
    pub fn persists(&self) -> bool {
        self.persist_session
            .unwrap_or_else(|| self.kind.persists_by_default())
    }

    /// Whether these params keep every rule of `run-start.json`. The rules with error
    /// codes of their own (`kind_mount_mismatch`, `pinned_call_required`) are rlxd's to
    /// check.
    pub fn validate(&self) -> Result<(), Invalid> {
        crate::space::check_name("space", &self.space)?;
        if let Mount::Connectors { connectors } = &self.mount {
            for connector in connectors {
                check::non_empty("mount.connectors", connector)?;
            }
        }
        for connector in &self.needs_connectors {
            check::non_empty("needs_connectors", connector)?;
        }
        if let Some(timeout) = self.timeout_s {
            check::at_least("timeout_s", timeout, 1)?;
            check::at_most("timeout_s", timeout, 604_800)?;
        }
        check::at_least("deadline_ms", self.deadline_ms, 1)?;
        check::at_most("deadline_ms", self.deadline_ms, 3_600_000)?;
        if let Some(pinned) = &self.pinned_call {
            check::non_empty("pinned_call.tool", &pinned.tool)?;
            let hex = pinned.input_sha256.len() == 64
                && pinned
                    .input_sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            if !hex {
                return Err(Invalid::new(
                    "pinned_call.input_sha256",
                    "must be 64 lower-case hex digits",
                ));
            }
        }
        if let Some(when) = &self.needs_you_when {
            pointer("needs_you_when.pointer", &when.pointer)?;
            if let Some(question) = &when.question_pointer {
                pointer("needs_you_when.question_pointer", question)?;
            }
        }
        Ok(())
    }
}

/// A JSON pointer: empty, or starting with `/`.
fn pointer(field: &str, value: &str) -> Result<(), Invalid> {
    if value.is_empty() || value.starts_with('/') {
        return Ok(());
    }
    Err(Invalid::new(
        field,
        format!("must be a JSON pointer, empty or starting with '/', not {value:?}"),
    ))
}

impl Serialize for RunStartParams {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(serde::ser::Error::custom)?;
        UncheckedRunStart::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for RunStartParams {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let object = Data::deserialize(deserializer)?;
        let params =
            UncheckedRunStart::deserialize(Value::Object(object)).map_err(de::Error::custom)?;
        params.validate().map_err(de::Error::custom)?;
        Ok(params)
    }
}

/// One run, by id: `run.start`'s reply, and the params of `run.cancel` and `run.get`.
///
/// ```json
/// {"run_id": "0f8e2d6c-3b1a-4c5d-9e7f-1a2b3c4d5e6f"}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "RunRef")]
#[non_exhaustive]
pub struct RunRef {
    /// The run's id.
    pub run_id: RunId,
}

impl RunRef {
    /// The run `run_id`.
    pub fn new(run_id: RunId) -> Self {
        Self { run_id }
    }
}

/// `run.cancel`'s reply. A queued or running run ends `failed {reason: "cancelled"}`,
/// its process tree killed and its partial text kept, and so does a `needs_you` one. A
/// run that had already ended is left as it was: `cancelled` is `false`, and `state` is
/// how it ended.
///
/// ```json
/// {"run_id": "0f8e2d6c-3b1a-4c5d-9e7f-1a2b3c4d5e6f", "cancelled": true, "state": "failed"}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "RunCancelData")]
#[non_exhaustive]
pub struct RunCancelData {
    /// The run's id.
    pub run_id: RunId,
    /// Whether this call ended the run.
    pub cancelled: bool,
    /// The run's state now.
    pub state: RunState,
}

impl RunCancelData {
    /// The reply for `run_id`, now in `state`.
    pub fn new(run_id: RunId, cancelled: bool, state: RunState) -> Self {
        Self {
            run_id,
            cancelled,
            state,
        }
    }
}

// --- run event data --------------------------------------------------------------------

/// `run.queued`: over its kind's pool, the run waits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct RunQueuedData {
    /// Its place in its kind's queue, from 1: the next to start is 1.
    #[schemars(range(min = 1))]
    pub position: u32,
}

impl RunQueuedData {
    /// Waiting at `position`.
    pub fn new(position: u32) -> Self {
        Self { position }
    }
}

/// One MCP server `system/init` lists, with its status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct McpServer {
    /// Its name, such as `claude.ai Slack`.
    pub name: String,
    /// Its status, such as `connected`, `pending`, `needs-auth` or `failed`.
    pub status: String,
}

impl McpServer {
    /// The server `name`, in `status`.
    pub fn new(name: impl Into<String>, status: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: status.into(),
        }
    }
}

/// `run.started`: Claude Code's stream-json `system/init`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct RunStartedData {
    /// The working directory Claude Code reports: the run's space.
    pub cwd: String,
    /// The model.
    pub model: String,
    /// The tools mounted.
    pub tools: Vec<String>,
    /// The MCP servers, connectors included, with their status.
    pub mcp_servers: Vec<McpServer>,
    /// Claude Code's version.
    pub claude_version: String,
    /// Whether this attempt resumed the run (`run.resume`).
    pub resumed: bool,
}

impl RunStartedData {
    /// Claude Code `claude_version` started in `cwd` with `model`, no tools mounted, as a
    /// first attempt.
    pub fn new(
        cwd: impl Into<String>,
        model: impl Into<String>,
        claude_version: impl Into<String>,
    ) -> Self {
        Self {
            cwd: cwd.into(),
            model: model.into(),
            tools: Vec::new(),
            mcp_servers: Vec::new(),
            claude_version: claude_version.into(),
            resumed: false,
        }
    }

    /// This start, with `tools` and `mcp_servers` mounted.
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<String>, mcp_servers: Vec<McpServer>) -> Self {
        self.tools = tools;
        self.mcp_servers = mcp_servers;
        self
    }

    /// This start, saying whether it resumed the run.
    #[must_use]
    pub fn resumed(mut self, resumed: bool) -> Self {
        self.resumed = resumed;
        self
    }
}

/// `run.text`: assistant text. With `partial`, a chunk as it streams; the whole message
/// follows as a `run.text` with `partial: false`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct RunTextData {
    /// The text.
    pub text: String,
    /// Whether it is a partial chunk.
    pub partial: bool,
}

impl RunTextData {
    /// `text`, a partial chunk or not.
    pub fn new(text: impl Into<String>, partial: bool) -> Self {
        Self {
            text: text.into(),
            partial,
        }
    }
}

/// `run.tool_result`: a `tool_result` block in a stream-json `user` message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct RunToolResultData {
    /// Claude Code's `tool_use_id`, which the call's other events share.
    pub call_id: String,
    /// The tool called, from the matching `tool_use` block; empty when none was seen.
    pub tool: String,
    /// Whether the tool failed, or the call was denied.
    pub is_error: bool,
    /// The result's content, exactly as Claude Code gave it, up to 1 MiB of JSON; past
    /// that, its first 1 MiB as a string.
    pub content: Value,
    /// Whether `content` was cut.
    pub truncated: bool,
}

impl RunToolResultData {
    /// Call `call_id`'s result from `tool`, whole.
    pub fn new(
        call_id: impl Into<String>,
        tool: impl Into<String>,
        is_error: bool,
        content: Value,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            tool: tool.into(),
            is_error,
            content,
            truncated: false,
        }
    }

    /// This result, saying whether its content was cut.
    #[must_use]
    pub fn truncated(mut self, truncated: bool) -> Self {
        self.truncated = truncated;
        self
    }
}

/// `run.rate_limited`: a `rate_limit_event`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct RunRateLimitedData {
    /// When the limit resets, RFC 3339 in UTC, when Claude Code said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, extend("format" = "date-time"))]
    pub resets_at: Option<String>,
    /// Claude Code's own word for it, such as `allowed_warning` or `rejected`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub status: Option<String>,
}

impl RunRateLimitedData {
    /// A limit that resets at `resets_at`, in `status`.
    pub fn new(resets_at: Option<String>, status: Option<String>) -> Self {
        Self { resets_at, status }
    }
}

/// Why a run needs the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NeedsYouReason {
    /// Its output asked a question (`needs_you_when`).
    Question,
    /// The app denied a call with `stop: true` (5.1).
    Denied,
}

/// `run.needs_you`: the run ended needing the user.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct RunNeedsYouData {
    /// Why.
    pub reason: NeedsYouReason,
    /// The question, read at `needs_you_when.question_pointer`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub question: Option<String>,
    /// The denied call, for `reason: "denied"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub call_id: Option<String>,
    /// For a run a scheduler job fired: the job's `loud`, opaque to rlxd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub loud: Option<bool>,
}

impl RunNeedsYouData {
    /// A run whose output asked the user `question`.
    pub fn question(question: Option<String>) -> Self {
        Self {
            reason: NeedsYouReason::Question,
            question,
            call_id: None,
            loud: None,
        }
    }
}

/// Tokens a run used.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct Usage {
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
}

impl Usage {
    /// `input_tokens` in, `output_tokens` out.
    pub fn new(input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            input_tokens,
            output_tokens,
        }
    }
}

/// `run.done`: the run ended with a result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct RunDoneData {
    /// The session to resume: the run's id, or `null` for a session-less run.
    pub session: Option<String>,
    /// The result's text.
    pub result: String,
    /// The output matching `output_schema`, when the run had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub structured_output: Option<Value>,
    /// Tokens used.
    pub usage: Usage,
    /// What it cost, in US dollars, when Claude Code said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub cost_usd: Option<f64>,
    /// How long it took, as Claude Code counts it.
    pub duration_ms: u64,
    /// How many turns it took.
    pub num_turns: u64,
    /// For a run a scheduler job fired: the job's `loud`, opaque to rlxd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub loud: Option<bool>,
}

impl RunDoneData {
    /// A result `result`, of the session `session`, that used `usage`.
    pub fn new(session: Option<String>, result: impl Into<String>, usage: Usage) -> Self {
        Self {
            session,
            result: result.into(),
            structured_output: None,
            usage,
            cost_usd: None,
            duration_ms: 0,
            num_turns: 0,
            loud: None,
        }
    }

    /// This result, which took `duration_ms` and `num_turns`.
    #[must_use]
    pub fn with_turns(mut self, duration_ms: u64, num_turns: u64) -> Self {
        self.duration_ms = duration_ms;
        self.num_turns = num_turns;
        self
    }
}

/// Why a run failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FailReason {
    /// `run.cancel`.
    Cancelled,
    /// Claude Code failed, crashed or could not start; `message` says how.
    Error,
    /// `timeout_s` ran out.
    Timeout,
    /// Claude Code was rate limited.
    RateLimited,
    /// rlxd stopped or crashed while the run was in flight; `run.resume` goes on with it.
    EngineRestarted,
    /// A connector in `needs_connectors` never connected (5.5).
    Connectors,
    /// A `connectors.call` was denied (5.3).
    Denied,
}

impl FailReason {
    /// The reason as the wire writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Error => "error",
            Self::Timeout => "timeout",
            Self::RateLimited => "rate_limited",
            Self::EngineRestarted => "engine_restarted",
            Self::Connectors => "connectors",
            Self::Denied => "denied",
        }
    }
}

/// A connector named in `needs_connectors`, and its status (5.5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct ConnectorStatus {
    /// The connector.
    pub id: String,
    /// Its status: `pending`, `needs-auth`, `failed` or `absent`.
    pub status: String,
}

impl ConnectorStatus {
    /// The connector `id`, in `status`.
    pub fn new(id: impl Into<String>, status: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            status: status.into(),
        }
    }
}

/// `run.failed`: the run ended without a result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct RunFailedData {
    /// Why.
    pub reason: FailReason,
    /// What happened, for a person.
    pub message: String,
    /// The assistant text the run got out before it ended.
    pub partial_text: String,
    /// For `reason: "connectors"`: each named connector that was not connected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub connectors: Option<Vec<ConnectorStatus>>,
}

impl RunFailedData {
    /// A failure for `reason`, saying `message`, after `partial_text`.
    pub fn new(
        reason: FailReason,
        message: impl Into<String>,
        partial_text: impl Into<String>,
    ) -> Self {
        Self {
            reason,
            message: message.into(),
            partial_text: partial_text.into(),
            connectors: None,
        }
    }

    /// This failure, naming `connectors`, each not connected, with its status.
    #[must_use]
    pub fn with_connectors(mut self, connectors: Vec<ConnectorStatus>) -> Self {
        self.connectors = Some(connectors);
        self
    }
}

// --- needs_you, from runs (7.3) ---------------------------------------------------------

/// Where something that needs the user comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NeedsYouSource {
    /// A run (`run_id`).
    Run,
    /// A fleet agent (`agent_id`), after the fleet moves onto rlxd (1.8).
    Fleet,
}

/// `needs_you.raised`, on the `needs_you` topic: something needs the user. A run raises
/// one with its `run.needs_you`, and `needs_you.cleared` with the same `id` follows when
/// it is resumed or cancelled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct NeedsYouRaisedData {
    /// What `needs_you.cleared` names: `run:<run_id>:<attempt>` for a run.
    #[schemars(length(min = 1))]
    pub id: String,
    /// Where it comes from.
    pub source: NeedsYouSource,
    /// The run, for `source: "run"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub run_id: Option<String>,
    /// The agent, for `source: "fleet"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub agent_id: Option<String>,
    /// Why: for a run, `run.needs_you`'s `reason`.
    pub reason: String,
    /// What to show the user: the question, when there is one.
    pub text: String,
}

impl NeedsYouRaisedData {
    /// A run's: `id`, for `run_id`, needing the user for `reason`, saying `text`.
    pub fn run(
        id: impl Into<String>,
        run_id: impl Into<String>,
        reason: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            source: NeedsYouSource::Run,
            run_id: Some(run_id.into()),
            agent_id: None,
            reason: reason.into(),
            text: text.into(),
        }
    }
}

/// `needs_you.cleared`, on the `needs_you` topic: what `needs_you.raised` named no longer
/// needs the user.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct NeedsYouClearedData {
    /// The `id` it was raised with.
    #[schemars(length(min = 1))]
    pub id: String,
}

impl NeedsYouClearedData {
    /// Clears `id`.
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }
}

// --- run.resume, run.get, run.list, run.events (5.4, 7.5) -------------------------------

/// `run.resume`'s params: go on with a run that has no live process, as a new attempt
/// with the same `run_id` (`--resume <run_id>`). `reply`, when given, is the new
/// attempt's first user message; without it, Claude Code is told to continue.
///
/// ```json
/// {"run_id": "0f8e2d6c-3b1a-4c5d-9e7f-1a2b3c4d5e6f", "reply": "#dev"}
/// ```
///
/// It resumes a run that needs the user, is done, or failed (`engine_restarted`
/// included), and refuses: a queued or running run (`run_alive`), a session-less one
/// (`no_session`), and a `send_executor` (`not_resumable`): a send is never retried.
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(title = "RunResumeParams")]
#[non_exhaustive]
pub struct RunResumeParams {
    /// The run.
    pub run_id: RunId,
    /// The user's reply, sent as the new attempt's first message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub reply: Option<String>,
}

/// The serde code of `RunResumeParams`'s fields, unchecked: see `UncheckedRunStart`.
#[derive(Serialize, Deserialize)]
#[serde(remote = "RunResumeParams")]
struct UncheckedRunResume {
    run_id: RunId,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    reply: Option<String>,
}

impl RunResumeParams {
    /// Resumes `run_id`, with `reply` when given.
    pub fn new(run_id: RunId, reply: Option<String>) -> Self {
        Self { run_id, reply }
    }
}

impl Serialize for RunResumeParams {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        UncheckedRunResume::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for RunResumeParams {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let object = Data::deserialize(deserializer)?;
        UncheckedRunResume::deserialize(Value::Object(object)).map_err(de::Error::custom)
    }
}

/// `run.resume`'s reply: the run, and the attempt it is now on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "RunResumeData")]
#[non_exhaustive]
pub struct RunResumeData {
    /// The run.
    pub run_id: RunId,
    /// Its new attempt, from 2.
    #[schemars(range(min = 2))]
    pub attempt: u64,
}

impl RunResumeData {
    /// `run_id`, now on `attempt`.
    pub fn new(run_id: RunId, attempt: u64) -> Self {
        Self { run_id, attempt }
    }
}

/// `run.list`'s params: which runs, of every run rlxd knows, live or ended, from before
/// an app or rlxd restart too.
///
/// ```json
/// {"states": ["running", "queued"], "job_id": "j1", "limit": 50}
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, JsonSchema)]
#[schemars(title = "RunListParams")]
#[non_exhaustive]
pub struct RunListParams {
    /// Only runs in these states. Absent: every state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub states: Option<Vec<RunState>>,
    /// Only the runs this scheduler job fired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, length(min = 1))]
    pub job_id: Option<String>,
    /// At most this many, the latest first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, range(min = 1))]
    pub limit: Option<u32>,
}

/// The serde code of `RunListParams`'s fields, unchecked: see `UncheckedRunStart`.
#[derive(Serialize, Deserialize)]
#[serde(remote = "RunListParams")]
struct UncheckedRunList {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    states: Option<Vec<RunState>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    job_id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    limit: Option<u32>,
}

impl RunListParams {
    /// Whether these params keep every rule of `run-list.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        if let Some(job_id) = &self.job_id {
            check::non_empty("job_id", job_id)?;
        }
        if let Some(limit) = self.limit {
            check::at_least("limit", u64::from(limit), 1)?;
        }
        Ok(())
    }
}

impl Serialize for RunListParams {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(serde::ser::Error::custom)?;
        UncheckedRunList::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for RunListParams {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let object = Data::deserialize(deserializer)?;
        let params =
            UncheckedRunList::deserialize(Value::Object(object)).map_err(de::Error::custom)?;
        params.validate().map_err(de::Error::custom)?;
        Ok(params)
    }
}

/// One run in `run.list`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct RunSummary {
    /// The run.
    pub run_id: RunId,
    /// Its kind.
    pub kind: RunKind,
    /// Its state now.
    pub state: RunState,
    /// When `run.start` took it.
    #[schemars(extend("format" = "date-time"))]
    pub started_at: String,
    /// When it last ended, if it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, extend("format" = "date-time"))]
    pub ended_at: Option<String>,
    /// Its last event's `run_seq`, 0 before its first.
    pub last_run_seq: u64,
    /// The scheduler job that fired it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub job_id: Option<String>,
    /// The `client` of the connection that started it.
    pub client: String,
    /// Its attempt, from 1.
    #[schemars(range(min = 1))]
    pub attempt: u64,
    /// Its space.
    pub space: String,
}

impl RunSummary {
    /// The `kind` run `run_id`, in `state`, started at `started_at`: on its first
    /// attempt, in the default space, with no events, and no client named.
    pub fn new(
        run_id: RunId,
        kind: RunKind,
        state: RunState,
        started_at: impl Into<String>,
    ) -> Self {
        Self {
            run_id,
            kind,
            state,
            started_at: started_at.into(),
            ended_at: None,
            last_run_seq: 0,
            job_id: None,
            client: String::new(),
            attempt: 1,
            space: crate::space::DEFAULT_SPACE.to_owned(),
        }
    }
}

/// `run.list`'s reply: the runs, the latest started first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "RunListData")]
#[non_exhaustive]
pub struct RunListData {
    /// The runs.
    pub runs: Vec<RunSummary>,
}

impl RunListData {
    /// The list of `runs`.
    pub fn new(runs: Vec<RunSummary>) -> Self {
        Self { runs }
    }
}

/// A gated call's ledger state (5.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CallState {
    /// Decided, and not allowed to run.
    Decided,
    /// Allowed, and running.
    Executing,
    /// It ran and succeeded: sent.
    Ok,
    /// It ran and failed: not sent.
    Error,
    /// It was running when its run ended: it may have sent.
    Unknown,
}

/// One gated call of a run (5.1): what `run.get` tells the app about a send.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct RunCall {
    /// Claude Code's `tool_use_id`.
    pub call_id: String,
    /// The tool.
    pub tool: String,
    /// The SHA-256 of the call's input as RFC 8785 JSON.
    pub input_sha256: String,
    /// `allow`, `deny` or `draft`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub decision: Option<String>,
    /// Who decided: `app`, `rlxd`, `timeout` or `unreachable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub by: Option<String>,
    /// Where it stands.
    pub state: CallState,
}

/// `run.get`'s reply: one run, durably: its state, structured output and end survive an
/// rlxd restart.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "RunGetData")]
#[non_exhaustive]
pub struct RunGetData {
    /// The run.
    pub run_id: RunId,
    /// Its kind.
    pub kind: RunKind,
    /// Its state now.
    pub state: RunState,
    /// Its attempt, from 1.
    #[schemars(range(min = 1))]
    pub attempt: u64,
    /// Its space.
    pub space: String,
    /// The `client` of the connection that started it.
    pub client: String,
    /// The scheduler job that fired it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub job_id: Option<String>,
    /// When `run.start` took it.
    #[schemars(extend("format" = "date-time"))]
    pub started_at: String,
    /// When it last ended, if it has: `finished_at` too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, extend("format" = "date-time"))]
    pub ended_at: Option<String>,
    /// When it last ended, if it has (the hub's name for `ended_at`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, extend("format" = "date-time"))]
    pub finished_at: Option<String>,
    /// Its last event's `run_seq`, 0 before its first.
    pub last_run_seq: u64,
    /// Its last event, as the stream carried it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub last_event: Option<Event>,
    /// The session to resume, or `null` for a session-less run or one not done yet.
    pub session: Option<String>,
    /// Its structured output, once it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub structured_output: Option<Value>,
    /// Every gated call's ledger record (5.1).
    pub calls: Vec<RunCall>,
}

impl RunGetData {
    /// The `kind` run `run_id`, in `state`, started at `started_at`: on its first
    /// attempt, in the default space, with no events, no session and no calls.
    pub fn new(
        run_id: RunId,
        kind: RunKind,
        state: RunState,
        started_at: impl Into<String>,
    ) -> Self {
        Self {
            run_id,
            kind,
            state,
            attempt: 1,
            space: crate::space::DEFAULT_SPACE.to_owned(),
            client: String::new(),
            job_id: None,
            started_at: started_at.into(),
            ended_at: None,
            finished_at: None,
            last_run_seq: 0,
            last_event: None,
            session: None,
            structured_output: None,
            calls: Vec::new(),
        }
    }
}

/// `run.events`'s params: replay a run's durable log from `from_run_seq`, then, for a
/// live run, follow it until it ends. The stream is event frames, then the reply
/// `{run_id, last_run_seq}`, which ends it.
///
/// ```json
/// {"run_id": "0f8e2d6c-3b1a-4c5d-9e7f-1a2b3c4d5e6f", "from_run_seq": 2}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(title = "RunEventsParams")]
#[non_exhaustive]
pub struct RunEventsParams {
    /// The run.
    pub run_id: RunId,
    /// The first `run_seq` to send. Absent: 1, the run's first event.
    #[serde(default = "first_run_seq")]
    #[schemars(range(min = 1))]
    pub from_run_seq: u64,
}

fn first_run_seq() -> u64 {
    1
}

/// The serde code of `RunEventsParams`'s fields, unchecked: see `UncheckedRunStart`.
#[derive(Serialize, Deserialize)]
#[serde(remote = "RunEventsParams")]
struct UncheckedRunEvents {
    run_id: RunId,
    #[serde(default = "first_run_seq")]
    from_run_seq: u64,
}

impl RunEventsParams {
    /// `run_id`'s events from `from_run_seq`.
    pub fn new(run_id: RunId, from_run_seq: u64) -> Self {
        Self {
            run_id,
            from_run_seq,
        }
    }

    /// Whether these params keep every rule of `run-events.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        check::at_least("from_run_seq", self.from_run_seq, 1)
    }
}

impl Serialize for RunEventsParams {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(serde::ser::Error::custom)?;
        UncheckedRunEvents::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for RunEventsParams {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let object = Data::deserialize(deserializer)?;
        let params =
            UncheckedRunEvents::deserialize(Value::Object(object)).map_err(de::Error::custom)?;
        params.validate().map_err(de::Error::custom)?;
        Ok(params)
    }
}

/// `run.events`'s reply, which ends its stream: the last `run_seq` it sent, or would
/// have.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "RunEventsData")]
#[non_exhaustive]
pub struct RunEventsData {
    /// The run.
    pub run_id: RunId,
    /// The run's last `run_seq` when the stream ended.
    pub last_run_seq: u64,
}

impl RunEventsData {
    /// The end of `run_id`'s stream, at `last_run_seq`.
    pub fn new(run_id: RunId, last_run_seq: u64) -> Self {
        Self {
            run_id,
            last_run_seq,
        }
    }
}

/// An event of a run's stream, as `run.events` and a capture of the `run` and
/// `needs_you` topics give them: `event.json`, plus 7.5's `data` for each run event type
/// this version describes. `run-event-list.json` is an array of them. A type it does not
/// describe needs only what `event.json` asks, so a newer rlxd's events still validate.
#[derive(Clone, Debug, PartialEq)]
pub struct RunEvent(pub Event);

impl JsonSchema for RunEvent {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("RunEvent")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let typed = [
            (
                run_types::QUEUED,
                generator.subschema_for::<RunQueuedData>(),
            ),
            (
                run_types::STARTED,
                generator.subschema_for::<RunStartedData>(),
            ),
            (run_types::TEXT, generator.subschema_for::<RunTextData>()),
            (
                run_types::TOOL_RESULT,
                generator.subschema_for::<RunToolResultData>(),
            ),
            (
                run_types::RATE_LIMITED,
                generator.subschema_for::<RunRateLimitedData>(),
            ),
            (
                run_types::NEEDS_YOU,
                generator.subschema_for::<RunNeedsYouData>(),
            ),
            (run_types::DONE, generator.subschema_for::<RunDoneData>()),
            (
                run_types::FAILED,
                generator.subschema_for::<RunFailedData>(),
            ),
            (
                run_types::NEEDS_YOU_RAISED,
                generator.subschema_for::<NeedsYouRaisedData>(),
            ),
            (
                run_types::NEEDS_YOU_CLEARED,
                generator.subschema_for::<NeedsYouClearedData>(),
            ),
        ];
        let mut rules = vec![Value::from(generator.subschema_for::<Event>())];
        rules.extend(typed.into_iter().map(|(r#type, data)| {
            json!({
                "if": {"properties": {"type": {"const": r#type}}, "required": ["type"]},
                "then": {"properties": {"data": data}},
            })
        }));
        json_schema!({
            "description": "One event of a run's stream: an event (event.json) whose data, \
                            for each run event type 7.5 describes, carries that type's \
                            fields beside the run fields.",
            "allOf": rules,
        })
    }
}
