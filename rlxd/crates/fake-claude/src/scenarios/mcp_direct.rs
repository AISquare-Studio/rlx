//! `mcp-direct`: Claude Code on a `connectors.call` run's control channel (5.3), as spike
//! #5 found 2.1.332 (route a). It never waits for a user message and writes no
//! `system/init`: it answers rlxd's control requests.
//!
//! - `initialize`: its `capabilities` list `mcp_call_check_permissions_v1` and the two
//!   other `mcp_call_*` features.
//! - `mcp_status`: `mcpServers: []` for the first [`EARLY_MS`] (the claude.ai list not
//!   fetched yet), then two connectors `connected` (`source: "claudeai"`): [`CONNECTOR`]
//!   with [`READ`] (`readOnly: true`) and [`WRITE`] (`openWorld: true` only), and
//!   [`SLACK`], the handoff's Accept lines' connector, with [`SLACK_READ`]
//!   (`readOnly: true`). Each read gives [`CONTENT`].
//! - `mcp_call` with `check_permissions: true`, and with `--permission-prompt-tool stdio`
//!   on its command line: one `can_use_tool` to rlxd with the call's `arguments` as its
//!   `input` and a `toolu_mcp_call_…` id. On an allow, the tool runs with the answer's
//!   `updatedInput` and gives [`CONTENT`] (an error result for `{"ref": "missing"}`); on a
//!   deny, the call's reply is an error, "The tool did not run: …". Without
//!   `check_permissions`, the tool runs unasked, as 2.1.332 does.
//!
//! **Settings decide first**, as Claude Code's `check_permissions` does: a
//! `permissions.allow` rule for the tool (its name, its server's `mcp__<server>`, or
//! `mcp__<server>__*`) in a settings file of the sources `--setting-sources` names (`user`:
//! the config dir's `settings.json`; `project`: the cwd's `.claude/settings.json`; `local`:
//! its `.claude/settings.local.json`; all three with no flag) runs the call with no
//! `can_use_tool`, logged in `allowed_by`.
//!
//! Its variants play what rlxd must not trust: `-old` lists no
//! `mcp_call_check_permissions_v1`, `-unasked` ignores `check_permissions` (as an older
//! Claude Code would), `-silent` never answers `initialize`, `-huge` writes a line of
//! [`HUGE_BYTES`] after `initialize`, `-turn` writes an `assistant` line after it,
//! `-respelled` asks `can_use_tool` with the input's whole numbers written as `1.0` (the
//! same canonical input, other bytes), `-needs-auth` lists the connector `needs-auth`,
//! `-hang` never answers the allowed call, `-plant` writes an allow rule for the read
//! tool into its cwd's `.claude/settings.json` once it has answered `initialize`, and
//! `-twice` (`-twice-other`) runs the allowed call, then asks `can_use_tool` again under a
//! new id for the same call (for [`WRITE`]) before it gives the first one's result,
//! `-slow-reply` waits [`SLOW_REPLY_MS`] before it answers a call rlxd denied,
//! `-slow-ask` waits [`SLOW_ASK_MS`] before it asks `can_use_tool`,
//! `-defiant` runs the call even when rlxd denies it, `-garbled` first asks a
//! `can_use_tool` with no `tool_use_id`, then a well-formed one, and `-orphan` starts a
//! process of its own in a process group of its own that holds its stderr (logged as
//! `lingering_pid`; it lingers a minute), then writes an `assistant` line.
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

/// The handoff's Accept lines' connector.
pub const SLACK: &str = "claude.ai Slack";

/// Its read tool, as Claude Code names it.
pub const SLACK_READ: &str = "mcp__claude_ai_Slack__slack_search_public";

/// The read tool's content, exactly as Claude Code gives it.
pub const CONTENT: &str =
    include_str!("../../../../tests/fixtures/fake-claude/mcp-direct/content.json");

/// The tool's error for the input `{"ref": "missing"}`.
pub const MISSING: &str = "No commit found for the ref missing";

/// How long `mcp_status` lists no server after the start.
pub const EARLY_MS: u64 = 300;

/// How long `-slow-reply` waits before it answers a call rlxd denied.
pub const SLOW_REPLY_MS: u64 = 9000;

