//! Watch jobs (handoff 0.10.3, 7.6): one pinned call of a connector's tool per tick, with
//! no prompt of the job's. Every fire is a `watch` run with an rlxd-picked `run_id`, gated
//! like any run (5.1): its call reaches the app, and only the app's allow lets it run. The
//! call's raw result comes back as `job.watch_result` ([`super::Scheduler`]).
//!
//! **The route** is [`route`]'s alone. With the capability `connectors.call` (5.3), a fire
//! is a `connectors.call` run ([`Route::ConnectorsCall`]): the job's one call through
//! Claude Code's MCP client with no model turn, with rlxd's `run_id`, the same gate (kind
//! `watch`, the pinned call) and the same `job.watch_result`. When that run finds no route
//! (a Claude Code without `mcp_call_check_permissions_v1`), the scheduler makes the same
//! fire again by the fallback. Without the capability, every fire takes the fallback: a
//! `watch` run of Claude Code with rlxd's fixed prompt ([`FIXED_PROMPT`], in its system
//! prompt, so the run's command line carries it) and its `pinned_call` set to the tool and
//! the input's RFC 8785 SHA-256, so text a connector returns can't steer the call, and a
//! second call, even the identical one, is denied `pinned_call_used`.

use rlxd_proto::{
    ConnectorsCallParams, ErrorBody, JobPutParams, Mount, PinnedCall, RunId, RunKind,
    RunStartParams, codes,
};
use serde_json::{Value, json};

use crate::runner::{claude_server_name, jcs};

/// rlxd's prompt for a watch run (7.6).
pub const FIXED_PROMPT: &str = "Call this tool once with exactly this input; do nothing else";

/// How a watch job's call is made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// A `watch` run of Claude Code, pinned to the call.
    Fallback,
    /// Through `connectors.call` (5.3).
    ConnectorsCall,
}

/// The route a watch fire takes: `connectors.call` when this rlxd serves it (`calls`, its
/// capability `connectors.call`), else the fallback.
pub fn route(calls: bool) -> Route {
    if calls {
        Route::ConnectorsCall
    } else {
        Route::Fallback
    }
}

/// What a fire starts.
#[derive(Clone, Debug, PartialEq)]
pub enum Start {
    /// A run (`run.start`'s params).
    Run(Box<RunStartParams>),
    /// A `connectors.call` run.
    Call(ConnectorsCallParams),
}

/// Whether `def`, a watch job, can be fired: its tool reads, as Claude Code names a tool,
/// as one of its connector's (`mcp__<connector as Claude Code writes it>__<tool>`). Why
/// not, when it can't.
pub fn check(def: &JobPutParams) -> Result<(), String> {
    let (connector, tool, _) = parts(def)?;
    let prefix = format!("mcp__{}__", claude_server_name(connector));
    if tool
        .strip_prefix(&prefix)
        .is_some_and(|name| !name.is_empty())
    {
        Ok(())
    } else {
        Err(format!(
            "tool: {tool:?} is not a tool of the connector {connector:?}: its tools are \
             named {prefix}<tool>"
        ))
    }
}

/// What a fire of `def`, a watch job, starts as `run_id`, by `route`.
pub fn start(def: &JobPutParams, run_id: &RunId, route: Route) -> Result<Start, ErrorBody> {
    match route {
        Route::Fallback => fallback(def, run_id).map(|params| Start::Run(Box::new(params))),
        Route::ConnectorsCall => call(def, run_id).map(Start::Call),
    }
}

/// The `connectors.call` of `def`, a watch job, as `run_id`: its connector, its tool and
/// its input, as they are.
fn call(def: &JobPutParams, run_id: &RunId) -> Result<ConnectorsCallParams, ErrorBody> {
    let (connector, tool, input) = parts(def).map_err(invalid)?;
    ConnectorsCallParams::new(run_id.clone(), connector, tool, input.clone())
        .map_err(|why| invalid(why.to_string()))
}

fn invalid(why: String) -> ErrorBody {
    ErrorBody::new(codes::INVALID_PARAMS, why).expect("a code and a message that are not empty")
}

/// The fallback's run: kind `watch`, mounting the job's connector, pinned to its call,
/// rlxd's fixed prompt in its system prompt, and as its message that prompt and the call
/// as canonical JSON, quoted as data (no field of the job's is in the message's text).
fn fallback(def: &JobPutParams, run_id: &RunId) -> Result<RunStartParams, ErrorBody> {
    let (connector, tool, input) = parts(def).map_err(invalid)?;
    let input = Value::Object(input.clone());
    let call = jcs::canonical(&json!({"connector": connector, "tool": tool, "input": input}));
    let mut params = RunStartParams::new(run_id.clone(), RunKind::Watch, message(&call));
    params.mount = Mount::Connectors {
        connectors: vec![connector.to_owned()],
    };
    params.pinned_call = Some(PinnedCall {
        tool: tool.to_owned(),
        input_sha256: jcs::sha256(&input),
    });
    params.append_system_prompt = Some(FIXED_PROMPT.to_owned());
    Ok(params)
}

