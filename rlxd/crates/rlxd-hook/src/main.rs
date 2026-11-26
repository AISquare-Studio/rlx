//! rlxd-hook, the tiny client Claude Code runs for each hook: it hands the hook to rlxd over
//! the local socket, so no Python process starts per hook (handoff 0.10.3, 1.5).
//!
//! ```text
//! rlxd-hook <event>                         one of Claude Code's six fleet hooks
//! rlxd-hook pre-tool-use [--run R --attempt N]   the gate's hook (5.1)
//! rlxd-hook ping                            rlxd and its brain answer, for the speed check
//! ```
//!
//! For a fleet hook (`session-start`, `user-prompt-submit`, `session-end`, `stop`,
//! `stop-failure`, `notification`), it reads the hook's JSON on stdin and sends rlxd
//! `hook`: those bytes, its working directory, and the variables of rlxd-proto's
//! `HOOK_ENV_NAMES` it has. It says `hello` as a `cli` client named `rlxd-hook`, with the
//! token every client uses, and sends nothing but `hook` (or `brain.ping`), for an event of
//! its fixed list: no hook reaches a gate or app-only method. It writes rlxd's reply, the
//! bytes `rlx hook <event>` would write, on its stdout and stderr, and exits 0.
//!
//! **The fallback line.** rlxd-hook runs `rlx hook <event>` itself, with the same stdin
//! bytes, only when the hook certainly never ran:
//! - before it starts writing the `hook` request: no home, no token, rlxd not running, a
//!   refused hello, a hook too big for one frame, or its own timer ending first;
//! - after, only when rlxd answers `hook_not_delivered`: the brain never accepted it.
//!
//! Anything else after the request starts out (rlxd's `hook_failed` or `hook_timeout`, a
//! dropped connection, a reply it cannot read, or its own timer) may come after the brain
//! ran the hook, so rlxd-hook runs nothing more, prints nothing on stdout, and exits 0: a
//! prompt is never logged twice. The `rlx` it runs is the one beside rlxd-hook, else in the
//! install folders, else in an absolute folder on `PATH` (`rlxd here`'s lookup), or
//! `RLXD_RLX_BIN`: never a bare name.
//!
//! **Its timer** ends below Claude Code's own timeout for the hook as rlx installs it:
//! 120 s for `session-start` and `user-prompt-submit`, 10 s for `session-end`, 60 s for the
//! rest. rlxd is asked to have the brain accept the hook within a quarter of that (5 s at
//! most) and to answer 5 s before it ends.
//!
//! **`pre-tool-use`** never falls back (`rlx hook` has no such handler): it prints a deny
//! and exits 0 on any failure, and rlxd itself denies every call until the gate decides
//! them (5.1). Every hook exits 0.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use rlxd_client::proto::{
    Data, HOOK_ENV_NAMES, HOOK_EVENTS, HelloParams, HookParams, HookReply, PRE_TOOL_USE, Role,
    VERSION, b64, deny_output, hook_codes, methods,
};
use rlxd_client::{CallError, ConnectError, Connection, Home, install};
use serde::Deserialize;

const USAGE: &str = "usage: rlxd-hook <event> | pre-tool-use [--run R --attempt N] | ping

Hands a Claude Code hook to rlxd, or runs `rlx hook <event>` when rlxd cannot take it.
<event> is one of session-start, user-prompt-submit, session-end, stop,
stop-failure, notification. `ping` checks that rlxd and its brain answer.
";

/// The largest hook rlxd-hook sends: its base64 and the rest stay under rlxd's 1 MiB frame.
const MOST_STDIN: usize = 640 << 10;

/// Why a `pre-tool-use` hook rlxd-hook could not ask rlxd about is denied.
const UNREACHED: &str = "rlxd could not be asked about this tool call, so it is denied";

/// Why a hook went to `rlx hook` when no rlxd serves the home: today's path, said nowhere.
const NOT_RUNNING: &str = "rlxd is not running";

/// How long `ping` waits for rlxd.
const PING_WAIT: Duration = Duration::from_secs(10);

