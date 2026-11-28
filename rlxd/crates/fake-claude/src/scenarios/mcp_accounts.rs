//! `mcp-accounts`: Claude Code on the control channel of a no-turn read of one account's
//! connectors (task #14), whose `mcp_status` lists what that account's config dir holds,
//! so each account home answers its own. It never waits for a user message and writes no
//! `system/init`: it answers rlxd's control requests.
//!
//! - `initialize`: its `capabilities`, as `mcp-direct`'s.
//! - `mcp_status`: `mcpServers: []` for the first [`EARLY_MS`] (the claude.ai list not
//!   fetched yet), then the rows `<config dir>/fake-connectors.json` holds, read at each
//!   ask (`CLAUDE_CONFIG_DIR`, else `~/.claude`): each `{name, status, source?, tools?}`,
//!   as Claude Code lists them, so a test changes a status by writing the file. No file
//!   is no connector.
//! - `mcp_call` (an identity read, task #14's step 2) of a tool a row lists: logged under
//!   `mcp_calls` (`{tool, arguments}`). With `check_permissions: true` and
//!   `--permission-prompt-tool stdio` it asks rlxd one `can_use_tool` first, and runs the
//!   tool only on an allow; it gives the tool's `result` from the row (a content list), or
//!   an empty one. Each tool run is logged under `tool_runs_of`.
//! - Anything else is refused. Every control request is logged under `control_requests`.
//!
//! `-old` refuses `mcp_status` and `mcp_call` too, as a Claude Code without them would.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::mcp_direct::EARLY_MS;
use crate::Fake;

/// The file in the config dir that holds the account's connectors.
pub const FILE: &str = "fake-connectors.json";

/// How long it waits for rlxd's next request before it ends.
const WAIT: Duration = Duration::from_secs(120);

/// Plays one read's start: refusing `mcp_status` too when `old`.
pub fn play(fake: &mut Fake, old: bool) -> i32 {
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
        if request["subtype"] == "mcp_call" && !old {
            asks += 1;
            if !call(fake, &id, &request, asks) {
                fake.mark_finished();
                return 0;
            }
            continue;
        }
        let response = match request["subtype"].as_str().unwrap_or_default() {
            "initialize" => Ok(json!({"commands": [], "models": [],
                "capabilities": ["mcp_call_check_permissions_v1",
                    "mcp_call_result_not_for_model_v1", "mcp_call_soft_error_result_v1"]})),
            "mcp_status" if !old => {
                let early = started.elapsed() < Duration::from_millis(EARLY_MS);
                Ok(json!({"mcpServers": if early { json!([]) } else { rows() }}))
            }
            other => Err(format!("fake-claude plays no {other}")),
        };
        let response = match response {
            Ok(response) => json!({"subtype": "success", "request_id": id, "response": response}),
            Err(error) => json!({"subtype": "error", "request_id": id, "error": error}),
        };
        fake.emit(&json!({"type": "control_response", "response": response}));
    }
}

/// What the account's file lists: no connector without one.
fn rows() -> Value {
    crate::config_dir()
        .and_then(|dir| std::fs::read_to_string(dir.join(FILE)).ok())
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .filter(Value::is_array)
        .unwrap_or_else(|| json!([]))
}

/// Plays one `mcp_call`: whether stdin stayed open throughout.
fn call(fake: &mut Fake, id: &Value, request: &Value, n: u32) -> bool {
    let tool = request["tool"].as_str().unwrap_or_default().to_owned();
    let arguments = request["arguments"].clone();
    fake.log_push("mcp_calls", json!({"tool": tool, "arguments": arguments}));
    let Some((server, result)) = tool_of(&tool) else {
        error(fake, id, &format!("No such tool available: {tool}"));
        return true;
    };
    let gated = request["check_permissions"] == true
        && fake.value("--permission-prompt-tool") == Some("stdio");
    if gated {
        let ask_id = format!("fake-ask-{n}");
        let display = tool.rsplit("__").next().unwrap_or_default().to_owned();
        fake.emit(&json!({"type": "control_request", "request_id": ask_id,
            "request": {"subtype": "can_use_tool", "tool_name": tool, "input": arguments,
                "mcp_server": {"name": server, "source": "claudeai"},
                "display_name": display, "tool_use_id": format!("toolu_mcp_call_{n:024x}"),
                "server_policy_ask_only": false, "suppress_always_allow_rule": true}}));
        let answer = loop {
            let Some(message) = fake.next_control_within(WAIT).ok().flatten() else {
                return false;
            };
            if message["type"] == "control_response" && message["response"]["request_id"] == ask_id
            {
                break message["response"].clone();
            }
        };
        fake.log_push("control_answers", answer.clone());
        if answer["subtype"] != "success" || answer["response"]["behavior"] != "allow" {
            let why = answer["response"]["message"].as_str().unwrap_or("denied");
            error(fake, id, &format!("The tool did not run: {why}"));
            return true;
        }
    }
    fake.ran_tool();
    fake.log_push("tool_runs_of", json!(tool));
    fake.emit(
        &json!({"type": "control_response", "response": {"subtype": "success",
        "request_id": id, "response": {"content": result}}}),
    );
    true
}

/// The server named for `tool`, as Claude Code names its tools, and the tool's `result`.
fn tool_of(tool: &str) -> Option<(String, Value)> {
    let rows = rows();
    for row in rows.as_array()? {
        let name = row["name"].as_str()?;
        for listed in row["tools"].as_array().into_iter().flatten() {
            let Some(each) = listed["name"].as_str() else {
                continue;
            };
            if tool == format!("mcp__{}__{each}", server_name(name)) {
                let result = listed.get("result").cloned().unwrap_or_else(|| json!([]));
                return Some((name.to_owned(), result));
            }
        }
    }
    None
}

/// A server's name as Claude Code writes it in its tools' names: each character but a
/// letter, a digit, `_` or `-` as `_`, and for a claude.ai connector runs of `_` as one
/// and none at the ends.
fn server_name(name: &str) -> String {
    let written: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if !name.starts_with("claude.ai ") {
        return written;
    }
    let mut collapsed = String::new();
    for c in written.chars() {
        if c == '_' && collapsed.ends_with('_') {
            continue;
        }
        collapsed.push(c);
    }
    collapsed.trim_matches('_').to_owned()
}

/// Writes an error `control_response` to the request `id`.
fn error(fake: &mut Fake, id: &Value, error: &str) {
    fake.emit(
        &json!({"type": "control_response", "response": {"subtype": "error",
        "request_id": id, "error": error}}),
    );
}