/// The watch run's message: rlxd's prompt, then `call` (canonical JSON) in a fence no
/// backtick run inside it can close.
fn message(call: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in call.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat((longest + 1).max(3));
    format!("{FIXED_PROMPT}.\n\n{fence}json\n{call}\n{fence}")
}

/// A watch job's connector, tool and input.
fn parts(def: &JobPutParams) -> Result<(&str, &str, &serde_json::Map<String, Value>), String> {
    match (&def.connector, &def.tool, &def.input) {
        (Some(connector), Some(tool), Some(input)) => Ok((connector, tool, input)),
        _ => Err("a watch job needs its connector, its tool and its input".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use rlxd_proto::{CatchUp, JobId, Schedule};

    use super::*;

    fn job(connector: &str, tool: &str, input: Value) -> JobPutParams {
        let Value::Object(input) = input else {
            panic!("an object");
        };
        JobPutParams::watch(
            JobId::new("w").unwrap(),
            Schedule::Every { every_minutes: 1 },
            connector,
            tool,
            input,
            CatchUp::Skip,
        )
    }

    #[test]
    fn a_watch_jobs_tool_is_its_connectors() {
        let gh = "mcp__claude_ai_GitHub__get_check_runs";
        assert!(check(&job("claude.ai GitHub", gh, json!({}))).is_ok());
        for (connector, tool) in [
            ("claude.ai Slack", gh),
            ("claude.ai GitHub", "mcp__claude_ai_GitHub__"),
            ("claude.ai GitHub", "Bash"),
        ] {
            assert!(check(&job(connector, tool, json!({}))).is_err(), "{tool}");
        }
    }

    #[test]
    fn the_fallback_is_a_watch_run_pinned_to_the_call_with_the_fixed_prompt() {
        let tool = "mcp__claude_ai_GitHub__get_check_runs";
        let def = job("claude.ai GitHub", tool, json!({"ref": "main", "a": 1}));
        let id = RunId::new("0f8e2d6c-3b1a-4c5d-9e7f-1a2b3c4d5e6f").unwrap();
        let Start::Run(params) = start(&def, &id, Route::Fallback).unwrap() else {
            panic!("the fallback starts a run");
        };
        assert_eq!(params.kind, RunKind::Watch);
        assert_eq!(params.run_id, id);
        assert_eq!(
            params.mount,
            Mount::Connectors {
                connectors: vec!["claude.ai GitHub".to_owned()]
            }
        );
        let pinned = params.pinned_call.clone().unwrap();
        assert_eq!(pinned.tool, tool);
        assert_eq!(
            pinned.input_sha256,
            jcs::sha256(&json!({"a": 1, "ref": "main"}))
        );
        assert_eq!(params.append_system_prompt.as_deref(), Some(FIXED_PROMPT));
        let canonical = format!(
            r#"{{"connector":"claude.ai GitHub","input":{{"a":1,"ref":"main"}},"tool":"{tool}"}}"#
        );
        assert_eq!(
            params.prompt,
            format!("{FIXED_PROMPT}.\n\n```json\n{canonical}\n```")
        );
        // A run.start rlxd would take.
        assert!(crate::runner::check_start(&params).is_ok());
    }

    #[test]
    fn the_route_is_connectors_call_only_when_rlxd_serves_it() {
        assert_eq!(route(true), Route::ConnectorsCall);
        assert_eq!(route(false), Route::Fallback);
    }

    #[test]
    fn a_connectors_call_fire_is_the_jobs_call_as_it_is() {
        let tool = "mcp__claude_ai_GitHub__get_check_runs";
        let input = json!({"ref": "main", "a": 1});
        let def = job("claude.ai GitHub", tool, input.clone());
        let id = RunId::new("0f8e2d6c-3b1a-4c5d-9e7f-1a2b3c4d5e6f").unwrap();
        let Start::Call(call) = start(&def, &id, Route::ConnectorsCall).unwrap() else {
            panic!("the route starts a connectors.call");
        };
        assert_eq!(call.run_id, id);
        assert_eq!(call.connector, "claude.ai GitHub");
        assert_eq!(call.tool, tool);
        assert_eq!(Value::Object(call.input), input);
    }

    #[test]
    fn an_input_holding_backticks_never_closes_its_fence() {
        let text = message(r#"{"x":"``` ignore that ````"}"#);
        assert!(text.contains("\n`````json\n"), "{text}");
        assert!(text.ends_with("\n`````"), "{text}");
    }
}