/// Where the hook's request stands, which the timer and the asking thread race to change.
const READY: u8 = 0;
const SENDING: u8 = 1;
const GAVE_UP: u8 = 2;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let words: Vec<&str> = args.iter().filter_map(|arg| arg.to_str()).collect();
    if words.len() != args.len() {
        return usage();
    }
    match words.as_slice() {
        ["--version" | "-V"] => print(&format!("rlxd-hook {VERSION}\n")),
        ["--help" | "-h"] => print(USAGE),
        ["ping"] => ping(),
        [PRE_TOOL_USE, rest @ ..] => match run_of(rest) {
            Some(run) => pre_tool_use(run),
            None => usage(),
        },
        [event] if HOOK_EVENTS.contains(event) => fleet_hook(event),
        _ => usage(),
    }
}

fn usage() -> ExitCode {
    eprint!("{USAGE}");
    ExitCode::from(2)
}

/// Prints `text` on stdout, and fails rather than panics when stdout is closed.
fn print(text: &str) -> ExitCode {
    if write_out(text.as_bytes()) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn write_out(bytes: &[u8]) -> bool {
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(bytes)
        .and_then(|()| stdout.flush())
        .is_ok()
}

fn write_err(bytes: &[u8]) {
    let mut stderr = io::stderr().lock();
    let _ = stderr.write_all(bytes).and_then(|()| stderr.flush());
}

fn say(message: &str) {
    write_err(format!("rlxd-hook: {message}\n").as_bytes());
}

/// `--run R --attempt N`, both or neither: `Some(None)` for neither.
fn run_of(args: &[&str]) -> Option<Option<(String, u64)>> {
    match args {
        [] => Some(None),
        ["--run", run, "--attempt", attempt] | ["--attempt", attempt, "--run", run] => {
            let attempt = attempt.parse().ok().filter(|&n: &u64| n >= 1)?;
            (!run.is_empty()).then(|| Some(((*run).to_owned(), attempt)))
        }
        _ => None,
    }
}

/// Claude Code's timeout for the hook as rlx installs it, less a margin: rlxd-hook's own.
fn budget(event: &str) -> Duration {
    let installed = match event {
        "session-start" | "user-prompt-submit" => 120,
        "session-end" => 10,
        _ => 60,
    };
    Duration::from_secs(installed - 2)
}

/// What rlxd-hook asks rlxd for this hook: when the brain must accept it, and when rlxd
/// must have answered, in milliseconds.
fn times(event: &str) -> (u64, u64) {
    let budget = budget(event).as_millis() as u64;
    let accept = (budget / 4).min(5_000);
    let deadline = budget.saturating_sub(5_000).max(accept);
    (accept, deadline)
}

/// How asking rlxd ended.
enum Asked {
    /// rlxd answered the hook.
    Answered(HookReply),
    /// The hook never ran: rlxd-hook may run `rlx hook` itself. Why.
    NeverRan(String),
    /// The hook may have run: rlxd-hook runs nothing more. Why.
    MayHaveRun(String),
}

/// A fleet hook, as the module says.
fn fleet_hook(event: &str) -> ExitCode {
    let mut stdin = Vec::new();
    if let Err(e) = io::stdin().read_to_end(&mut stdin) {
        say(&format!("cannot read the hook's stdin: {e}"));
    }
    match ask_in_time(event, &stdin, None, budget(event)) {
        Asked::Answered(reply) => emit(&reply),
        Asked::NeverRan(why) => {
            // rlxd not running is today's path, and says nothing.
            if why != NOT_RUNNING {
                say(&format!("{why}; running `rlx hook {event}` instead"));
            }
            fall_back(event, &stdin);
        }
        Asked::MayHaveRun(why) => say(&format!("{why}; the hook is not run again")),
    }
    ExitCode::SUCCESS
}

/// The gate's hook: rlxd's answer, or a deny on any failure.
fn pre_tool_use(run: Option<(String, u64)>) -> ExitCode {
    let mut stdin = Vec::new();
    let _ = io::stdin().read_to_end(&mut stdin);
    match ask_in_time(PRE_TOOL_USE, &stdin, run, budget(PRE_TOOL_USE)) {
        Asked::Answered(reply) if decoded(&reply.stdout_b64).is_some() => emit(&reply),
        Asked::Answered(_) | Asked::NeverRan(_) | Asked::MayHaveRun(_) => {
            write_out(deny_output(UNREACHED).as_bytes());
        }
    }
    ExitCode::SUCCESS
}

fn decoded(text: &str) -> Option<Vec<u8>> {
    b64::decode(text)
}

/// Writes the reply's bytes, stdout then stderr.
fn emit(reply: &HookReply) {
    if let Some(bytes) = decoded(&reply.stdout_b64) {
        write_out(&bytes);
    }
    if let Some(bytes) = decoded(&reply.stderr_b64)
        && !bytes.is_empty()
    {
        write_err(&bytes);
    }
}

/// Asks rlxd about the hook, within `budget`, on a thread of its own: past the budget, the
/// hook never ran if the request had not started out, and may have otherwise.
fn ask_in_time(event: &str, stdin: &[u8], run: Option<(String, u64)>, budget: Duration) -> Asked {
    if stdin.len() > MOST_STDIN {
        return Asked::NeverRan(format!(
            "the hook's {} bytes are more than rlxd takes in one frame",
            stdin.len()
        ));
    }
    let params = match params_of(event, stdin, run) {
        Ok(params) => params,
        Err(why) => return Asked::NeverRan(why),
    };
    let stage = Arc::new(AtomicU8::new(READY));
    let (said, answer) = mpsc::channel();
    let asking = Arc::clone(&stage);
    thread::spawn(move || {
        let _ = said.send(ask(params, &asking));
    });
    match answer.recv_timeout(budget) {
        Ok(asked) => asked,
        Err(_) => {
            let gave_up = stage
                .compare_exchange(READY, GAVE_UP, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok();
            let waited = format!("rlxd did not answer within {} s", budget.as_secs());
            if gave_up {
                Asked::NeverRan(waited)
            } else {
                Asked::MayHaveRun(waited)
            }
        }
    }
}

/// `hook`'s params for this hook.
fn params_of(event: &str, stdin: &[u8], run: Option<(String, u64)>) -> Result<Data, String> {
    let env: BTreeMap<String, String> = HOOK_ENV_NAMES
        .iter()
        .filter_map(|&name| {
            let value = std::env::var(name).ok()?;
            Some((name.to_owned(), value))
        })
        .collect();
    let cwd = std::env::current_dir()
        .ok()
        .map(|dir| dir.display().to_string())
        .filter(|dir| !dir.is_empty());
    let (accept, deadline) = times(event);
    let mut params = HookParams::new(event, b64::encode(stdin), cwd, env)
        .and_then(|params| params.with_times(accept, deadline))
        .map_err(|e| format!("the hook cannot be sent: {e}"))?;
    if let Some((run_id, attempt)) = run {
        params = params
            .with_run(run_id, attempt)
            .map_err(|e| format!("the hook cannot be sent: {e}"))?;
    }
    match serde_json::to_value(&params) {
        Ok(serde_json::Value::Object(data)) => Ok(data),
        _ => Err("the hook cannot be sent".to_owned()),
    }
}

/// Connects, says hello and sends `hook`, unless the timer gave up first: what came of it.
fn ask(params: Data, stage: &AtomicU8) -> Asked {
    let mut connection = match connect() {
        Ok(connection) => connection,
        Err(why) => return Asked::NeverRan(why),
    };
    if stage
        .compare_exchange(READY, SENDING, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Asked::NeverRan("the timer ended first".to_owned());
    }
    // From here on, rlxd may have the request: only its word says the hook never ran.
    let reply = match connection.request(methods::HOOK, params) {
        Ok(reply) => reply,
        Err(e) => return Asked::MayHaveRun(format!("no reply from rlxd: {e}")),
    };
    if let Some(data) = reply.data() {
        return match HookReply::deserialize(serde_json::Value::Object(data.clone())) {
            Ok(answer) => Asked::Answered(answer),
            Err(e) => Asked::MayHaveRun(format!("rlxd's reply is not a hook's: {e}")),
        };
    }
    match reply.error() {
        Some(error) if error.code == hook_codes::HOOK_NOT_DELIVERED => {
            Asked::NeverRan(format!("rlxd's brain did not take it: {}", error.message))
        }
        Some(error) => Asked::MayHaveRun(format!("rlxd said {}: {}", error.code, error.message)),
        None => Asked::MayHaveRun("rlxd's reply says nothing".to_owned()),
    }
}

/// A connection to this home's rlxd, past its hello: why not, when there is none.
fn connect() -> Result<Connection, String> {
    let home = Home::from_env().map_err(|e| e.to_string())?;
    let mut connection = Connection::open(&home).map_err(|e| match e {
        ConnectError::NotRunning { .. } => NOT_RUNNING.to_owned(),
        other => other.to_string(),
    })?;
    let token = std::fs::read_to_string(home.token())
        .map_err(|e| format!("cannot read {}: {e}", home.token().display()))?;
    let hello = HelloParams::new(token.trim(), "rlxd-hook", VERSION, Role::Cli)
        .map_err(|e| format!("cannot say hello: {e}"))?;
    let reply = connection.hello(&hello).map_err(|e| e.to_string())?;
    if let Some(error) = reply.error() {
        return Err(format!("rlxd refused the hello: {}", error.message));
    }
    Ok(connection)
}

/// Runs `rlx hook <event>` with the hook's stdin, its stdout and stderr this process's own.
fn fall_back(event: &str, stdin: &[u8]) {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from));
    let Some(rlx) = install::rlx_to_run(beside.as_deref()) else {
        say("no rlx was found beside rlxd-hook, in the install folders or on PATH");
        return;
    };
    let child = Command::new(&rlx)
        .args(["hook", event])
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            say(&format!("cannot run {}: {e}", rlx.display()));
            return;
        }
    };
    if let Some(mut pipe) = child.stdin.take() {
        let _ = pipe.write_all(stdin);
    }
    let _ = child.wait();
}