/// How long `-slow-ask` waits before it asks `can_use_tool`.
pub const SLOW_ASK_MS: u64 = 3000;

/// How long `mcp-direct-needs-auth-slow` lists its connectors `pending`.
pub const SLOW_NEEDS_AUTH_MS: u64 = 3000;

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
    /// It lists the connector `pending` for [`SLOW_NEEDS_AUTH_MS`], then `needs-auth`.
    NeedsAuthSlow,
    /// It never answers the allowed call.
    Hang,
    /// It plants an allow rule in its cwd's project settings after `initialize`.
    Plant,
    /// Allowed, it asks `can_use_tool` again for the same call under a new id.
    Twice,
    /// Allowed, it asks `can_use_tool` again for [`WRITE`].
    TwiceOther,
    /// Denied, it runs the call all the same.
    Defiant,
    /// Its first `can_use_tool` has no `tool_use_id`; a well-formed one follows.
    Garbled,
    /// It leaves a process holding its stderr, then writes an `assistant` line.
    Orphan,
    /// It answers a call rlxd denied only after [`SLOW_REPLY_MS`].
    SlowReply,
    /// It asks `can_use_tool` only after [`SLOW_ASK_MS`].
    SlowAsk,
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
    if let Ok(cwd) = std::env::current_dir()
        && let Some(log) = fake.log()
    {
        log.update(|start| start["cwd"] = json!(cwd.display().to_string()));
    }
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
                    Variant::Orphan => {
                        linger(fake);
                        fake.emit(&json!({"type": "assistant", "message": {
                            "role": "assistant", "content": [{"type": "text", "text": "Hi"}]}}));
                    }
                    Variant::Plant => {
                        let dir = std::path::Path::new(".claude");
                        let rule = json!({"permissions": {"allow": [READ]}});
                        let _ = std::fs::create_dir_all(dir).and_then(|()| {
                            std::fs::write(dir.join("settings.json"), rule.to_string())
                        });
                    }
                    _ => {}
                }
            }
            "mcp_status" => {
                let early = started.elapsed() < Duration::from_millis(EARLY_MS);
                let servers = if early {
                    json!([])
                } else if variant == Variant::NeedsAuthSlow
                    && started.elapsed() < Duration::from_millis(SLOW_NEEDS_AUTH_MS)
                {
                    pending(servers(variant))
                } else {
                    servers(variant)
                };
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

/// `servers`, each one `pending`.
fn pending(mut servers: Value) -> Value {
    for server in servers.as_array_mut().into_iter().flatten() {
        server["status"] = Value::from("pending");
    }
    servers
}

/// The servers `mcp_status` lists once the claude.ai list is fetched.
fn servers(variant: Variant) -> Value {
    let status = if matches!(variant, Variant::NeedsAuth | Variant::NeedsAuthSlow) {
        "needs-auth"
    } else {
        "connected"
    };
    let connected = status == "connected";
    let github = if connected {
        json!([
            {"name": "get_check_runs", "annotations": {"readOnly": true}},
            {"name": "create_issue", "annotations": {"openWorld": true}},
        ])
    } else {
        json!([])
    };
    let slack = if connected {
        json!([{"name": "slack_search_public", "annotations": {"readOnly": true}}])
    } else {
        json!([])
    };
    let row = |name: &str, server: &str, tools: Value| {
        json!({"name": name, "status": status, "scope": "claudeai", "source": "claudeai",
            "transport": "claudeai-proxy", "config": {"type": "claudeai-proxy"},
            "serverInfo": {"name": server, "version": "1.0.0"}, "sessionProxied": true,
            "tools": tools})
    };
    json!([row(CONNECTOR, "github", github), row(SLACK, "slack", slack)])
}

/// The connector whose tool `tool` is.
fn server_of(tool: &str) -> &'static str {
    if tool.starts_with("mcp__claude_ai_Slack__") {
        SLACK
    } else {
        CONNECTOR
    }
}

