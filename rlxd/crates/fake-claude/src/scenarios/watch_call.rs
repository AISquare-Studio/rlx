//! `watch-call`: a watch job's run (handoff 0.10.3, 7.6), as a model would make it. It
//! reads the connector, the tool and the input from the JSON rlxd quotes in its message,
//! lists that tool (read-only) of that connector (from claude.ai) in `system/init`, and
//! makes the call through the run's PreToolUse hook. Allowed, the tool gives
//! `watch-call/content.json`; or, as a tool would for its input, an error for `{"ref":
//! "missing"}` ([`MISSING`]) and [`HUGE_BYTES`] of text for `{"ref": "huge"}`. Then it
//! tries the identical call again, which rlxd denies (`pinned_call_used`), and ends.

use serde_json::{Value, json};

use crate::{Fake, MODEL, uuid};

/// The call's `tool_use_id`.
pub const CALL_ID: &str = "toolu_01FakeWatchCall0000000001";

/// The second, identical call's.
pub const AGAIN_ID: &str = "toolu_01FakeWatchCall0000000002";

/// The tool's result, exactly as Claude Code gives it.
pub const CONTENT: &str =
    include_str!("../../../../tests/fixtures/fake-claude/watch-call/content.json");

/// The tool's error for the input `{"ref": "missing"}`.
pub const MISSING: &str = "No commit found for the ref missing";

/// How much text the tool gives for the input `{"ref": "huge"}`: more than a tool
/// result's 1 MiB.
pub const HUGE_BYTES: usize = 2 << 20;

/// What the tool gives for `input`, and whether it failed.
fn result_of(input: &Value) -> (Value, bool) {
    match input["ref"].as_str() {
        Some("missing") => (Value::String(MISSING.to_owned()), true),
        Some("huge") => (
            json!([{"type": "text", "text": "x".repeat(HUGE_BYTES)}]),
            false,
        ),
        _ => (
            serde_json::from_str(CONTENT).expect("content.json is JSON"),
            false,
        ),
    }
}

/// `watch-call`.
pub fn run(fake: &mut Fake) -> i32 {
    let Some(message) = fake.first_message() else {
        return super::NO_PROMPT;
    };
    let Some(call) = quoted(&Fake::text_of(&message)) else {
        fake.init(json!([]), json!([]));
        fake.say("There is no call to make.");
        fake.succeed("no call", None);
        return 0;
    };
    let connector = call["connector"].as_str().unwrap_or_default().to_owned();
    let tool = call["tool"].as_str().unwrap_or_default().to_owned();
    let input = call["input"].clone();
    fake.init(
        json!([{"name": tool, "annotations": {"readOnlyHint": true}}]),
        json!([{"name": connector, "status": "connected", "source": "claudeai"}]),
    );
    let (content, failed) = result_of(&input);
    let mut said = Vec::new();
    for id in [CALL_ID, AGAIN_ID] {
        tool_use(fake, id, &tool, &input);
        let answer = fake.ask_hook("call", id, &tool, &input);
        if answer.allowed() {
            fake.ran_tool();
            tool_result(fake, id, content.clone(), failed);
            said.push("ran");
        } else {
            let denied = format!("Hook PreToolUse denied this tool: {}", answer.reason);
            tool_result(fake, id, Value::String(denied), true);
            said.push("denied");
        }
    }
    let said = said.join(", ");
    fake.say(&said);
    fake.succeed(&said, None);
    0
}

/// The JSON object in the message's first fenced block: the call rlxd quotes as data.
fn quoted(text: &str) -> Option<Value> {
    let mut lines = text.lines();
    let fence = lines
        .by_ref()
        .find(|line| line.starts_with("```"))?
        .trim_end_matches("json")
        .to_owned();
    let body: Vec<&str> = lines.take_while(|line| *line != fence).collect();
    serde_json::from_str(&body.join("\n")).ok()
}

fn tool_use(fake: &mut Fake, call_id: &str, tool: &str, input: &Value) {
    let session = fake.session_id().to_owned();
    let line = json!({"type": "assistant", "message": {
        "id": format!("msg_{}", &uuid()[..8]), "type": "message", "role": "assistant",
        "model": MODEL, "content": [{"type": "tool_use", "id": call_id, "name": tool,
            "input": input}],
        "stop_reason": "tool_use", "usage": {"input_tokens": 20, "output_tokens": 9}},
        "parent_tool_use_id": null, "session_id": session, "uuid": uuid()});
    fake.remember(&line);
    fake.emit(&line);
}

fn tool_result(fake: &mut Fake, call_id: &str, content: Value, is_error: bool) {
    let session = fake.session_id().to_owned();
    let line = json!({"type": "user", "message": {"role": "user", "content": [
        {"tool_use_id": call_id, "type": "tool_result", "content": content,
            "is_error": is_error}]},
        "parent_tool_use_id": null, "session_id": session, "uuid": uuid()});
    fake.remember(&line);
    fake.emit(&line);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_call_is_read_from_the_fenced_json() {
        let text = "Call this tool once with exactly this input; do nothing else.\n\n\
                    ````json\n{\"connector\":\"c\",\"input\":{\"x\":\"```\"},\"tool\":\"t\"}\n````";
        let call = quoted(text).unwrap();
        assert_eq!(call["tool"], "t");
        assert_eq!(call["input"]["x"], "```");
        assert_eq!(quoted("no fence here"), None);
    }
}
