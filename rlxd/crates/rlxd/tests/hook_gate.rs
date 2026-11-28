//! The tool-call gate through the real `rlxd-hook pre-tool-use` (handoff 0.10.3, 5.1, and
//! 1.5's hook client), on every OS. Each run's settings name the rlxd-hook beside rlxd
//! (`common::rlxd_hook` builds it there, and no `RLXD_TEST_HOOK` is set), which fake-claude
//! runs as Claude Code runs a hook: detached, with the call as JSON on its stdin and the
//! attempt's key in its environment. rlxd-hook sends the key as `hook`'s `hook_key`, and
//! rlxd binds the request to the run's live attempt by that key and by the process that
//! connected, which must be inside the run.
//!
//! The app is a test client in gate dev mode: an `rlxd events --as-app` capture holds the
//! app role while it runs ([`App`]), and `rlxd call --as-app run.decide` answers.

mod common;

use std::path::{Path, PathBuf};
use std::process::Child;
use std::thread;
use std::time::{Duration, Instant};

use common::{TestHome, rlxd_hook};
use serde_json::{Value, json};

/// fake-claude's first call.
const CALL_ID: &str = "toolu_01FakeGateCall000000000001";

const SEND: &str = "mcp__claude_ai_Slack__send_message";

/// fake-claude, linked (or copied) to `<home>/bin/fake-claude/gate`.
fn link(root: &Path) -> PathBuf {
    let dir = root.join("bin").join("fake-claude");
    std::fs::create_dir_all(&dir).unwrap();
    let name = if cfg!(windows) { "gate.exe" } else { "gate" };
    let binary = Path::new(env!("CARGO_BIN_EXE_fake-claude"));
    if std::fs::hard_link(binary, dir.join(name)).is_err() {
        std::fs::copy(binary, dir.join(name)).unwrap();
    }
    dir.join("gate")
}

/// A home whose rlxd runs fake-claude's gate scenarios with the real rlxd-hook as their
/// hook, in gate dev mode, the test client paired as the app (and not connected), `envs`
/// set too.
fn home_with(envs: &[(&str, &str)]) -> TestHome {
    // Built beside the test rlxd, where each run's settings name it.
    let hook = rlxd_hook();
    assert!(hook.is_file(), "{}", hook.display());
    let home = TestHome::new();
    let claude = link(home.root());
    let config = home.root().join("claude-config");
    let user = home.root().join("user-home");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&user).unwrap();
    let mut home = home
        .with_claude(claude)
        .with_env("CLAUDE_CONFIG_DIR", config.to_str().unwrap())
        .with_env("HOME", user.to_str().unwrap())
        .with_env("USERPROFILE", user.to_str().unwrap());
    for (name, value) in envs {
        home = home.with_env(name, value);
    }
    home.start_dev();
    let paired = home.rlxd(&["call", "--as-app", "status"]);
    assert_eq!(paired.code(), Some(0), "{paired:?}");
    common::eventually("the pairing call is gone", || home.app_clients() == 0);
    home
}

/// An app gate client, connected until dropped.
struct App {
    capture: Child,
}

impl App {
    fn open(home: &TestHome) -> Self {
        let capture = home.spawn(&[
            "events",
            "--as-app",
            "--topics",
            "daemon",
            "--follow",
            "--timeout",
            "300",
        ]);
        common::eventually("the app is connected", || home.app_clients() > 0);
        Self { capture }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = self.capture.kill();
        let _ = self.capture.wait();
    }
}

/// A file outside the home where fake-claude keeps each start's hook key
/// (`FAKE_CLAUDE_KEY_OUT`), removed when dropped.
struct KeyFile(PathBuf);

impl KeyFile {
    fn new() -> Self {
        let name = format!("rlxd-hook-gate-keys-{}-{}", std::process::id(), run_id());
        Self(std::env::temp_dir().join(name))
    }

