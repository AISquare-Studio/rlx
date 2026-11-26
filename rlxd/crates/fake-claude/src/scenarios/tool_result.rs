//! `tool-result`: a recorded connector call and its `tool_result` block, whose content is
//! `tool-result/content.json` exactly, then an answer. It replays the call's stream-json
//! only: it runs no tool and asks no hook (the gate's own scenarios are 5.1's).

use serde_json::{Value, json};

use crate::{Fake, MODEL, uuid};

/// The call's `tool_use_id`, the run event's `call_id`.
pub const CALL_ID: &str = "toolu_01FakeToolResult0000000001";

/// The tool it calls.
pub const TOOL: &str = "mcp__claude_ai_Slack__slack_search_public";

/// The result's content, exactly as Claude Code gave it.
pub const CONTENT: &str =
    include_str!("../../../../tests/fixtures/fake-claude/tool-result/content.json");

pub fn run(fake: &mut Fake) -> i32 {
    if fake.begin().is_none() {
        return super::NO_PROMPT;
    }
    let content: Value = serde_json::from_str(CONTENT).expect("content.json is JSON");
    let session = fake.session_id().to_owned();
    let call = json!({"type": "assistant", "message": {
        "id": "msg_fake_tool_call", "type": "message", "role": "assistant", "model": MODEL,
        "content": [{"type": "tool_use", "id": CALL_ID, "name": TOOL,
            "input": {"query": "standup"}}],
        "stop_reason": "tool_use", "usage": {"input_tokens": 20, "output_tokens": 9}},
        "parent_tool_use_id": null, "session_id": session, "uuid": uuid()});
    fake.remember(&call);
    fake.emit(&call);
    let result = json!({"type": "user", "message": {"role": "user", "content": [
        {"tool_use_id": CALL_ID, "type": "tool_result", "content": content,
            "is_error": false}]},
        "parent_tool_use_id": null, "session_id": session, "uuid": uuid(),
        "tool_use_result": {"fake": "Claude Code's own copy, which rlxd does not read"}});
    fake.remember(&result);
    fake.emit(&result);
    fake.say("Standup is at 10.");
    fake.succeed("Standup is at 10.", None);
    0
}
