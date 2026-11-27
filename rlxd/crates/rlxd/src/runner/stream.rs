//! Claude Code's stream-json, read line by line into what the runner makes of it (handoff
//! 0.10.3, 7.5). A type or field this module does not know is ignored, since Claude Code
//! adds them: only the facts below are read.

use std::collections::HashMap;
use std::io;

use rlxd_proto::{McpServer, Usage};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// The longest stream-json line rlxd reads: 16 MiB, the longest event a client reads. A
/// longer one fails the run (`line_too_long`) without rlxd holding it.
pub const MAX_LINE: usize = 16 << 20;

/// The most of a tool result's content an event carries: 1 MiB of JSON.
pub const MAX_CONTENT: usize = 1 << 20;

/// What one line says.
#[derive(Debug, PartialEq)]
pub enum Line {
    /// `system/init`.
    Init(Init),
    /// Assistant text, whole.
    Text(String),
    /// A chunk of assistant text, under `--include-partial-messages`.
    Partial(String),
    /// A `tool_use` block: the call's id and tool.
    ToolUse { id: String, name: String },
    /// A `tool_result` block in a user message.
    ToolResult {
        call_id: String,
        is_error: bool,
        content: Value,
    },
    /// A `rate_limit_event`.
    RateLimit {
        resets_at: Option<u64>,
        status: Option<String>,
    },
    /// The run's `result`.
    Result(Outcome),
    /// Anything else: other system messages, types Claude Code added later, lines that are
    /// not JSON objects.
    Other,
}

/// `system/init`'s facts.
#[derive(Debug, Default, PartialEq)]
pub struct Init {
    pub cwd: String,
    pub model: String,
    pub tools: Vec<String>,
    pub mcp_servers: Vec<McpServer>,
    pub claude_version: String,
    /// Each tool's MCP `readOnlyHint`, for the tools listed as objects that carry one
    /// (`{"name", "annotations": {"readOnlyHint"}}`). Claude Code 2.1 lists names only,
    /// so this is empty and every hint `null` (5.3); the probe checks a later one.
    pub hints: HashMap<String, bool>,
}

/// A `result` line.
#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    /// `subtype: "success"` and not `is_error`.
    pub success: bool,
    /// Its subtype, such as `success` or `error_during_execution`.
    pub subtype: String,
    pub result: String,
    pub structured_output: Option<Value>,
    pub usage: Usage,
    pub cost_usd: Option<f64>,
    pub duration_ms: u64,
    pub num_turns: u64,
    /// What an error result says went wrong.
    pub errors: Vec<String>,
}

/// One stream-json line, read. One line can hold several blocks: each is a [`Line`].
pub fn parse(line: &[u8]) -> Vec<Line> {
    let Ok(Value::Object(object)) = serde_json::from_slice::<Value>(line) else {
        return vec![Line::Other];
    };
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    match object.get("type").and_then(Value::as_str) {
        Some("system") if object.get("subtype").and_then(Value::as_str) == Some("init") => {
            let value = Value::Object(object);
            vec![Line::Init(init(&value))]
        }
        Some("assistant") => {
            let blocks = object
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array);
            let mut lines = Vec::new();
            let mut said = String::new();
            for block in blocks.into_iter().flatten() {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => said.push_str(&text(block, "text").unwrap_or_default()),
                    Some("tool_use") => lines.push(Line::ToolUse {
                        id: text(block, "id").unwrap_or_default(),
                        name: text(block, "name").unwrap_or_default(),
                    }),
                    _ => {}
                }
            }
            if !said.is_empty() {
                lines.insert(0, Line::Text(said));
            }
            lines
        }
        Some("user") => object
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
            .map(|block| Line::ToolResult {
                call_id: text(block, "tool_use_id").unwrap_or_default(),
                is_error: block.get("is_error").and_then(Value::as_bool) == Some(true),
                content: block.get("content").cloned().unwrap_or(Value::Null),
            })
            .collect(),
        Some("stream_event") => {
            let event = object.get("event").unwrap_or(&Value::Null);
            let delta = event.get("delta").unwrap_or(&Value::Null);
            let is_text = event.get("type").and_then(Value::as_str) == Some("content_block_delta")
                && delta.get("type").and_then(Value::as_str) == Some("text_delta");
            match text(delta, "text") {
                Some(chunk) if is_text && !chunk.is_empty() => vec![Line::Partial(chunk)],
                _ => vec![Line::Other],
            }
        }
        Some("rate_limit_event") => {
            let info = object.get("rate_limit_info").unwrap_or(&Value::Null);
            vec![Line::RateLimit {
                resets_at: info.get("resetsAt").and_then(Value::as_u64),
                status: text(info, "status"),
            }]
        }
        Some("result") => vec![Line::Result(outcome(&Value::Object(object)))],
        _ => vec![Line::Other],
    }
}

