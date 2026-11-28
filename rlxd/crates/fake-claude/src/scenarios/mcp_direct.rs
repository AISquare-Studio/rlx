//! `mcp-direct`: Claude Code on a `connectors.call` run's control channel (5.3), as spike
//! #5 found 2.1.332 (route a). It never waits for a user message and writes no
//! `system/init`: it answers rlxd's control requests.
//!
//! - `initialize`: its `capabilities` list `mcp_call_check_permissions_v1` and the two
//!   other `mcp_call_*` features.
//! - `mcp_status`: `mcpServers: []` for the first [`EARLY_MS`] (the claude.ai list not
//!   fetched yet), then [`CONNECTOR`] `connected` (`source: "claudeai"`) with two tools:
//!   [`READ`] (`readOnly: true`) and [`WRITE`] (`openWorld: true` only).
//! - `mcp_call` with `check_permissions: true`, and with `--permission-prompt-tool stdio`
//!   on its command line: one `can_use_tool` to rlxd with the call's `arguments` as its
//!   `input` and a `toolu_mcp_call_…` id. On an allow, the tool runs with the answer's
//!   `updatedInput` and gives [`CONTENT`] (an error result for `{"ref": "missing"}`); on a
//!   deny, the call's reply is an error, "The tool did not run: …". Without
//!   `check_permissions`, the tool runs unasked, as 2.1.332 does.
//!
//! Its variants play what rlxd must not trust: `-old` lists no
//! `mcp_call_check_permissions_v1`, `-unasked` ignores `check_permissions` (as an older
//! Claude Code would), `-silent` never answers `initialize`, `-huge` writes a line of
//! [`HUGE_BYTES`] after `initialize`, `-turn` writes an `assistant` line after it,
//! `-respelled` asks `can_use_tool` with the input's whole numbers written as `1.0` (the
//! same canonical input, other bytes), `-needs-auth` lists the connector `needs-auth`, and
//! `-hang` never answers the allowed call.
//!
//! Started as one of rlxd's conversations (with `--settings`, as a watch job's fallback
//! run is), each plays [`super::watch_call`]: so a fire that falls back has a model to make
//! its call.
//!
//! The log: every control request rlxd sent in `control_requests` (its `request`), every
//! answer rlxd gave a `can_use_tool` in `control_answers` (its `response`), and each input
//! a tool ran with in `tool_inputs` (with `tool_runs` counting them).

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::Fake;

/// The connector.
pub const CONNECTOR: &str = "claude.ai GitHub";

/// Its read tool, as Claude Code names it.
pub const READ: &str = "mcp__claude_ai_GitHub__get_check_runs";

/// Its write tool, as Claude Code names it.
pub const WRITE: &str = "mcp__claude_ai_GitHub__create_issue";

/// The read tool's content, exactly as Claude Code gives it.
pub const CONTENT: &str =
    include_str!("../../../../tests/fixtures/fake-claude/mcp-direct/content.json");

/// The tool's error for the input `{"ref": "missing"}`.
pub const MISSING: &str = "No commit found for the ref missing";

/// How long `mcp_status` lists no server after the start.
pub const EARLY_MS: u64 = 300;

/// The line `-huge` writes: past rlxd's 4 MiB for a control line.
pub const HUGE_BYTES: usize = 5 << 20;

/// What a variant does differently.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// Claude Code 2.1.332, as the spike found it.
    Gated,
    /// Its `initialize` lists no `mcp_call_check_permissions_v1`.
    Old,
    /// It ignores `check_permissions`, and runs the call unasked.
    Unasked,
    /// It never answers `initialize`.
    Silent,
    /// It writes a line of [`HUGE_BYTES`] after `initialize`.
    Huge,
    /// It writes an `assistant` line after `initialize`.
    Turn,
    /// It asks `can_use_tool` with the input's whole numbers written as floats.
    Respelled,
    /// It lists the connector `needs-auth`.
    NeedsAuth,
    /// It never answers the allowed call.
    Hang,
}

/// The longest it waits for rlxd's next message.
const WAIT: Duration = Duration::from_secs(120);

/// `mcp-direct`, as `variant` plays it.
pub fn play(fake: &mut Fake, variant: Variant) -> i32 {
    if fake.flag("--settings") {
        return super::watch_call::run(fake);
    }
    let started = Instant::now();
    let mut asks = 0_u32;
    loop {
        let message = match fake.next_control_within(WAIT) {
            Ok(Some(message)) => message,
            // stdin closed: rlxd is done with it.
            Ok(None) | Err(_) => {
                fake.mark_finished();
                return 0;
            }
        };
        if message["type"] != "control_request" {
            continue;
        }
        let id = message["request_id"].clone();
        let request = message["request"].clone();
        fake.log_push("control_requests", request.clone());
        match request["subtype"].as_str().unwrap_or_default() {
            "initialize" => {
                if variant == Variant::Silent {
                    continue;
                }
                let mut capabilities = vec![
                    "mcp_call_result_not_for_model_v1",
                    "mcp_call_soft_error_result_v1",
                ];
                if variant != Variant::Old {
                    capabilities.insert(0, "mcp_call_check_permissions_v1");
                }
                success(
                    fake,
                    &id,
                    json!({"commands": [], "output_style": "default",
                        "available_output_styles": ["default"], "models": [],
                        "account": {"email": "probe@example.com"},
                        "capabilities": capabilities, "fake_new_field": true}),
                );
                match variant {
                    Variant::Huge => {
                        let pad = "x".repeat(HUGE_BYTES);
                        fake.emit(&json!({"type": "fake_noise", "pad": pad}));
                    }
                    Variant::Turn => fake.emit(&json!({"type": "assistant", "message": {
                        "role": "assistant", "content": [{"type": "text", "text": "Hi"}]}})),
                    _ => {}
                }
            }
            "mcp_status" => {
                let early = started.elapsed() < Duration::from_millis(EARLY_MS);
                let servers = if early { json!([]) } else { servers(variant) };
                success(fake, &id, json!({"mcpServers": servers}));
            }
            "mcp_call" => {
                asks += 1;
                if !call(fake, &id, &request, variant, asks) {
                    // stdin closed mid-call.
                    fake.mark_finished();
                    return 0;
                }
            }
            other => {
                let error = format!("fake-claude plays no {other}");
                fake.emit(&json!({"type": "control_response", "response": {
                    "subtype": "error", "request_id": id, "error": error}}));
            }
        }
    }
}