    fn keys(&self) -> Vec<String> {
        std::fs::read_to_string(&self.0)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

impl Drop for KeyFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn run_id() -> String {
    common::runs::run_id()
}

fn seq(home: &TestHome) -> u64 {
    home.status()["seq"].as_u64().unwrap()
}

/// A task run of the gate scenario `scenario`, mounting Slack.
fn task(scenario: &str) -> Value {
    json!({"kind": "task", "prompt": scenario, "space": "default",
        "mount": {"kind": "connectors", "connectors": ["claude.ai Slack"]}})
}

/// `rlxd call run.start params`, which must succeed: the run's id.
fn start(home: &TestHome, params: &Value) -> String {
    let id = run_id();
    let mut params = params.clone();
    params["run_id"] = json!(id);
    let run = home.rlxd(&["call", "run.start", &params.to_string()]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    id
}

/// The app's `run.decide`.
fn decide(home: &TestHome, r: &str, call_id: &str, decision: &str, reason: &str) -> common::Run {
    let params = json!({"run_id": r, "call_id": call_id, "decision": decision, "reason": reason});
    home.rlxd(&["call", "--as-app", "run.decide", &params.to_string()])
}

/// The run events rlxd holds after `since`.
fn replay(home: &TestHome, since: u64) -> Vec<Value> {
    let run = home.rlxd(&["events", "--topics", "run", "--since", &since.to_string()]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    run.lines()
}

/// Waits up to a minute until the replay after `since` satisfies `done`.
fn wait_until(
    home: &TestHome,
    since: u64,
    what: &str,
    done: impl Fn(&[Value]) -> bool,
) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let events = replay(home, since);
        if done(&events) {
            return events;
        }
        assert!(Instant::now() < deadline, "never: {what}: {events:#?}");
        thread::sleep(Duration::from_millis(100));
    }
}

/// Run `r`'s events of `type`.
fn of<'a>(events: &'a [Value], r: &str, r#type: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|e| e["data"]["run_id"] == r && e["type"] == r#type)
        .collect()
}

fn ended(events: &[Value], r: &str) -> bool {
    ["run.done", "run.failed", "run.needs_you"]
        .iter()
        .any(|t| !of(events, r, t).is_empty())
}

/// Waits until run `r` has `n` `run.tool_call`s.
fn calls_out(home: &TestHome, since: u64, r: &str, n: usize) -> Vec<Value> {
    wait_until(home, since, &format!("{n} run.tool_call of {r}"), |e| {
        of(e, r, "run.tool_call").len() >= n
    })
}

/// Waits until run `r` ends.
fn end_of(home: &TestHome, since: u64, r: &str) -> Vec<Value> {
    wait_until(home, since, &format!("the end of {r}"), |e| ended(e, r))
}

/// The fixture log's starts of run `r`: those whose command line names it.
fn starts_of(home: &TestHome, r: &str) -> Vec<Value> {
    let text =
        std::fs::read_to_string(home.root().join("fake-claude.log.json")).unwrap_or_default();
    let log: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({"starts": []}));
    log["starts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| {
            s["argv"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a.as_str().unwrap_or("").contains(r))
        })
        .cloned()
        .collect()
}

/// The hook's answers to run `r`'s starts, in order.
fn replies(home: &TestHome, r: &str) -> Vec<Value> {
    starts_of(home, r)
        .iter()
        .flat_map(|s| s["hook_replies"].as_array().unwrap().clone())
        .collect()
}

fn tool_runs(home: &TestHome, r: &str) -> u64 {
    starts_of(home, r)
        .iter()
        .map(|s| s["tool_runs"].as_u64().unwrap())
        .sum()
}

