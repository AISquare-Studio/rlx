//! `hook` (handoff 0.10.3, 1.5): one Claude Code hook, from rlxd-hook.
//!
//! - **Its home.** A hook whose own rlx home (by `rlxd_client::Home`'s rules, on the
//!   variables rlxd-hook sent) is not this rlxd's is `hook_not_delivered`, so rlxd-hook runs
//!   `rlx hook` in its own; and nothing is published here. Only a `cli` client may call
//!   `hook` (`role_not_allowed`).
//! - **The event.** Every other `hook` that parses publishes one `hook.<event>` on the
//!   `hook` topic first, `{session_id, cwd, run_id?}` (7.3): the session and directory from the
//!   hook's JSON (the directory rlxd-hook ran in when the JSON names none).
//! - **The fleet's six** go to the resident brain (`crate::brain`), which runs `rlx hook
//!   <event>`'s own code with the hook's stdin, directory and allowlisted environment
//!   (`hook_env_forwarded`; any other name is dropped here), and answers the bytes for
//!   rlxd-hook's stdout and stderr. The brain must accept it within `accept_ms` (5 s
//!   unless rlxd-hook says less) and finish it within `deadline_ms`.
//! - **Python's start.** What Python reads only as a process starts, and which shapes a
//!   hook's stdio and text files (`PYTHONIOENCODING`, `PYTHONUTF8`), is the brain's own:
//!   rlxd's, which started it. A fleet hook whose values differ cannot run in the brain as
//!   `rlx hook` would, so it is `hook_not_delivered` too, and runs as `rlx hook`.
//! - **The fallback line** is the brain's `accepted`. A hook the brain never accepted
//!   fails with `hook_not_delivered`: nothing of it ran, and rlxd-hook runs `rlx hook
//!   <event>` itself. Once accepted, a failure is `hook_failed` and an overrun
//!   `hook_timeout`, and rlxd-hook runs nothing more: the prompt is never logged twice.
//! - **`pre-tool-use`** is the gate's (5.1), and never reaches the brain. Its run, attempt
//!   and key (`hook_key`), with the process that connected (the connection's peer, pinned
//!   when it was accepted), go to [`crate::runner::Runner::pre_tool_use`], which binds them
//!   to the run's live attempt and answers allow or deny; one that names no run, no
//!   attempt or no key, or whose stdin is not JSON, is denied here. The key is never logged,
//!   published or forwarded.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rlxd_client::Home;
use rlxd_proto::{
    Data, ErrorBody, HookEventData, HookParams, HookReply, PRE_TOOL_USE, b64, codes, deny_output,
    hook_codes, hook_env_forwarded, hook_event_type, topics,
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

/// Why a `pre-tool-use` that names no run, attempt or key is denied.
const UNBOUND: &str = "rlxd refused the hook request: it names no run, attempt or hook key";

/// Why a `pre-tool-use` whose stdin is not JSON is denied.
const NOT_JSON: &str = "rlxd refused the hook request: its stdin is not JSON";

/// `hook` from the process `peer`: publishes its event, then runs it, as the module says.
pub async fn hook(
    daemon: &Daemon,
    peer: Arc<crate::peer::Peer>,
    params: Data,
) -> Result<Data, ErrorBody> {
    let params = HookParams::deserialize(Value::Object(params)).map_err(|e| {
        error(
            codes::INVALID_PARAMS,
            &format!("hook's params are not valid: {e}"),
        )
    })?;
    if let Err(why) = same_home(daemon.home(), &params) {
        return Err(error(hook_codes::HOOK_NOT_DELIVERED, &why));
    }
    let stdin = b64::decode(&params.stdin_b64).unwrap_or_default();
    publish(daemon, &params, &stdin);
    if params.event == PRE_TOOL_USE {
        return pre_tool_use(daemon, peer, params, &stdin).await;
    }
    if let Err(why) = same_start(&params.env, |name| std::env::var(name).ok()) {
        return Err(error(hook_codes::HOOK_NOT_DELIVERED, &why));
    }
    let env: serde_json::Map<String, Value> = params
        .env
        .iter()
        .filter(|(name, _)| hook_env_forwarded(name))
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
        // Either the brain took it and ran past its deadline, or it stalled with the hook
        // taken before rlxd read that it had (its accept wait, then a kill): both may have
        // run it.
        Err(CallError::TimedOut) => Err(error(
            hook_codes::HOOK_TIMED_OUT,
            &format!(
                "the brain took the hook and did not answer: it stalled, or ran past the {} ms \
                 it had",
                deadline.as_millis()
            ),
        )),
    }
}