fn init(value: &Value) -> Init {
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let mut hints = HashMap::new();
    let tools = value
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tool| match tool {
            Value::String(name) => Some(name.clone()),
            Value::Object(tool) => {
                let name = tool.get("name")?.as_str()?.to_owned();
                if let Some(hint) = tool
                    .get("annotations")
                    .and_then(|a| a.get("readOnlyHint"))
                    .and_then(Value::as_bool)
                {
                    hints.insert(name.clone(), hint);
                }
                Some(name)
            }
            _ => None,
        })
        .collect();
    let mcp_servers = value
        .get("mcp_servers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|server| {
            Some(McpServer::new(
                server.get("name")?.as_str()?,
                server
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
            ))
        })
        .collect();
    Init {
        cwd: text("cwd"),
        model: text("model"),
        tools,
        mcp_servers,
        claude_version: text("claude_code_version"),
        hints,
    }
}

fn outcome(value: &Value) -> Outcome {
    let number = |v: &Value, key: &str| v.get(key).and_then(Value::as_u64).unwrap_or_default();
    let subtype = value
        .get("subtype")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let is_error = value.get("is_error").and_then(Value::as_bool) == Some(true);
    let usage = value.get("usage").unwrap_or(&Value::Null);
    let errors = value
        .get("errors")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|e| e.as_str().map_or_else(|| e.to_string(), str::to_owned))
        .collect();
    Outcome {
        success: subtype == "success" && !is_error,
        result: value
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        structured_output: value
            .get("structured_output")
            .filter(|v| !v.is_null())
            .cloned(),
        usage: Usage::new(
            number(usage, "input_tokens"),
            number(usage, "output_tokens"),
        ),
        cost_usd: value.get("total_cost_usd").and_then(Value::as_f64),
        duration_ms: number(value, "duration_ms"),
        num_turns: number(value, "num_turns"),
        errors,
        subtype,
    }
}

/// A tool result's content, cut to [`MAX_CONTENT`] bytes of JSON: as it is, or its
/// first bytes as a string, with whether it was cut.
pub fn bounded(content: Value) -> (Value, bool) {
    let text = content.to_string();
    if text.len() <= MAX_CONTENT {
        return (content, false);
    }
    let mut end = MAX_CONTENT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (Value::String(text[..end].to_owned()), true)
}

/// `text` in pieces of at most [`MAX_CONTENT`] bytes, each cut at a character boundary,
/// so no event grows past what a client reads.
pub fn pieces(text: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut rest = text;
    while rest.len() > MAX_CONTENT {
        let mut end = MAX_CONTENT;
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        pieces.push(&rest[..end]);
        rest = &rest[end..];
    }
    pieces.push(rest);
    pieces
}

/// Why a line could not be read.
#[derive(Debug)]
pub enum ReadError {
    /// It is longer than [`MAX_LINE`].
    TooLong,
    /// Reading failed.
    Io(io::Error),
}