/// The servers `mcp_status` lists once the claude.ai list is fetched.
fn servers(variant: Variant) -> Value {
    let status = if variant == Variant::NeedsAuth {
        "needs-auth"
    } else {
        "connected"
    };
    let tools = if status == "connected" {
        json!([
            {"name": "get_check_runs", "annotations": {"readOnly": true}},
            {"name": "create_issue", "annotations": {"openWorld": true}},
        ])
    } else {
        json!([])
    };
    json!([{"name": CONNECTOR, "status": status, "scope": "claudeai", "source": "claudeai",
        "transport": "claudeai-proxy", "config": {"type": "claudeai-proxy"},
        "serverInfo": {"name": "github", "version": "1.0.0"}, "sessionProxied": true,
        "tools": tools}])
}

/// Plays one `mcp_call`: whether stdin stayed open throughout.
fn call(fake: &mut Fake, id: &Value, request: &Value, variant: Variant, n: u32) -> bool {
    let tool = request["tool"].as_str().unwrap_or_default().to_owned();
    let arguments = request["arguments"].clone();
    if variant == Variant::NeedsAuth || !(tool == READ || tool == WRITE) {
        fake.emit(
            &json!({"type": "control_response", "response": {"subtype": "error",
            "request_id": id, "error": "MCP server not connected: claude_ai_GitHub"}}),
        );
        return true;
    }
    let gated = request["check_permissions"] == true
        && fake.value("--permission-prompt-tool") == Some("stdio")
        && variant != Variant::Unasked;
    if !gated {
        run_tool(fake, id, &arguments);
        return true;
    }
    let ask_id = format!("fake-ask-{n}");
    let asked = if variant == Variant::Respelled {
        respelled(&arguments)
    } else {
        arguments.clone()
    };
    let display = tool.rsplit("__").next().unwrap_or_default().to_owned();
    fake.emit(&json!({"type": "control_request", "request_id": ask_id,
        "request": {"subtype": "can_use_tool", "tool_name": tool, "input": asked,
            "mcp_server": {"name": CONNECTOR, "source": "claudeai"},
            "display_name": display, "tool_use_id": format!("toolu_mcp_call_{n:024x}"),
            "server_policy_ask_only": false, "suppress_always_allow_rule": true}}));
    // The answer to it: rlxd's other messages meanwhile are logged, and skipped.
    let answer = loop {
        let message = match fake.next_control_within(WAIT) {
            Ok(Some(message)) => message,
            Ok(None) | Err(_) => return false,
        };
        if message["type"] == "control_response" && message["response"]["request_id"] == ask_id {
            break message["response"].clone();
        }
        if message["type"] == "control_request" {
            fake.log_push("control_requests", message["request"].clone());
        }
    };
    fake.log_push("control_answers", answer.clone());
    let decided = &answer["response"];
    if answer["subtype"] == "success" && decided["behavior"] == "allow" {
        if variant == Variant::Hang {
            // Never answers: rlxd's wait for the result must end it.
            return !fake.stdin_closed_within(WAIT.as_millis() as u64);
        }
        let input = decided
            .get("updatedInput")
            .cloned()
            .unwrap_or_else(|| arguments.clone());
        run_tool(fake, id, &input);
    } else {
        let why = decided["message"].as_str().unwrap_or("denied");
        fake.emit(
            &json!({"type": "control_response", "response": {"subtype": "error",
            "request_id": id, "error": format!("The tool did not run: {why}")}}),
        );
    }
    true
}

/// Runs the tool with `input`, and answers the `mcp_call` `id` with its result.
fn run_tool(fake: &mut Fake, id: &Value, input: &Value) {
    fake.ran_tool();
    fake.log_push("tool_inputs", input.clone());
    let result = if input["ref"] == "missing" {
        json!({"content": [{"type": "text", "text": MISSING}], "isError": true})
    } else {
        let content: Value = serde_json::from_str(CONTENT).expect("content.json is JSON");
        json!({"content": content, "_meta": {"fake": true}})
    };
    success(fake, id, result);
}

/// Writes a successful `control_response` to the request `id`.
fn success(fake: &mut Fake, id: &Value, response: Value) {
    fake.emit(
        &json!({"type": "control_response", "response": {"subtype": "success",
        "request_id": id, "response": response}}),
    );
}

/// `value` with every whole number written as a float: `1` as `1.0`.
fn respelled(value: &Value) -> Value {
    match value {
        Value::Number(n) if n.is_u64() || n.is_i64() => n
            .as_f64()
            .and_then(serde_json::Number::from_f64)
            .map_or_else(|| value.clone(), Value::Number),
        Value::Array(items) => Value::Array(items.iter().map(respelled).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), respelled(item)))
                .collect(),
        ),
        other => other.clone(),
    }
}