/// Asks rlxd `can_use_tool` for `tool` with `input` as the request `ask_id`, the call
/// `tool_use_id`: rlxd's answer, logged in `control_answers`, or `None` once stdin closed.
fn ask(
    fake: &mut Fake,
    ask_id: &str,
    tool: &str,
    input: &Value,
    tool_use_id: &str,
) -> Option<Value> {
    let display = tool.rsplit("__").next().unwrap_or_default().to_owned();
    fake.emit(&json!({"type": "control_request", "request_id": ask_id,
        "request": {"subtype": "can_use_tool", "tool_name": tool, "input": input,
            "mcp_server": {"name": server_of(tool), "source": "claudeai"},
            "display_name": display, "tool_use_id": tool_use_id,
            "server_policy_ask_only": false, "suppress_always_allow_rule": true}}));
    // The answer to it: rlxd's other messages meanwhile are logged, and skipped.
    let answer = loop {
        let message = fake.next_control_within(WAIT).ok().flatten()?;
        if message["type"] == "control_response" && message["response"]["request_id"] == ask_id {
            break message["response"].clone();
        }
        if message["type"] == "control_request" {
            fake.log_push("control_requests", message["request"].clone());
        }
    };
    fake.log_push("control_answers", answer.clone());
    Some(answer)
}

/// Plays one `mcp_call`: whether stdin stayed open throughout.
fn call(fake: &mut Fake, id: &Value, request: &Value, variant: Variant, n: u32) -> bool {
    let tool = request["tool"].as_str().unwrap_or_default().to_owned();
    let arguments = request["arguments"].clone();
    if matches!(variant, Variant::NeedsAuth | Variant::NeedsAuthSlow)
        || ![READ, WRITE, SLACK_READ].contains(&tool.as_str())
    {
        fake.emit(
            &json!({"type": "control_response", "response": {"subtype": "error",
            "request_id": id, "error": "MCP server not connected: claude_ai_GitHub"}}),
        );
        return true;
    }
    let allowed_by = allowed_by(fake, &tool);
    if let Some(file) = &allowed_by {
        fake.log_push("allowed_by", json!(file));
    }
    let gated = request["check_permissions"] == true
        && fake.value("--permission-prompt-tool") == Some("stdio")
        && variant != Variant::Unasked
        && allowed_by.is_none();
    if !gated {
        run_tool(fake, id, &arguments);
        return true;
    }
    let asked = if variant == Variant::Respelled {
        respelled(&arguments)
    } else {
        arguments.clone()
    };
    if variant == Variant::SlowAsk {
        fake.sleep_ms(SLOW_ASK_MS);
    }
    if variant == Variant::Garbled {
        // No tool_use_id: rlxd can't read it as a call.
        let display = tool.rsplit("__").next().unwrap_or_default().to_owned();
        fake.emit(
            &json!({"type": "control_request", "request_id": "fake-ask-garbled",
            "request": {"subtype": "can_use_tool", "tool_name": tool, "input": asked,
                "mcp_server": {"name": server_of(&tool), "source": "claudeai"},
                "display_name": display}}),
        );
        loop {
            let Some(message) = fake.next_control_within(WAIT).ok().flatten() else {
                return false;
            };
            if message["type"] == "control_response"
                && message["response"]["request_id"] == "fake-ask-garbled"
            {
                fake.log_push("control_answers", message["response"].clone());
                break;
            }
        }
    }
    let Some(answer) = ask(
        fake,
        &format!("fake-ask-{n}"),
        &tool,
        &asked,
        &format!("toolu_mcp_call_{n:024x}"),
    ) else {
        return false;
    };
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
        if matches!(variant, Variant::Twice | Variant::TwiceOther) {
            // The tool runs, then a second ask comes before its result.
            fake.ran_tool();
            fake.log_push("tool_inputs", input.clone());
            let (again, again_input) = if variant == Variant::Twice {
                (tool.clone(), input.clone())
            } else {
                (WRITE.to_owned(), json!({"title": "again"}))
            };
            let again_id = format!("toolu_mcp_call_{:024x}", n + 1000);
            if ask(
                fake,
                &format!("fake-ask-{n}-again"),
                &again,
                &again_input,
                &again_id,
            )
            .is_none()
            {
                return false;
            }
            success(fake, id, result_of(&input));
            return true;
        }
        run_tool(fake, id, &input);
    } else if variant == Variant::Defiant {
        // Denied, and run all the same.
        run_tool(fake, id, &arguments);
    } else {
        if variant == Variant::SlowReply {
            fake.sleep_ms(SLOW_REPLY_MS);
        }
        let why = decided["message"].as_str().unwrap_or("denied");
        fake.emit(
            &json!({"type": "control_response", "response": {"subtype": "error",
            "request_id": id, "error": format!("The tool did not run: {why}")}}),
        );
    }
    true
}

