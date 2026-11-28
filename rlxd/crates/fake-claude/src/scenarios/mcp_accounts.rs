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
//! - Anything else, `mcp_call` included, is refused, and logged as every control request
//!   is, under `control_requests`.
//!
//! `-old` refuses `mcp_status` too, as a Claude Code without it would.

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