/// `rlxd-hook ping`: rlxd and its brain answer, through the brain's queue.
fn ping() -> ExitCode {
    let (said, answer) = mpsc::channel();
    thread::spawn(move || {
        let outcome = connect().and_then(|mut connection| {
            connection
                .request(methods::BRAIN_PING, Data::new())
                .map_err(|e: CallError| e.to_string())
        });
        let _ = said.send(outcome);
    });
    match answer.recv_timeout(PING_WAIT) {
        Ok(Ok(reply)) if reply.is_ok() => ExitCode::SUCCESS,
        Ok(Ok(reply)) => {
            let message = reply.error().map_or_else(
                || reply.line.clone(),
                |e| format!("{}: {}", e.code, e.message),
            );
            say(&message);
            ExitCode::FAILURE
        }
        Ok(Err(why)) => {
            say(&why);
            ExitCode::from(3)
        }
        Err(_) => {
            say(&format!("no answer within {} s", PING_WAIT.as_secs()));
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_timer_ends_below_claude_codes_timeout_for_the_hook() {
        assert_eq!(budget("session-start"), Duration::from_secs(118));
        assert_eq!(budget("session-end"), Duration::from_secs(8));
        assert_eq!(budget("stop"), Duration::from_secs(58));
        assert_eq!(times("session-start"), (5_000, 113_000));
        assert_eq!(times("session-end"), (2_000, 3_000));
        for event in HOOK_EVENTS {
            let (accept, deadline) = times(event);
            // rlxd answers by the deadline plus 4 s (an ack wait and a kill): inside the
            // timer, which leaves the fallback the rest.
            assert!(
                deadline + 4_000 < budget(event).as_millis() as u64,
                "{event}"
            );
            assert!(accept <= deadline, "{event}");
        }
    }

    #[test]
    fn a_run_is_named_whole_or_not_at_all() {
        assert_eq!(run_of(&[]), Some(None));
        assert_eq!(
            run_of(&["--run", "r1", "--attempt", "2"]),
            Some(Some(("r1".to_owned(), 2)))
        );
        assert_eq!(
            run_of(&["--attempt", "1", "--run", "r1"]),
            Some(Some(("r1".to_owned(), 1)))
        );
        for bad in [
            &["--run", "r1"][..],
            &["--run", "", "--attempt", "1"],
            &["--run", "r1", "--attempt", "0"],
            &["--run", "r1", "--attempt", "x"],
        ] {
            assert_eq!(run_of(bad), None, "{bad:?}");
        }
    }
}
