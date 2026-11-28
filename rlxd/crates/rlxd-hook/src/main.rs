//! rlxd-hook, the tiny client Claude Code runs for each hook: it hands the hook to rlxd over
//! the local socket, so no Python process starts per hook (handoff 0.10.3, 1.5).
//!
//! ```text
//! rlxd-hook <event>                         one of Claude Code's six fleet hooks
//! rlxd-hook pre-tool-use --run R --attempt N [--deadline-ms D]   the gate's hook (5.1)
//! rlxd-hook ping                            rlxd and its brain answer, for the speed check
//! ```
//!
//! For a fleet hook (`session-start`, `user-prompt-submit`, `session-end`, `stop`,
//! `stop-failure`, `notification`), it reads the hook's JSON on stdin and sends rlxd
//! `hook`: those bytes, its working directory, and the variables it has of rlxd-proto's
//! `HOOK_ENV_NAMES`, by exact name. It says `hello` as a `cli` client named `rlxd-hook`, with the
//! token every client uses, and sends nothing but `hook` (or `hook.ping`), for an event of
//! its fixed list: no hook reaches a gate or app-only method. It writes rlxd's reply, the
//! bytes `rlx hook <event>` would write, on its stdout and stderr, and exits 0.
//!
//! **The fallback line.** rlxd-hook runs `rlx hook <event>` itself, with the same stdin
//! bytes, only when the hook certainly never ran:
//! - before it starts writing the `hook` request: no home, no token, rlxd not running, a
//!   refused hello, its own timer ending first, or a hook it cannot send as it is: a
//!   request longer than rlxd reads in one frame (`HOOK_FRAME_MOST`: the stdin in base64
//!   with the variables), a variable longer than `HOOK_ENV_VALUE_MOST`, or a directory or
//!   variable that is not UTF-8, which JSON cannot carry unchanged;
//! - after, only when rlxd answers that it never handed the hook to the brain:
//!   `hook_not_delivered` (the brain never accepted it), or a refusal of the request
//!   itself (`invalid_request`, `invalid_params`, `unknown_method`, `role_not_allowed`).
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
//! most) and to let it run until 1 s before the timer ends: a `session-end` gets 7 s, past
//! the store's 5 s wait for a busy database, as `rlx hook` does. A hook still running then
//! may have run, so rlxd-hook runs nothing more, whether rlxd's `hook_timeout` or its own
//! timer comes first.
//!
//! **`pre-tool-use`** is the gate's (5.1), and never falls back (`rlx hook` has no such
//! handler). The run's settings name it with the run, its attempt and the run's
//! `deadline_ms`; it reads the attempt's key from its own `RLXD_HOOK_KEY`, and sends it as
//! the request's `hook_key` only (never with the variables, which never hold it, nor on
//! stdout or stderr). rlxd's gate binds the request to the run's live attempt by the key and
//! by this process, which must be inside the run, and answers allow or deny.
//! - With no run or no key, it asks nothing: it prints a deny.
//! - Its timer is the run's `deadline_ms` and 5 s (the default `deadline_ms` when the
//!   command line names none): past the gate's own deadline, which denies the call
//!   `by: "timeout"`, and below Claude Code's timeout for the hook, which rlxd writes as
//!   `deadline_ms` and 10 s.
//! - It prints the gate's answer only, rebuilt from the decision and the reason it read
//!   (`allow_output` or `deny_output`), never rlxd's bytes as they came nor anything on
//!   stderr, and only when rlxd's reply is that answer and nothing else (`is_gate_output`).
//!   Anything else (no rlxd, a refusal, a dropped connection, a reply it cannot read, an
//!   empty one, its timer) prints a deny, so nothing but the gate's allow lets a call
//!   through.
//!
//! Every hook exits 0, a usage error too (Claude Code reads 2 as a blocking error): it says
//! why on stderr, and `pre-tool-use` still prints its deny.

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
    DEFAULT_DEADLINE_MS, Data, HOOK_ENV_NAMES, HOOK_EVENTS, HOOK_FRAME_MOST, HOOK_KEY_ENV,
    HelloParams, HookParams, HookReply, PRE_TOOL_USE, Role, VERSION, allow_output, b64, codes,
    deny_output, hook_codes, is_gate_output, methods,
};
use rlxd_client::{CallError, ConnectError, Connection, Home, install};
use serde::Deserialize;

const USAGE: &str =
    "usage: rlxd-hook <event> | pre-tool-use --run R --attempt N [--deadline-ms D] | ping

Hands a Claude Code hook to rlxd, or runs `rlx hook <event>` when rlxd cannot take it.
<event> is one of session-start, user-prompt-submit, session-end, stop,
stop-failure, notification. `ping` checks that rlxd and its brain answer.
";

/// Room in a `hook` request's line for what is not its params: the envelope's id, method
/// and schema version.
const ENVELOPE_ROOM: usize = 256;