/// The next line of `reader`, without its newline: `None` at the end. A line longer than
/// `max` is refused as soon as it passes `max`, so no more than that is ever held.
pub async fn next_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
    max: usize,
) -> Result<Option<()>, ReadError> {
    line.clear();
    loop {
        let buffer = reader.fill_buf().await.map_err(ReadError::Io)?;
        if buffer.is_empty() {
            return Ok((!line.is_empty()).then_some(()));
        }
        let (taken, done) = match buffer.iter().position(|&b| b == b'\n') {
            Some(at) => (at + 1, true),
            None => (buffer.len(), false),
        };
        let body = if done { taken - 1 } else { taken };
        if line.len() + body > max {
            return Err(ReadError::TooLong);
        }
        line.extend_from_slice(&buffer[..body]);
        reader.consume(taken);
        if done {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(()));
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn one(value: Value) -> Line {
        let mut lines = parse(value.to_string().as_bytes());
        assert_eq!(lines.len(), 1, "{lines:?}");
        lines.remove(0)
    }

    #[test]
    fn init_gives_the_run_its_facts_and_ignores_the_rest() {
        let line = one(
            json!({"type": "system", "subtype": "init", "cwd": "/s", "model": "m",
            "tools": ["mcp__x__y"], "mcp_servers": [{"name": "claude.ai Slack",
            "status": "connected", "new": 1}], "claude_code_version": "2.1.314",
            "something_new": {"a": 1}}),
        );
        assert_eq!(
            line,
            Line::Init(Init {
                cwd: "/s".into(),
                model: "m".into(),
                tools: vec!["mcp__x__y".into()],
                mcp_servers: vec![McpServer::new("claude.ai Slack", "connected")],
                claude_version: "2.1.314".into(),
                hints: HashMap::new(),
            })
        );
    }

    #[test]
    fn a_tool_listed_with_annotations_gives_its_read_only_hint() {
        let line = one(json!({"type": "system", "subtype": "init", "tools": [
            "Read", {"name": "mcp__a__r", "annotations": {"readOnlyHint": true}},
            {"name": "mcp__a__w", "annotations": {"readOnlyHint": false}},
            {"name": "mcp__a__u"}, {"no_name": 1}]}));
        let Line::Init(init) = line else {
            panic!("{line:?}");
        };
        assert_eq!(init.tools, ["Read", "mcp__a__r", "mcp__a__w", "mcp__a__u"]);
        assert_eq!(
            init.hints,
            HashMap::from([
                ("mcp__a__r".to_owned(), true),
                ("mcp__a__w".to_owned(), false)
            ])
        );
    }

    #[test]
    fn unknown_types_and_lines_that_are_not_objects_are_other() {
        assert_eq!(one(json!({"type": "brand_new"})), Line::Other);
        assert_eq!(
            one(json!({"type": "system", "subtype": "hook_started"})),
            Line::Other
        );
        assert_eq!(parse(b"not json"), vec![Line::Other]);
        assert_eq!(parse(b"[1,2]"), vec![Line::Other]);
    }

    #[test]
    fn assistant_text_and_tool_use_blocks_are_read() {
        let lines = parse(
            json!({"type": "assistant", "message": {"content": [
                {"type": "text", "text": "a"}, {"type": "thinking", "thinking": "x"},
                {"type": "tool_use", "id": "toolu_1", "name": "mcp__s__t", "input": {}},
                {"type": "text", "text": "b"}]}})
            .to_string()
            .as_bytes(),
        );
        assert_eq!(
            lines,
            vec![
                Line::Text("ab".into()),
                Line::ToolUse {
                    id: "toolu_1".into(),
                    name: "mcp__s__t".into()
                }
            ]
        );
    }

    #[test]
    fn a_tool_result_partial_text_and_a_rate_limit_are_read() {
        let content = json!([{"type": "text", "text": "r"}]);
        assert_eq!(
            one(
                json!({"type": "user", "message": {"content": [{"type": "tool_result",
                "tool_use_id": "toolu_1", "content": content, "is_error": true}]}})
            ),
            Line::ToolResult {
                call_id: "toolu_1".into(),
                is_error: true,
                content
            }
        );
        assert_eq!(
            one(
                json!({"type": "stream_event", "event": {"type": "content_block_delta",
                "delta": {"type": "text_delta", "text": "He"}}})
            ),
            Line::Partial("He".into())
        );
        assert_eq!(
            one(json!({"type": "stream_event", "event": {"type": "message_start"}})),
            Line::Other
        );
        assert_eq!(
            one(
                json!({"type": "rate_limit_event", "rate_limit_info": {"resetsAt": 1_764_000_000,
                "status": "rejected"}})
            ),
            Line::RateLimit {
                resets_at: Some(1_764_000_000),
                status: Some("rejected".into())
            }
        );
    }

    #[test]
    fn a_result_says_whether_it_succeeded() {
        let Line::Result(done) = one(json!({"type": "result", "subtype": "success",
            "is_error": false, "result": "ok", "structured_output": {"v": 1},
            "usage": {"input_tokens": 3, "output_tokens": 4}, "total_cost_usd": 0.5,
            "duration_ms": 9, "num_turns": 2}))
        else {
            panic!()
        };
        assert!(done.success);
        assert_eq!(done.structured_output, Some(json!({"v": 1})));
        assert_eq!(done.usage, Usage::new(3, 4));
        assert_eq!((done.duration_ms, done.num_turns), (9, 2));
        let Line::Result(failed) = one(json!({"type": "result",
            "subtype": "error_during_execution", "is_error": true, "errors": ["boom"]}))
        else {
            panic!()
        };
        assert!(!failed.success);
        assert_eq!(failed.errors, ["boom"]);
    }

    #[test]
    fn content_past_a_mebibyte_is_cut_and_said_to_be() {
        let small = json!([{"type": "text", "text": "x"}]);
        assert_eq!(bounded(small.clone()), (small, false));
        let big = json!([{"type": "text", "text": "é".repeat(MAX_CONTENT)}]);
        let (cut, truncated) = bounded(big);
        assert!(truncated);
        assert!(cut.as_str().unwrap().len() <= MAX_CONTENT);
        let text = "é".repeat(MAX_CONTENT);
        let parts = pieces(&text);
        assert!(parts.len() > 1 && parts.iter().all(|p| p.len() <= MAX_CONTENT));
        assert_eq!(parts.concat(), text);
    }

    #[tokio::test]
    async fn a_line_past_the_limit_is_refused_without_being_held() {
        let mut reader = tokio::io::BufReader::with_capacity(4, &b"abc\r\nabcdefgh\nx"[..]);
        let mut line = Vec::new();
        assert!(matches!(
            next_line(&mut reader, &mut line, 5).await,
            Ok(Some(()))
        ));
        assert_eq!(line, b"abc");
        assert!(matches!(
            next_line(&mut reader, &mut line, 5).await,
            Err(ReadError::TooLong)
        ));
        assert!(line.len() <= 5);
        let mut reader = &b"last"[..];
        assert!(matches!(
            next_line(&mut reader, &mut line, 5).await,
            Ok(Some(()))
        ));
        assert_eq!(line, b"last");
        assert!(matches!(
            next_line(&mut reader, &mut line, 5).await,
            Ok(None)
        ));
    }
}