/// A `pre-tool-use`, to the gate: its answer, allow or deny.
async fn pre_tool_use(
    daemon: &Daemon,
    peer: Arc<crate::peer::Peer>,
    params: HookParams,
    stdin: &[u8],
) -> Result<Data, ErrorBody> {
    let (Some(run_id), Some(attempt), Some(key)) = (params.run_id, params.attempt, params.hook_key)
    else {
        return Ok(reply(deny_output(UNBOUND).as_bytes()));
    };
    let Ok(stdin) = serde_json::from_slice::<Value>(stdin) else {
        return Ok(reply(deny_output(NOT_JSON).as_bytes()));
    };
    let ask = crate::runner::Ask {
        run_id,
        attempt,
        key,
        peer,
        stdin,
    };
    match daemon.runner().pre_tool_use(ask).await {
        Some(output) => Ok(reply(output.as_bytes())),
        // Test builds' dropped hook connections: no answer, which rlxd-hook denies.
        None => Err(error(
            hook_codes::HOOK_FAILED,
            "the hook's connection was dropped before the call was decided",
        )),
    }
}

/// Whether the hook's own rlx home, by `rlxd_client::Home`'s rules on the variables
/// rlxd-hook sent (`RLX_HOME`, else `AISQUARE_HOME`, else `.aisquare` in its user's home;
/// a relative one taken from its directory), is the home this rlxd serves. Else why not:
/// its brain would act on another home than `rlx hook` would. So is a hook whose
/// `RLX_HOME` and `AISQUARE_HOME` name different homes: rlx reads `AISQUARE_HOME` alone
/// before 0.10.2's sync, `RLX_HOME` first after it.
fn same_home(own: &Home, params: &HookParams) -> Result<(), String> {
    let var = |name: &str| {
        params
            .env
            .get(name)
            .filter(|value| !value.is_empty())
            .map(OsString::from)
    };
    let user = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let user_home = var(user).map(PathBuf::from).or_else(std::env::home_dir);
    let cwd = params.cwd.as_deref().map(Path::new);
    let resolve = |rlx: Option<OsString>, legacy: Option<OsString>| {
        Home::from_values(rlx, legacy, user_home.clone())
            .map(|home| real(home.root(), cwd))
            .map_err(|e| e.to_string())
    };
    let theirs = resolve(var("RLX_HOME"), var("AISQUARE_HOME"))?;
    if var("RLX_HOME").is_some()
        && var("AISQUARE_HOME").is_some()
        && resolve(None, var("AISQUARE_HOME"))? != theirs
    {
        return Err(
            "the hook's RLX_HOME and AISQUARE_HOME name different homes, so rlx's \
             releases would disagree on its home: run as `rlx hook`, which picks its own"
                .to_owned(),
        );
    }
    let ours = real(own.root(), None);
    if theirs == ours {
        Ok(())
    } else {
        Err(format!(
            "the hook's rlx home is {}, not this rlxd's ({}): run as `rlx hook`, in its own",
            theirs.display(),
            ours.display()
        ))
    }
}

/// The variables Python reads only as a process starts that shape what a hook writes and
/// reads: its stdio's encoding (`PYTHONIOENCODING`) and UTF-8 mode (`PYTHONUTF8`).
const READ_AT_START: &[&str] = &["PYTHONIOENCODING", "PYTHONUTF8"];

/// Whether the hook's [`READ_AT_START`] variables are the brain's (`own`: rlxd's, which the
/// brain started with). Else why not: the brain's streams and files would not be the ones
/// `rlx hook` makes.
fn same_start(
    hook: &std::collections::BTreeMap<String, String>,
    own: impl Fn(&str) -> Option<String>,
) -> Result<(), String> {
    for &name in READ_AT_START {
        let theirs = hook.get(name).map(String::as_str);
        if theirs != own(name).as_deref() {
            return Err(format!(
                "the hook's {name} is not the brain's, which Python reads only as it starts: \
                 run as `rlx hook`, which starts with it"
            ));
        }
    }
    Ok(())
}

/// `path`, joined to `cwd` when relative, as the OS resolves it (links and `..` gone)
/// when it exists.
fn real(path: &Path, cwd: Option<&Path>) -> PathBuf {
    let path = match cwd {
        Some(cwd) if path.is_relative() => cwd.join(path),
        _ => path.to_path_buf(),
    };
    std::fs::canonicalize(&path).unwrap_or(path)
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn rlxd_hook_sends_no_frame_longer_than_rlxd_reads() {
        assert_eq!(rlxd_proto::HOOK_FRAME_MOST, crate::server::MAX_FRAME);
    }

    #[test]
    fn a_hook_python_starts_otherwise_is_not_the_brains() {
        let hook = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|&(name, value)| (name.to_owned(), value.to_owned()))
                .collect()
        };
        let none: fn(&str) -> Option<String> = |_| None;
        let utf8: fn(&str) -> Option<String> =
            |name| (name == "PYTHONUTF8").then(|| "1".to_owned());
        assert!(same_start(&hook(&[("PATH", "/bin")]), none).is_ok());
        assert!(same_start(&hook(&[("PYTHONUTF8", "1")]), utf8).is_ok());
        for (theirs, own) in [
            (hook(&[("PYTHONUTF8", "1")]), none),
            (hook(&[]), utf8),
            (hook(&[("PYTHONUTF8", "0")]), utf8),
            (hook(&[("PYTHONIOENCODING", "latin-1")]), none),
        ] {
            assert!(same_start(&theirs, own).is_err(), "{theirs:?}");
        }
    }
}