/// Why a `pre-tool-use` hook run with a command line rlxd-hook does not take is denied.
const MISRUN: &str = "rlxd-hook was run with arguments it does not take, so the call is denied";

/// Why a `pre-tool-use` hook rlxd-hook could not ask rlxd about is denied.
const UNREACHED: &str = "rlxd could not be asked about this tool call, so it is denied";

/// Why a `pre-tool-use` hook whose command line names no run is denied.
const NO_RUN: &str =
    "rlxd-hook pre-tool-use was given no --run and --attempt, so the call is denied";

/// Why a `pre-tool-use` hook with no key is denied.
const NO_KEY: &str = "rlxd-hook pre-tool-use has no RLXD_HOOK_KEY, so the call is denied";

/// What a `pre-tool-use`'s timer adds to the run's `deadline_ms`: rlxd writes Claude Code's
/// timeout for the hook as `deadline_ms` and 10 s.
const GATE_SLACK: Duration = Duration::from_secs(5);

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
        // The gate's hook still denies, whatever else its command line holds: an exit 0
        // with nothing printed would let Claude Code ask no one.
        if args.first().is_some_and(|first| first == PRE_TOOL_USE) {
            write_out(deny_output(MISRUN).as_bytes());
        }
        return usage();
    }
    match words.as_slice() {
        ["--version" | "-V"] => print(&format!("rlxd-hook {VERSION}\n")),
        ["--help" | "-h"] => print(USAGE),
        ["ping"] => ping(),
        [PRE_TOOL_USE, rest @ ..] => match run_of(rest) {
            Some(run) => pre_tool_use(run),
            None => {
                write_out(deny_output(MISRUN).as_bytes());
                usage()
            }
        },
        [event] if HOOK_EVENTS.contains(event) => fleet_hook(event),
        _ => usage(),
    }
}

/// A command line rlxd-hook does not take: why on stderr, and exit 0, as every hook does
/// (Claude Code reads exit 2 as a blocking error).
fn usage() -> ExitCode {
    eprint!("{USAGE}");
    ExitCode::SUCCESS
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

/// A run's tool call, as a `pre-tool-use` command line names it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Run {
    id: String,
    attempt: u64,
    /// The run's `deadline_ms`, when the command line says it.
    deadline_ms: Option<u64>,
}

/// `--run R --attempt N [--deadline-ms D]`, in any order, `--run` and `--attempt` both or
/// none of them: `Some(None)` for none. `None` for anything else.
fn run_of(args: &[&str]) -> Option<Option<Run>> {
    if args.is_empty() {
        return Some(None);
    }
    let (mut id, mut attempt, mut deadline_ms) = (None, None, None);
    for pair in args.chunks(2) {
        let [flag, value] = pair else {
            return None;
        };
        let slot = match *flag {
            "--run" => &mut id,
            "--attempt" => &mut attempt,
            "--deadline-ms" => &mut deadline_ms,
            _ => return None,
        };
        if slot.replace(*value).is_some() {
            return None;
        }
    }
    let id = id.filter(|id| !id.is_empty())?.to_owned();
    let attempt = attempt?.parse().ok().filter(|&n: &u64| n >= 1)?;
    let deadline_ms = match deadline_ms {
        Some(ms) => Some(ms.parse().ok().filter(|&n: &u64| n >= 1)?),
        None => None,
    };
    Some(Some(Run {
        id,
        attempt,
        deadline_ms,
    }))
}

