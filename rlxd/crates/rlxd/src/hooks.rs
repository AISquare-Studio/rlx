//! `hook` (handoff 0.10.3, 1.5): one Claude Code hook, from rlxd-hook.
//!
//! - **The event.** Every `hook` that parses publishes one `hook.<event>` on the `hook`
//!   topic first, `{session_id, cwd, run_id?}` (7.3): the session and directory from the
//!   hook's JSON (the directory rlxd-hook ran in when the JSON names none).
//! - **The fleet's six** go to the resident brain (`crate::brain`), which runs `rlx hook
//!   <event>`'s own code with the hook's stdin, directory and allowlisted environment
//!   (`HOOK_ENV_NAMES`; any other name is dropped here), and answers the bytes for
//!   rlxd-hook's stdout and stderr. The brain must accept it within `accept_ms` (5 s
//!   unless rlxd-hook says less) and finish it within `deadline_ms`.
//! - **The fallback line** is the brain's `accepted`. A hook the brain never accepted
//!   fails with `hook_not_delivered`: nothing of it ran, and rlxd-hook runs `rlx hook
//!   <event>` itself. Once accepted, a failure is `hook_failed` and an overrun
//!   `hook_timeout`, and rlxd-hook runs nothing more: the prompt is never logged twice.
//! - **`pre-tool-use`** is the gate's (5.1). Until the gate decides tool calls, rlxd
//!   denies every one itself, and nothing here can allow one.

use std::time::Duration;

use rlxd_proto::{
    Data, ErrorBody, HOOK_ENV_NAMES, HookEventData, HookParams, HookReply, PRE_TOOL_USE, b64,
    codes, deny_output, hook_codes, hook_event_type, topics,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::brain::CallError;
use crate::log::warning;
use crate::server::Daemon;

/// How long the brain may take to accept a hook when rlxd-hook does not say, and at most.
const ACCEPT_DEFAULT: Duration = Duration::from_secs(5);
const ACCEPT_MOST: Duration = Duration::from_secs(30);

/// How long a hook may take when rlxd-hook does not say, and at most.
const DEADLINE_DEFAULT: Duration = Duration::from_secs(50);
const DEADLINE_MOST: Duration = Duration::from_secs(600);

/// Why every `pre-tool-use` is denied, until the gate decides them (5.1).
pub const NO_GATE_YET: &str =
    "rlxd's gate does not decide tool calls yet, so every one is denied (rlx 0.10.3, 5.1)";

/// `hook`: publishes its event, then runs it, as the module says.
pub async fn hook(daemon: &Daemon, params: Data) -> Result<Data, ErrorBody> {
    let params = HookParams::deserialize(Value::Object(params)).map_err(|e| {
        error(
            codes::INVALID_PARAMS,
            &format!("hook's params are not valid: {e}"),
        )
    })?;
    let stdin = b64::decode(&params.stdin_b64).unwrap_or_default();
    publish(daemon, &params, &stdin);
    if params.event == PRE_TOOL_USE {
        return Ok(reply(deny_output(NO_GATE_YET).as_bytes()));
    }
    let env: serde_json::Map<String, Value> = params
        .env
        .iter()
        .filter(|(name, _)| HOOK_ENV_NAMES.contains(&name.as_str()))
        .map(|(name, value)| (name.clone(), Value::String(value.clone())))
        .collect();
    let mut asked = json!({
        "event": params.event,
        "stdin_b64": params.stdin_b64,
        "env": env,
    });
    if let Some(cwd) = &params.cwd {
        asked["cwd"] = Value::String(cwd.clone());
    }
    let accept = params
        .accept_ms
        .map_or(ACCEPT_DEFAULT, Duration::from_millis)
        .min(ACCEPT_MOST);
    let deadline = params
        .deadline_ms
        .map_or(DEADLINE_DEFAULT, Duration::from_millis)
        .min(DEADLINE_MOST);
    let asked = asked.as_object().cloned().unwrap_or_default();
    match daemon.brain().call("hook", asked, accept, deadline).await {
        Ok(result) => match HookReply::deserialize(Value::Object(result)) {
            Ok(reply) => Ok(object(&reply)),
            Err(e) => Err(error(
                hook_codes::HOOK_FAILED,
                &format!("the brain's reply is not a hook's: {e}"),
            )),
        },
        Err(CallError::NotDelivered(why)) => Err(with_brain(
            hook_codes::HOOK_NOT_DELIVERED,
            &format!("the brain never took the hook: {}", why.message),
            &why,
        )),
        Err(CallError::Failed(why)) => Err(with_brain(
            hook_codes::HOOK_FAILED,
            &format!("the brain took the hook, then failed: {}", why.message),
            &why,
        )),
        Err(CallError::TimedOut) => Err(error(
            hook_codes::HOOK_TIMED_OUT,
            &format!(
                "the brain took the hook and did not finish it within {} ms",
                deadline.as_millis()
            ),
        )),
    }
}

/// Publishes the hook's `hook.<event>`.
fn publish(daemon: &Daemon, params: &HookParams, stdin: &[u8]) {
    let Some(r#type) = hook_event_type(&params.event) else {
        return;
    };
    let payload: Value = serde_json::from_slice(stdin).unwrap_or(Value::Null);
    let text = |field: &str| {
        payload
            .get(field)
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let session_id = text("session_id").unwrap_or_default();
    let cwd = text("cwd")
        .or_else(|| params.cwd.clone())
        .unwrap_or_default();
    let mut data = HookEventData::new(session_id, cwd);
    if let Some(run_id) = params.run_id.clone() {
        match data.clone().with_run(run_id) {
            Ok(with_run) => data = with_run,
            Err(e) => warning!("hook: the run id was not kept: {e}"),
        }
    }
    if let Err(e) = daemon.bus().publish(topics::HOOK, &r#type, object(&data)) {
        warning!("{} was not published: {e}", r#type);
    }
}

/// A reply carrying `stdout` alone.
fn reply(stdout: &[u8]) -> Data {
    object(&json!({
        "stdout_b64": b64::encode(stdout),
        "stderr_b64": "",
        "exit_code": 0,
    }))
}

fn object<T: serde::Serialize>(value: &T) -> Data {
    match serde_json::to_value(value) {
        Ok(Value::Object(data)) => data,
        _ => Data::new(),
    }
}

fn error(code: &str, message: &str) -> ErrorBody {
    ErrorBody::new(code, message).unwrap_or_else(|_| {
        ErrorBody::new(codes::INTERNAL, "an error with no message").expect("a valid error")
    })
}

/// `code`'s error, saying in `detail.brain` what the brain's supervisor said.
fn with_brain(code: &str, message: &str, why: &ErrorBody) -> ErrorBody {
    let detail = json!({"brain": why.code});
    let base = error(code, message);
    match detail {
        Value::Object(fields) => base.clone().with_detail(fields).unwrap_or(base),
        _ => base,
    }
}
