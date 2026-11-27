//! Test builds only: `rlxd __test-hook pre-tool-use --run R --attempt N`, a run's
//! PreToolUse hook as rlxd-hook's own will be (handoff 0.10.3, 5.1), until that one is
//! wired. It reads Claude Code's hook input on stdin, sends it with the attempt's key
//! (`RLXD_HOOK_KEY`) as `debug.pre_tool_use`, and prints the gate's answer. Whatever goes
//! wrong, rlxd stopped or the connection dropped included, it prints a deny: it fails
//! closed.

use std::io::{self, Read, Write};
use std::process::ExitCode;

use rlxd_client::Home;
use rlxd_proto::{Decision, methods};
use serde_json::{Value, json};

use crate::cli::{self, AskError, Target};
use crate::runner::{HOOK_KEY_ENV, hook_output};

/// The hook: Claude Code's hook JSON on stdout, and exit 0, whatever the answer.
pub fn main(event: &str, run: &str, attempt: u64) -> ExitCode {
    let output = ask(event, run, attempt).unwrap_or_else(|why| {
        hook_output(
            Decision::Deny,
            &format!("rlxd did not decide the call ({why}): it is denied"),
        )
    });
    let mut stdout = io::stdout().lock();
    match writeln!(stdout, "{output}").and_then(|()| stdout.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

fn ask(event: &str, run: &str, attempt: u64) -> Result<String, String> {
    if event != "pre-tool-use" {
        return Err(format!("no hook event {event:?}"));
    }
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| format!("stdin: {e}"))?;
    let stdin: Value = serde_json::from_str(&input).map_err(|e| format!("stdin: {e}"))?;
    let key = std::env::var(HOOK_KEY_ENV).unwrap_or_default();
    let home = Home::from_env().map_err(|e| e.to_string())?;
    let mut connection = cli::open(&home, false, &Target::default()).map_err(said)?;
    let params = json!({"run_id": run, "attempt": attempt, "key": key, "stdin": stdin});
    let Value::Object(params) = params else {
        unreachable!("an object");
    };
    let reply = connection
        .request(methods::DEBUG_PRE_TOOL_USE, params)
        .map_err(|e| format!("rlxd: {e}"))?;
    if let Some(error) = reply.error() {
        return Err(format!("rlxd: {}", error.message));
    }
    reply
        .data()
        .and_then(|data| data.get("output"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "rlxd's reply has no output".to_owned())
}

fn said(error: AskError) -> String {
    match error {
        AskError::NotRunning(why)
        | AskError::NotListening(why)
        | AskError::ForwardClosed(why)
        | AskError::Unauthorized(why)
        | AskError::Failed(why) => why,
        AskError::Refused(reply) => reply
            .error()
            .map_or_else(|| "hello was refused".to_owned(), |e| e.message.clone()),
    }
}