/// Run `r`'s ledger, as `run.get` gives it.
fn ledger(home: &TestHome, r: &str) -> Vec<Value> {
    let run = home.rlxd(&["call", "run.get", &json!({"run_id": r}).to_string()]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    run.data()["calls"].as_array().cloned().unwrap_or_default()
}

/// Run `r`'s first attempt's hook command, as its settings name it.
fn hook_command(home: &TestHome, r: &str) -> String {
    let path = home
        .root()
        .join("rlxd")
        .join("runs")
        .join(r)
        .join("1")
        .join("settings.json");
    let settings: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// `rlxd-hook pre-tool-use` for run `r`'s attempt 1, run from the test, outside every run,
/// with `key` and `stdin`: its stdout's decision and reason, and its stderr.
fn outsider(home: &TestHome, r: &str, key: &str, stdin: &Value) -> (String, String, String) {
    outsider_within(home, r, key, stdin, 120_000)
}

/// [`outsider`], its command line saying `deadline_ms`.
fn outsider_within(
    home: &TestHome,
    r: &str,
    key: &str,
    stdin: &Value,
    deadline_ms: u64,
) -> (String, String, String) {
    let deadline_ms = deadline_ms.to_string();
    let (code, stdout, stderr) = home.run_bytes(
        &rlxd_hook(),
        &[
            "pre-tool-use",
            "--run",
            r,
            "--attempt",
            "1",
            "--deadline-ms",
            &deadline_ms,
        ],
        home.root(),
        stdin.to_string().as_bytes(),
        &[("RLXD_HOOK_KEY", key)],
    );
    assert_eq!(code, Some(0), "{stderr}");
    let reply: Value = serde_json::from_slice(&stdout)
        .unwrap_or_else(|e| panic!("not one reply ({e}): {}", String::from_utf8_lossy(&stdout)));
    let said = &reply["hookSpecificOutput"];
    (
        said["permissionDecision"].as_str().unwrap().to_owned(),
        said["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .to_owned(),
        stderr,
    )
}

#[test]
fn through_rlxd_hook_the_apps_allow_runs_the_tool_and_its_deny_is_claude_codes() {
    let home = home_with(&[]);
    let _app = App::open(&home);
    let since = seq(&home);
    let a = start(&home, &task("slack-send"));
    calls_out(&home, since, &a, 1);
    // The run's hook is the real rlxd-hook, with the run's deadline for its own timer.
    let command = hook_command(&home, &a);
    assert!(command.contains("rlxd-hook"), "{command}");
    assert!(!command.contains("__test-hook"), "{command}");
    assert!(
        command.ends_with(&format!(
            "pre-tool-use --run {a} --attempt 1 --deadline-ms 120000"
        )),
        "{command}"
    );
    assert_eq!(
        decide(&home, &a, CALL_ID, "allow", "go ahead").code(),
        Some(0)
    );
    end_of(&home, since, &a);
    let answers = replies(&home, &a);
    assert_eq!(answers[0]["decision"], "allow", "{answers:#?}");
    assert_eq!(answers[0]["reason"], "go ahead");
    assert_eq!(tool_runs(&home, &a), 1);
    let calls = ledger(&home, &a);
    assert_eq!(calls.len(), 1, "{calls:#?}");
    assert_eq!(calls[0]["state"], "ok");

    let b = start(&home, &task("slack-send"));
    calls_out(&home, since, &b, 1);
    assert_eq!(
        decide(&home, &b, CALL_ID, "deny", "not this channel").code(),
        Some(0)
    );
    end_of(&home, since, &b);
    let answers = replies(&home, &b);
    assert_eq!(answers[0]["decision"], "deny", "{answers:#?}");
    assert_eq!(answers[0]["reason"], "not this channel");
    assert_eq!(tool_runs(&home, &b), 0);
    // The gate's run events are its own: its hook requests, allowed or denied, put nothing
    // on the hook topic.
    assert_eq!(hook_events(&home), Vec::<Value>::new());
}

/// Every event on the `hook` topic.
fn hook_events(home: &TestHome) -> Vec<Value> {
    let run = home.rlxd(&["events", "--topics", "hook", "--since", "0"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    run.lines()
}

#[test]
fn through_rlxd_hook_a_request_not_bound_to_the_run_is_refused_and_no_key_leaks() {
    let kept = KeyFile::new();
    let home = home_with(&[("FAKE_CLAUDE_KEY_OUT", kept.0.to_str().unwrap())]);
    let _app = App::open(&home);
    let since = seq(&home);
    // Run B waits on its call; run A, in the same space, knows B's id.
    let b = start(&home, &task("slack-send"));
    calls_out(&home, since, &b, 1);
    let space = home.rlxd(&["call", "space.ensure", r#"{"name":"default"}"#]);
    let space = PathBuf::from(space.data()["path"].as_str().unwrap());
    std::fs::write(space.join("other-run"), &b).unwrap();
    let a = start(&home, &task("bad-hooks"));
    let events = calls_out(&home, since, &a, 1);
    let answers = replies(&home, &a);
    let labels: Vec<&str> = answers
        .iter()
        .map(|a| a["label"].as_str().unwrap())
        .collect();
    assert_eq!(
        labels,
        ["wrong-key", "no-key", "wrong-attempt", "other-run"],
        "{answers:#?}"
    );
    // Each denied for its own binding. With no key, rlxd-hook asks nothing at all.
    for (answer, why) in answers.iter().zip([
        "its key is not the attempt's",
        "no RLXD_HOOK_KEY",
        "attempt 2",
        "its key is not the attempt's",
    ]) {
        assert_eq!(answer["decision"], "deny", "{answer}");
        let reason = answer["reason"].as_str().unwrap();
        assert!(reason.contains(why), "{why}: {answer}");
    }
    // None reached the app: A's real call and B's are the only calls.
    let tool_calls: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "run.tool_call")
        .collect();
    assert_eq!(tool_calls.len(), 2, "{events:#?}");
    // A process outside every run reaches nothing: with a key it guessed, or with B's own
    // key (B started first, so its key is the first kept).
    let keys = kept.keys();
    assert_eq!(keys.len(), 2, "{keys:?}");
    let stdin = json!({"hook_event_name": "PreToolUse", "tool_name": SEND,
        "tool_input": {}, "tool_use_id": "toolu_outsider"});
    for (key, why) in [
        ("1".repeat(64), "its key is not the attempt's"),
        (keys[0].clone(), "not inside the run"),
    ] {
        let (decision, reason, stderr) = outsider(&home, &b, &key, &stdin);
        assert_eq!(decision, "deny", "{reason}");
        assert!(reason.contains(why), "{why}: {reason}");
        assert!(!reason.contains(&key) && !stderr.contains(&key));
    }
    assert_eq!(
        decide(&home, &a, CALL_ID, "allow", "the real one").code(),
        Some(0)
    );
    assert_eq!(decide(&home, &b, CALL_ID, "deny", "b").code(), Some(0));
    end_of(&home, since, &a);
    end_of(&home, since, &b);
    assert_eq!(replies(&home, &a).last().unwrap()["decision"], "allow");
    assert_eq!(ledger(&home, &a).len(), 1);
    assert_eq!(ledger(&home, &b).len(), 1);
    let events = replay(&home, since);
    assert!(
        events
            .iter()
            .all(|e| !e.to_string().contains("toolu_outsider")),
        "{events:#?}"
    );
    // Nor do requests the gate refused, from inside a run or outside every run.
    assert_eq!(hook_events(&home), Vec::<Value>::new());
    // No key is anywhere in the home: rlxd's log, the run records and logs, the settings,
    // the hook events, the fixture log.
    let all = home.rlxd(&["events", "--since", "0"]);
    for key in &keys {
        assert!(!all.stdout.contains(key.as_str()), "a key in the events");
    }
    let mut stack = vec![home.root().to_path_buf()];
    let mut files = 0;
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() {
                files += 1;
                let bytes = std::fs::read(&path).unwrap_or_default();
                let text = String::from_utf8_lossy(&bytes);
                for key in &keys {
                    assert!(!text.contains(key.as_str()), "a key in {}", path.display());
                }
            }
        }
    }
    assert!(files > 5, "{files}");
}

/// Why rlxd-hook denies a call it could not ask rlxd about.
const UNREACHED: &str = "rlxd could not be asked about this tool call, so it is denied";

#[test]
fn an_rlxd_that_never_answers_is_a_deny_from_rlxd_hooks_own_timer() {
    // rlxd holds each pre-tool-use 30 s; rlxd-hook's timer is deadline_ms (1 s) and 5 s.
    let home = home_with(&[("RLXD_TEST_HOOK_HOLD_MS", "30000")]);
    let stdin = json!({"hook_event_name": "PreToolUse", "tool_name": SEND,
        "tool_input": {}, "tool_use_id": "toolu_held"});
    let started = Instant::now();
    let (decision, reason, stderr) =
        outsider_within(&home, &run_id(), &"2".repeat(64), &stdin, 1_000);
    let took = started.elapsed();
    assert_eq!(decision, "deny", "{reason}");
    assert_eq!(reason, UNREACHED);
    assert!(stderr.is_empty(), "{stderr}");
    assert!(
        took >= Duration::from_millis(5_500) && took < Duration::from_secs(15),
        "{took:?}"
    );
    assert!(
        home.log().contains("holding a pre-tool-use"),
        "{}",
        home.log()
    );
}

#[test]
fn a_connection_rlxd_drops_mid_request_is_a_deny() {
    // rlxd holds the request, then is killed with it: rlxd-hook's connection drops long
    // before its own timer (20 s and 5 s).
    let home = home_with(&[("RLXD_TEST_HOOK_HOLD_MS", "60000")]);
    let pid = home.status()["pid"].as_u64().unwrap();
    let stdin = json!({"hook_event_name": "PreToolUse", "tool_name": SEND,
        "tool_input": {}, "tool_use_id": "toolu_dropped"});
    let mut command = home.command_of(
        &rlxd_hook(),
        &[
            "pre-tool-use",
            "--run",
            &run_id(),
            "--attempt",
            "1",
            "--deadline-ms",
            "20000",
        ],
    );
    command
        .env("RLXD_HOOK_KEY", "3".repeat(64))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let started = Instant::now();
    let mut child = common::launch(&mut command).expect("rlxd-hook starts");
    {
        use std::io::Write;
        let mut input = child.stdin.take().unwrap();
        input.write_all(stdin.to_string().as_bytes()).unwrap();
    }
    common::eventually("rlxd holds the request", || {
        home.log().contains("holding a pre-tool-use")
    });
    common::kill_hard(pid);
    let run = common::finish(child);
    let took = started.elapsed();
    assert_eq!(run.code(), Some(0), "{run:?}");
    let reply: Value = serde_json::from_str(run.stdout.trim()).unwrap();
    let said = &reply["hookSpecificOutput"];
    assert_eq!(said["permissionDecision"], "deny", "{run:?}");
    assert_eq!(said["permissionDecisionReason"], UNREACHED, "{run:?}");
    assert!(took < Duration::from_secs(15), "{took:?}");
}