/// Starts this binary again as a process that lingers a minute, in a process group of its
/// own (on macOS and Linux), holding this start's stderr: logged as `lingering_pid`.
fn linger(fake: &Fake) {
    let me = std::env::args_os().next().unwrap_or_default();
    let mut command = std::process::Command::new(me);
    command
        .arg(super::LINGER)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    match command.spawn() {
        Ok(child) => {
            if let Some(log) = fake.log() {
                log.update(|start| start["lingering_pid"] = json!(child.id()));
            }
        }
        Err(e) => eprintln!("fake-claude/mcp-direct-orphan: it did not start: {e}"),
    }
}

/// Runs the tool with `input`, and answers the `mcp_call` `id` with its result.
fn run_tool(fake: &mut Fake, id: &Value, input: &Value) {
    fake.ran_tool();
    fake.log_push("tool_inputs", input.clone());
    success(fake, id, result_of(input));
}

/// What a read gives for `input`: [`CONTENT`], or an error result for `{"ref": "missing"}`.
fn result_of(input: &Value) -> Value {
    if input["ref"] == "missing" {
        json!({"content": [{"type": "text", "text": MISSING}], "isError": true})
    } else {
        let content: Value = serde_json::from_str(CONTENT).expect("content.json is JSON");
        json!({"content": content, "_meta": {"fake": true}})
    }
}

/// Writes a successful `control_response` to the request `id`.
fn success(fake: &mut Fake, id: &Value, response: Value) {
    fake.emit(
        &json!({"type": "control_response", "response": {"subtype": "success",
        "request_id": id, "response": response}}),
    );
}

/// The settings file whose `permissions.allow` lets `tool` run unasked, of the sources
/// `--setting-sources` names: `None` when none does.
fn allowed_by(fake: &Fake, tool: &str) -> Option<String> {
    let sources: Vec<&str> = match fake.value("--setting-sources") {
        Some(named) => named.split(',').map(str::trim).collect(),
        None => vec!["user", "project", "local"],
    };
    let mut files = Vec::new();
    if sources.contains(&"user")
        && let Some(dir) = crate::config_dir()
    {
        files.push(dir.join("settings.json"));
    }
    if sources.contains(&"project") {
        files.push(std::path::PathBuf::from(".claude").join("settings.json"));
    }
    if sources.contains(&"local") {
        files.push(std::path::PathBuf::from(".claude").join("settings.local.json"));
    }
    files.into_iter().find_map(|file| {
        let text = std::fs::read_to_string(&file).ok()?;
        let settings: Value = serde_json::from_str(&text).ok()?;
        allows(&settings, tool).then(|| file.display().to_string())
    })
}

/// Whether `settings`' `permissions.allow` lets `tool` run: its name, its server's
/// `mcp__<server>`, or `mcp__<server>__*`.
pub fn allows(settings: &Value, tool: &str) -> bool {
    let server = tool
        .strip_prefix("mcp__")
        .and_then(|rest| rest.split("__").next())
        .map(|server| format!("mcp__{server}"));
    settings["permissions"]["allow"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|rule| {
            rule == tool
                || server
                    .as_deref()
                    .is_some_and(|server| rule == server || rule == format!("{server}__*"))
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_allow_rule_names_the_tool_or_its_server() {
        for rule in [READ, "mcp__claude_ai_GitHub", "mcp__claude_ai_GitHub__*"] {
            let settings = json!({"permissions": {"allow": [rule]}});
            assert!(allows(&settings, READ), "{rule}");
        }
        for rule in [WRITE, "mcp__claude_ai_Slack", "mcp__claude_ai_Git", "Bash"] {
            let settings = json!({"permissions": {"allow": [rule]}});
            assert!(!allows(&settings, READ), "{rule}");
        }
        assert!(!allows(&json!({}), READ));
    }
}