/// A `pre-tool-use`'s timer: the run's `deadline_ms` (the default when unsaid) and
/// [`GATE_SLACK`].
fn gate_budget(deadline_ms: Option<u64>) -> Duration {
    Duration::from_millis(deadline_ms.unwrap_or(DEFAULT_DEADLINE_MS)).saturating_add(GATE_SLACK)
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

/// What rlxd-hook asks rlxd for a hook whose timer is `budget`: when the brain must accept
/// it, and when rlxd must have answered, in milliseconds.
fn times_within(budget: Duration) -> (u64, u64) {
    let budget = u64::try_from(budget.as_millis()).unwrap_or(u64::MAX);
    let accept = (budget / 4).min(5_000);
    let deadline = budget.saturating_sub(1_000).max(accept);
    (accept, deadline)
}

/// rlxd's refusals of a request itself, before it hands anything to the brain: the hook
/// never ran.
const NEVER_HANDED_ON: &[&str] = &[
    codes::INVALID_REQUEST,
    codes::INVALID_PARAMS,
    codes::UNKNOWN_METHOD,
    codes::ROLE_NOT_ALLOWED,
];

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
    match ask_in_time(event, &stdin, None, None, budget(event)) {
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

/// The gate's hook: the gate's answer, or a deny on anything else. Nothing it writes, on
/// any path, holds the key.
fn pre_tool_use(run: Option<Run>) -> ExitCode {
    let mut stdin = Vec::new();
    let _ = io::stdin().read_to_end(&mut stdin);
    let Some(run) = run else {
        write_out(deny_output(NO_RUN).as_bytes());
        return ExitCode::SUCCESS;
    };
    // Text or not at all: a key that is not UTF-8 is none.
    let Some(key) = std::env::var(HOOK_KEY_ENV)
        .ok()
        .filter(|key| !key.is_empty())
    else {
        write_out(deny_output(NO_KEY).as_bytes());
        return ExitCode::SUCCESS;
    };
    let budget = gate_budget(run.deadline_ms);
    // Only the gate's answer goes through, rebuilt: any other reply, an empty one included
    // (on which Claude Code asks no one), is a deny, and rlxd's stderr is never written.
    let answer = match ask_in_time(
        PRE_TOOL_USE,
        &stdin,
        Some((run.id, run.attempt)),
        Some(key),
        budget,
    ) {
        Asked::Answered(reply) => {
            decoded(&reply.stdout_b64).and_then(|stdout| gate_answer(&stdout))
        }
        Asked::NeverRan(_) | Asked::MayHaveRun(_) => None,
    };
    write_out(answer.unwrap_or_else(|| deny_output(UNREACHED)).as_bytes());
    ExitCode::SUCCESS
}

/// The gate's answer in `stdout`, rebuilt from its decision and its reason alone: `None`
/// unless `stdout` is that answer and nothing else (`is_gate_output`).
fn gate_answer(stdout: &[u8]) -> Option<String> {
    if !is_gate_output(stdout) {
        return None;
    }
    let said: serde_json::Value = serde_json::from_slice(stdout).ok()?;
    let output = &said["hookSpecificOutput"];
    let reason = output["permissionDecisionReason"].as_str()?;
    match output["permissionDecision"].as_str()? {
        "allow" => Some(allow_output(reason)),
        "deny" => Some(deny_output(reason)),
        _ => None,
    }
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
fn ask_in_time(
    event: &str,
    stdin: &[u8],
    run: Option<(String, u64)>,
    key: Option<String>,
    budget: Duration,
) -> Asked {
    if stdin.len().div_ceil(3) * 4 > HOOK_FRAME_MOST {
        return Asked::NeverRan(too_long(stdin.len().div_ceil(3) * 4));
    }
    let params = match params_of(event, stdin, run, key, budget) {
        Ok(params) => params,
        Err(why) => return Asked::NeverRan(why),
    };
    let line = serde_json::to_vec(&params).map_or(usize::MAX, |params| params.len());
    if line.saturating_add(ENVELOPE_ROOM) > HOOK_FRAME_MOST {
        return Asked::NeverRan(too_long(line));
    }
    let stage = Arc::new(AtomicU8::new(READY));
    let (said, answer) = mpsc::channel();
    let asking = Arc::clone(&stage);
    // A thread that can't start has sent nothing: never a panic (an exit 101 a
    // `pre-tool-use` must not have), a hook that never ran.
    let started = thread::Builder::new().spawn(move || {
        let _ = said.send(ask(params, &asking));
    });
    if let Err(e) = started {
        return Asked::NeverRan(format!("rlxd-hook could not start its thread: {e}"));
    }
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

fn too_long(bytes: usize) -> String {
    format!(
        "the hook's request would be {bytes} bytes or more, longer than rlxd reads in one frame"
    )
}

/// `hook`'s params for this hook, or why it cannot be sent as it is: a directory or a
/// variable that is not UTF-8 (JSON carries text, and a lossy one would not be the hook's),
/// a variable longer than rlxd-proto allows, or a key that is not one (whose refusal never
/// says it).
fn params_of(
    event: &str,
    stdin: &[u8],
    run: Option<(String, u64)>,
    key: Option<String>,
    budget: Duration,
) -> Result<Data, String> {
    // By exact name, as Windows looks one up whatever its case; nothing else.
    let mut env = BTreeMap::new();
    for &name in HOOK_ENV_NAMES {
        if let Some(value) = std::env::var_os(name) {
            let value = value
                .into_string()
                .map_err(|_| format!("the hook's {name} is not UTF-8"))?;
            env.insert(name.to_owned(), value);
        }
    }
    let cwd = match std::env::current_dir() {
        Ok(dir) => Some(
            dir.into_os_string()
                .into_string()
                .map_err(|_| "the hook's directory is not UTF-8".to_owned())?,
        ),
        Err(_) => None,
    }
    .filter(|dir| !dir.is_empty());
    let (accept, deadline) = times_within(budget);
    let mut params = HookParams::new(event, b64::encode(stdin), cwd, env)
        .and_then(|params| params.with_times(accept, deadline))
        .map_err(|e| format!("the hook cannot be sent: {e}"))?;
    if let Some((run_id, attempt)) = run {
        params = params
            .with_run(run_id, attempt)
            .map_err(|e| format!("the hook cannot be sent: {e}"))?;
    }
    if let Some(key) = key {
        params = params
            .with_key(key)
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
        // rlxd refused the request itself, so nothing of it reached the brain.
        Some(error) if NEVER_HANDED_ON.contains(&error.code.as_str()) => Asked::NeverRan(format!(
            "rlxd refused it, {}: {}",
            error.code, error.message
        )),
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
    let started = thread::Builder::new().spawn(move || {
        let outcome = connect().and_then(|mut connection| {
            connection
                .request(methods::HOOK_PING, Data::new())
                .map_err(|e: CallError| e.to_string())
        });
        let _ = said.send(outcome);
    });
    if let Err(e) = started {
        say(&format!("could not start its thread: {e}"));
        return ExitCode::FAILURE;
    }
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
        assert_eq!(times_within(budget("session-start")), (5_000, 117_000));
        // Past the store's 5 s wait for a busy database (review #2562).
        assert_eq!(times_within(budget("session-end")), (2_000, 7_000));
        assert_eq!(times_within(budget("stop")), (5_000, 57_000));
        for event in HOOK_EVENTS {
            let (accept, deadline) = times_within(budget(event));
            assert_eq!(
                deadline + 1_000,
                budget(event).as_millis() as u64,
                "{event}"
            );
            assert!(accept <= deadline, "{event}");
            assert!(accept <= 5_000, "{event}");
        }
    }

    fn run(id: &str, attempt: u64, deadline_ms: Option<u64>) -> Option<Option<Run>> {
        Some(Some(Run {
            id: id.to_owned(),
            attempt,
            deadline_ms,
        }))
    }

    #[test]
    fn the_gates_answer_is_rebuilt_from_its_decision_and_reason_alone() {
        let spaced = b" { \"hookSpecificOutput\" : {\"permissionDecisionReason\": \"ok\", \"hookEventName\": \"PreToolUse\", \"permissionDecision\": \"allow\"} }\n\n";
        assert_eq!(gate_answer(spaced), Some(allow_output("ok")));
        assert_eq!(
            gate_answer(deny_output("no").as_bytes()),
            Some(deny_output("no"))
        );
        for other in [&b""[..], b"{}", b"allow"] {
            assert_eq!(gate_answer(other), None);
        }
    }

    #[test]
    fn a_gates_timer_is_the_runs_deadline_and_5_s_below_claude_codes_timeout() {
        assert_eq!(gate_budget(Some(120_000)), Duration::from_secs(125));
        assert_eq!(gate_budget(None), Duration::from_secs(125));
        assert_eq!(gate_budget(Some(3_000)), Duration::from_secs(8));
        // Longer than the old 58 s, so a 120 s decision is waited for.
        assert!(gate_budget(None) > budget(PRE_TOOL_USE));
        for deadline_ms in [1_u64, 999, 1_000, 120_000, 600_000] {
            // rlxd writes Claude Code's timeout as ceil(deadline_ms / 1000) + 10 s.
            let claude_code = Duration::from_secs(deadline_ms.div_ceil(1000) + 10);
            assert!(
                gate_budget(Some(deadline_ms)) < claude_code,
                "{deadline_ms}"
            );
        }
        // No overflow, however long the deadline.
        assert!(gate_budget(Some(u64::MAX)) > Duration::from_millis(u64::MAX - 1));
        let (accept, deadline) = times_within(gate_budget(Some(120_000)));
        assert!(accept <= deadline);
    }

    #[test]
    fn a_run_is_named_whole_or_not_at_all() {
        assert_eq!(run_of(&[]), Some(None));
        assert_eq!(
            run_of(&["--run", "r1", "--attempt", "2"]),
            run("r1", 2, None)
        );
        assert_eq!(
            run_of(&["--attempt", "1", "--run", "r1", "--deadline-ms", "3000"]),
            run("r1", 1, Some(3000))
        );
        assert_eq!(
            run_of(&["--deadline-ms", "5", "--run", "r1", "--attempt", "1"]),
            run("r1", 1, Some(5))
        );
        for bad in [
            &["--deadline-ms", "5"][..],
            &["--run", "r1", "--attempt", "1", "--deadline-ms", "0"],
            &["--run", "r1", "--attempt", "1", "--deadline-ms", "x"],
            &["--run", "r1", "--attempt", "1", "--deadline-ms"],
            &["--run", "r1", "--run", "r2", "--attempt", "1"],
            &["--run", "r1", "--attempt", "1", "--key", "k"],
        ] {
            assert_eq!(run_of(bad), None, "{bad:?}");
        }
        assert_eq!(
            run_of(&["--attempt", "1", "--run", "r1"]),
            run("r1", 1, None)
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
