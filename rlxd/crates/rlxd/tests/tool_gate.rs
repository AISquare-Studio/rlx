//! The tool-call gate's Accept checks (handoff 0.10.3, 5.1), through the real socket, with
//! fake-claude standing in for Claude Code on every OS. The `[real]` checks, with a real
//! Claude Code and connector, are the owner's to run.
//!
//! Each home runs fake-claude's `gate` scenario, which plays the gate scenario its prompt
//! names (`slack-send`, `send-twice`, …), and sets `RLXD_TEST_HOOK`, so each run's settings
//! name rlxd itself as the hook (`rlxd __test-hook pre-tool-use`, which asks
//! `debug.pre_tool_use`) until rlxd-hook's own pre-tool-use is wired. fake-claude runs that
//! command as Claude Code runs a hook, from inside the run, and the fixture log keeps each
//! answer (`hook_replies`) and each tool it ran (`tool_runs`).
//!
//! The app is a test client in gate dev mode: an `rlxd events --as-app` capture holds the
//! app role while it runs ([`App`]), and `rlxd call --as-app run.decide` answers.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use common::{TestHome, assert_valid, finish};
use serde_json::{Value, json};

/// fake-claude's first call, and its second (`send-twice`), and `slack-send`'s resumed.
const CALL_ID: &str = "toolu_01FakeGateCall000000000001";
const SECOND_CALL_ID: &str = "toolu_01FakeGateCall000000000002";
const RESUMED_CALL_ID: &str = "toolu_01FakeGateCall0000000000r1";

const SEND: &str = "mcp__claude_ai_Slack__send_message";
const READ: &str = "mcp__claude_ai_Slack__slack_search_public";

/// `gate/input.json`'s RFC 8785 SHA-256: the file spells it in another order, spaced.
const INPUT_SHA256: &str = "e372970f8358d0e43056b3fd4e0724415e3ecb9002602e04792d66307c36f89f";

/// `{"query":"standup"}`'s, `slack-read`'s input.
const READ_SHA256: &str = "967a5a33053be05e098a0b6bb1591e1420307d3c076a560713e6933d1c51eb2f";

/// Another send's: the same channel, another message.
const OTHER_SHA256: &str = "0ddf929659dddfd2dcff23112d2a6a51710824462bd0be4e8500fa6a75229f6a";

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

/// A home whose rlxd runs fake-claude's gate scenarios with rlxd as their hook, in gate
/// dev mode, the test client paired as the app (and not connected), `envs` set too.
fn home_with(envs: &[(&str, &str)]) -> TestHome {
    let home = TestHome::new();
    let claude = link(home.root());
    let config = home.root().join("claude-config");
    let user = home.root().join("user-home");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&user).unwrap();
    let mut home = home
        .with_claude(claude)
        .with_env("RLXD_TEST_HOOK", "1")
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

fn home() -> TestHome {
    home_with(&[])
}

/// An app connection, held while this lives: an `rlxd events --as-app` capture.
struct App {
    capture: Child,
}

impl App {
    fn open(home: &TestHome) -> Self {
        let before = home.app_clients();
        let capture = home.spawn(&[
            "events",
            "--as-app",
            "--topics",
            "daemon",
            "--follow",
            "--timeout",
            "300",
        ]);
        common::eventually("the app is connected", || home.app_clients() > before);
        Self { capture }
    }

    /// Ends the app's connection, and waits until rlxd counts it gone.
    fn close(mut self, home: &TestHome) {
        let _ = self.capture.kill();
        let _ = self.capture.wait();
        common::eventually("the app is gone", || home.app_clients() == 0);
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = self.capture.kill();
        let _ = self.capture.wait();
    }
}

/// A file outside the home where fake-claude keeps each start's hook key
/// (`FAKE_CLAUDE_KEY_OUT`), removed when dropped, a failed test's too.
struct KeyFile(PathBuf);

impl KeyFile {
    fn new() -> Self {
        let name = format!("rlxd-keys-{}-{}", std::process::id(), run_id());
        Self(std::env::temp_dir().join(name))
    }
}

impl Drop for KeyFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A new run id.
fn run_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let mixed = nanos ^ (u128::from(std::process::id()) << 80) ^ (u128::from(n) << 64);
    let hex = format!("{mixed:032x}");
    format!(
        "{}-{}-4{}-8{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    )
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

/// `rlxd call method params` as the app.
fn as_app(home: &TestHome, method: &str, params: &Value) -> common::Run {
    home.rlxd(&["call", "--as-app", method, &params.to_string()])
}

/// The app's `run.decide`.
fn decide(home: &TestHome, r: &str, call_id: &str, decision: &str, reason: &str) -> common::Run {
    let params = json!({"run_id": r, "call_id": call_id, "decision": decision, "reason": reason});
    as_app(home, "run.decide", &params)
}

/// `rlxd call method params` as the cli, which must succeed: its data.
fn call(home: &TestHome, method: &str, params: &Value) -> Value {
    let run = home.rlxd(&["call", method, &params.to_string()]);
    assert_eq!(run.code(), Some(0), "{method}: {run:?}");
    run.data()
}

/// The run and needs_you events rlxd holds after `since`.
fn replay(home: &TestHome, since: u64) -> Vec<Value> {
    let run = home.rlxd(&[
        "events",
        "--topics",
        "run,needs_you",
        "--since",
        &since.to_string(),
    ]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    run.lines()
}

/// Waits up to `seconds` until the replay after `since` satisfies `done`.
fn wait_until(
    home: &TestHome,
    since: u64,
    seconds: u64,
    what: &str,
    done: impl Fn(&[Value]) -> bool,
) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        let events = replay(home, since);
        if done(&events) {
            return events;
        }
        assert!(Instant::now() < deadline, "never: {what}: {events:#?}");
        thread::sleep(Duration::from_millis(100));
    }
}

/// `id`'s events, of `type` when given.
fn of<'a>(events: &'a [Value], id: &str, r#type: Option<&str>) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|e| e["data"]["run_id"] == id)
        .filter(|e| r#type.is_none_or(|t| e["type"] == t))
        .collect()
}

fn ended(events: &[Value], id: &str) -> bool {
    of(events, id, None).iter().any(|e| {
        ["run.done", "run.failed", "run.needs_you"].contains(&e["type"].as_str().unwrap_or(""))
    })
}

/// Waits until run `r` has `n` `run.tool_call`s: the events so far.
fn calls_out(home: &TestHome, since: u64, r: &str, n: usize) -> Vec<Value> {
    wait_until(home, since, 60, &format!("{n} run.tool_call of {r}"), |e| {
        of(e, r, Some("run.tool_call")).len() >= n
    })
}

/// Waits until run `r` ends: the events so far.
fn end_of(home: &TestHome, since: u64, r: &str) -> Vec<Value> {
    wait_until(home, since, 60, &format!("the end of {r}"), |e| ended(e, r))
}

/// Run `r`'s end event.
fn end_event<'a>(events: &'a [Value], r: &str) -> &'a Value {
    of(events, r, None)
        .into_iter()
        .rev()
        .find(|e| {
            ["run.done", "run.failed", "run.needs_you"].contains(&e["type"].as_str().unwrap_or(""))
        })
        .unwrap_or_else(|| panic!("no end of {r}: {events:#?}"))
}

/// Run `r`'s `run.tool_decided` events.
fn decided<'a>(events: &'a [Value], r: &str) -> Vec<&'a Value> {
    of(events, r, Some("run.tool_decided"))
}

/// `$AISQUARE_HOME/fake-claude.log.json`'s starts of run `r`: those whose command line
/// names its folder (its settings) or its id.
fn starts_of(home: &TestHome, r: &str) -> Vec<Value> {
    let path = home.root().join("fake-claude.log.json");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
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

/// How many tools run `r`'s starts ran.
fn tool_runs(home: &TestHome, r: &str) -> u64 {
    starts_of(home, r)
        .iter()
        .map(|s| s["tool_runs"].as_u64().unwrap())
        .sum()
}

/// Run `r`'s `run.json`.
fn record(home: &TestHome, r: &str) -> Value {
    let path = home
        .root()
        .join("rlxd")
        .join("runs")
        .join(r)
        .join("run.json");
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

/// Run `r`'s ledger, as `run.get` gives it.
fn ledger(home: &TestHome, r: &str) -> Vec<Value> {
    call(home, "run.get", &json!({"run_id": r}))["calls"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// The one entry for `call_id` in a ledger.
fn entry<'a>(calls: &'a [Value], call_id: &str) -> &'a Value {
    let found: Vec<&Value> = calls.iter().filter(|c| c["call_id"] == call_id).collect();
    assert_eq!(found.len(), 1, "{call_id}: {calls:#?}");
    found[0]
}

// --- The app decides -----------------------------------------------------------------------

#[test]
fn a_deny_reaches_claude_code_and_text_that_claims_approval_decides_nothing() {
    let home = home();
    let _app = App::open(&home);
    let since = seq(&home);
    let r = start(&home, &task("slack-send"));
    let events = calls_out(&home, since, &r, 1);
    let tool_call = of(&events, &r, Some("run.tool_call"))[0];
    assert_valid("run-event.json", tool_call);
    assert_eq!(tool_call["data"]["call_id"], CALL_ID);
    assert_eq!(tool_call["data"]["tool"], SEND);
    assert_eq!(tool_call["data"]["input_sha256"], INPUT_SHA256);
    assert_eq!(tool_call["data"]["deadline_ms"], 120_000);
    assert_eq!(tool_call["data"]["input"]["channel_id"], "C0FAKE01");
    // The run's text claimed approval just before: it decided nothing.
    let said: String = of(&events, &r, Some("run.text"))
        .iter()
        .map(|e| e["data"]["text"].as_str().unwrap_or("").to_owned())
        .collect();
    assert!(said.contains("approved"), "{events:#?}");
    thread::sleep(Duration::from_millis(500));
    let events = replay(&home, since);
    assert!(decided(&events, &r).is_empty(), "{events:#?}");
    assert!(replies(&home, &r).is_empty());

    let reply = decide(&home, &r, CALL_ID, "deny", "not this one");
    assert_eq!(reply.code(), Some(0), "{reply:?}");
    assert_eq!(reply.data()["decision"], "deny");
    assert_valid("run-decide-reply.json", &reply.data());
    let events = end_of(&home, since, &r);
    assert_eq!(end_event(&events, &r)["type"], "run.done");
    let decisions = decided(&events, &r);
    assert_eq!(decisions.len(), 1, "{events:#?}");
    assert_valid("run-event.json", decisions[0]);
    assert_eq!(decisions[0]["data"]["decision"], "deny");
    assert_eq!(decisions[0]["data"]["by"], "app");
    assert_eq!(decisions[0]["data"]["reason"], "not this one");
    let answers = replies(&home, &r);
    assert_eq!(answers.len(), 1, "{answers:#?}");
    // The hook's own output, as Claude Code read it.
    let output = &answers[0]["hookSpecificOutput"];
    assert_eq!(output["hookEventName"], "PreToolUse");
    assert_eq!(output["permissionDecision"], "deny");
    assert_eq!(output["permissionDecisionReason"], "not this one");
    assert_eq!(tool_runs(&home, &r), 0);
    let calls = ledger(&home, &r);
    let call = entry(&calls, CALL_ID);
    assert_eq!(call["decision"], "deny");
    assert_eq!(call["by"], "app");
    assert_eq!(call["state"], "decided");
    assert_eq!(call["input_sha256"], INPUT_SHA256);
    assert_eq!(
        record(&home, &r)["calls"],
        json!(calls),
        "run.json holds it"
    );

    // A draft is a deny to Claude Code, and the ledger says draft.
    let since = seq(&home);
    let d = start(&home, &task("slack-send"));
    calls_out(&home, since, &d, 1);
    let params = json!({"run_id": d, "call_id": CALL_ID, "decision": "draft",
        "reason": "drafted for you", "draft_id": "draft-7"});
    let reply = as_app(&home, "run.decide", &params);
    assert_eq!(reply.code(), Some(0), "{reply:?}");
    let events = end_of(&home, since, &d);
    let decisions = decided(&events, &d);
    assert_eq!(decisions[0]["data"]["decision"], "draft");
    assert_eq!(decisions[0]["data"]["draft_id"], "draft-7");
    assert_eq!(replies(&home, &d)[0]["decision"], "deny");
    assert_eq!(tool_runs(&home, &d), 0);
    assert_eq!(entry(&ledger(&home, &d), CALL_ID)["decision"], "draft");
}

#[test]
fn an_allow_runs_the_tool_and_the_ledger_says_ok_or_error() {
    let home = home();
    let _app = App::open(&home);
    let since = seq(&home);
    let r = start(&home, &task("slack-send"));
    calls_out(&home, since, &r, 1);
    let reply = decide(&home, &r, CALL_ID, "allow", "looks right");
    assert_eq!(reply.code(), Some(0), "{reply:?}");
    let events = end_of(&home, since, &r);
    assert_eq!(end_event(&events, &r)["type"], "run.done");
    assert_eq!(replies(&home, &r)[0]["decision"], "allow");
    assert_eq!(tool_runs(&home, &r), 1);
    // Decided (and written) before the hook had its answer, so before the tool's result.
    let decided_at = events
        .iter()
        .position(|e| e["data"]["run_id"] == r.as_str() && e["type"] == "run.tool_decided")
        .unwrap();
    let result_at = events
        .iter()
        .position(|e| e["data"]["run_id"] == r.as_str() && e["type"] == "run.tool_result")
        .unwrap();
    assert!(decided_at < result_at, "{events:#?}");
    let calls = ledger(&home, &r);
    assert_eq!(entry(&calls, CALL_ID)["decision"], "allow");
    assert_eq!(entry(&calls, CALL_ID)["state"], "ok");
    assert_eq!(record(&home, &r)["calls"], json!(calls));

    let since = seq(&home);
    let f = start(&home, &task("send-fails"));
    calls_out(&home, since, &f, 1);
    assert_eq!(decide(&home, &f, CALL_ID, "allow", "ok").code(), Some(0));
    end_of(&home, since, &f);
    assert_eq!(tool_runs(&home, &f), 1);
    assert_eq!(entry(&ledger(&home, &f), CALL_ID)["state"], "error");
}

#[test]
fn the_first_answer_wins_and_later_ones_are_refused() {
    let home = home();
    let _app = App::open(&home);
    let since = seq(&home);
    let r = start(&home, &task("slack-send"));
    calls_out(&home, since, &r, 1);
    // Two answers at once: one wins.
    let answers: Vec<Child> = ["allow", "deny"]
        .iter()
        .map(|decision| {
            let params = json!({"run_id": r, "call_id": CALL_ID, "decision": decision,
                "reason": format!("{decision} it")});
            let mut command =
                home.command(&["call", "--as-app", "run.decide", &params.to_string()]);
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            common::launch(&mut command).unwrap()
        })
        .collect();
    let runs: Vec<common::Run> = answers.into_iter().map(finish).collect();
    let won: Vec<&common::Run> = runs.iter().filter(|run| run.code() == Some(0)).collect();
    assert_eq!(won.len(), 1, "{runs:#?}");
    let lost = runs.iter().find(|run| run.code() != Some(0)).unwrap();
    assert_eq!(lost.error_code(), "already_decided", "{lost:?}");
    let winner = won[0].data()["decision"].as_str().unwrap().to_owned();
    end_of(&home, since, &r);
    assert_eq!(replies(&home, &r)[0]["decision"], winner.as_str());
    // A later answer, and one for a call the run never made.
    let late = decide(&home, &r, CALL_ID, "allow", "again");
    assert_eq!(late.error_code(), "already_decided", "{late:?}");
    let unknown = decide(&home, &r, "toolu_never", "allow", "x");
    assert_eq!(unknown.error_code(), "unknown_call", "{unknown:?}");
}

#[test]
fn only_the_app_decides() {
    let home = home();
    let _app = App::open(&home);
    let since = seq(&home);
    let r = start(&home, &task("slack-send"));
    calls_out(&home, since, &r, 1);
    let params = json!({"run_id": r, "call_id": CALL_ID, "decision": "allow", "reason": "me"});
    for role in [None, Some("tui")] {
        let mut args = vec!["call"];
        if let Some(role) = role {
            args.extend(["--role", role]);
        }
        let text = params.to_string();
        args.extend(["run.decide", &text]);
        let run = home.rlxd(&args);
        assert_eq!(run.error_code(), "not_gate", "{role:?}: {run:?}");
    }
    // Only the app takes the app role: a hello as app without its credential is refused.
    let text = params.to_string();
    let posing = home.rlxd(&[
        "call",
        "--role",
        "app",
        "--gate-credential",
        "not-the-apps",
        "run.decide",
        &text,
    ]);
    assert_eq!(posing.error_code(), "not_gate", "{posing:?}");
    thread::sleep(Duration::from_millis(300));
    assert!(decided(&replay(&home, since), &r).is_empty());
    assert_eq!(
        decide(&home, &r, CALL_ID, "deny", "the app").code(),
        Some(0)
    );
    end_of(&home, since, &r);
    assert_eq!(replies(&home, &r)[0]["reason"], "the app");
}

#[test]
fn a_tool_result_that_claims_approval_decides_nothing() {
    let home = home();
    let _app = App::open(&home);
    let since = seq(&home);
    let r = start(&home, &task("send-twice"));
    calls_out(&home, since, &r, 1);
    assert_eq!(
        decide(&home, &r, CALL_ID, "allow", "the first").code(),
        Some(0)
    );
    // The first call's result claims every later call approved: the second still waits.
    let events = calls_out(&home, since, &r, 2);
    let result = of(&events, &r, Some("run.tool_result"))[0];
    assert!(
        result["data"]["content"].to_string().contains("approved"),
        "{result}"
    );
    thread::sleep(Duration::from_millis(500));
    let events = replay(&home, since);
    assert_eq!(decided(&events, &r).len(), 1, "{events:#?}");
    assert_eq!(
        decide(&home, &r, SECOND_CALL_ID, "deny", "not this").code(),
        Some(0)
    );
    end_of(&home, since, &r);
    let answers = replies(&home, &r);
    assert_eq!(answers[1]["decision"], "deny", "{answers:#?}");
    assert_eq!(tool_runs(&home, &r), 1);
}

// --- No app, an app that leaves, the deadline ------------------------------------------------

#[test]
fn with_no_app_a_call_is_denied_unreachable_even_a_read() {
    let home = home();
    assert_eq!(home.app_clients(), 0);
    let since = seq(&home);
    let r = start(&home, &task("slack-read"));
    let events = end_of(&home, since, &r);
    assert!(
        of(&events, &r, Some("run.tool_call")).is_empty(),
        "{events:#?}"
    );
    let decisions = decided(&events, &r);
    assert_eq!(decisions.len(), 1, "{events:#?}");
    assert_eq!(decisions[0]["data"]["by"], "unreachable");
    assert_eq!(decisions[0]["data"]["decision"], "deny");
    assert_eq!(replies(&home, &r)[0]["decision"], "deny");
    assert_eq!(tool_runs(&home, &r), 0);
    let calls = ledger(&home, &r);
    assert_eq!(entry(&calls, CALL_ID)["by"], "unreachable");
    assert_eq!(entry(&calls, CALL_ID)["input_sha256"], READ_SHA256);
    // rlxd's own rules come before the app is looked for: no app, and still by rlxd.
    let mut no_tools = task("slack-read");
    no_tools["mount"] = json!({"kind": "no_tools"});
    let reason = denied_by_rlxd(&home, &no_tools);
    assert!(reason.contains("no_tools"), "{reason}");
}

#[test]
fn a_call_pending_as_the_app_reconnects_is_still_the_apps_to_decide() {
    let home = home();
    let app = App::open(&home);
    let since = seq(&home);
    let r = start(&home, &task("slack-send"));
    calls_out(&home, since, &r, 1);
    app.close(&home);
    thread::sleep(Duration::from_millis(300));
    assert!(
        decided(&replay(&home, since), &r).is_empty(),
        "held, not denied"
    );
    let _app = App::open(&home);
    let reply = decide(&home, &r, CALL_ID, "allow", "back again");
    assert_eq!(reply.code(), Some(0), "{reply:?}");
    end_of(&home, since, &r);
    assert_eq!(replies(&home, &r)[0]["decision"], "allow");
    assert_eq!(tool_runs(&home, &r), 1);
}

#[test]
fn an_app_that_never_comes_back_leaves_the_call_to_time_out_and_too_late() {
    let home = home();
    let app = App::open(&home);
    let since = seq(&home);
    let mut params = task("slack-send");
    params["deadline_ms"] = json!(1500);
    let r = start(&home, &params);
    let events = calls_out(&home, since, &r, 1);
    assert_eq!(
        of(&events, &r, Some("run.tool_call"))[0]["data"]["deadline_ms"],
        1500
    );
    app.close(&home);
    let events = end_of(&home, since, &r);
    let decisions = decided(&events, &r);
    assert_eq!(decisions[0]["data"]["by"], "timeout", "{events:#?}");
    assert_eq!(replies(&home, &r)[0]["decision"], "deny");
    assert_eq!(tool_runs(&home, &r), 0);
    let late = decide(&home, &r, CALL_ID, "allow", "sorry, late");
    assert_eq!(late.error_code(), "too_late", "{late:?}");
    assert_eq!(entry(&ledger(&home, &r), CALL_ID)["by"], "timeout");
    // Claude Code never ends the hook before rlxd's deadline: deadline + 10 s.
    let settings = home
        .root()
        .join("rlxd")
        .join("runs")
        .join(&r)
        .join("1")
        .join("settings.json");
    let settings: Value =
        serde_json::from_str(&std::fs::read_to_string(settings).unwrap()).unwrap();
    assert_eq!(
        settings["hooks"]["PreToolUse"][0]["hooks"][0]["timeout"],
        12
    );
}

#[test]
fn a_timeout_racing_an_answer_gives_one_decision() {
    let home = home();
    let _app = App::open(&home);
    let since = seq(&home);
    let mut params = task("slack-send");
    params["deadline_ms"] = json!(1000);
    let r = start(&home, &params);
    calls_out(&home, since, &r, 1);
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(1000) {
        thread::sleep(Duration::from_millis(20));
    }
    let reply = decide(&home, &r, CALL_ID, "allow", "just in time?");
    let events = end_of(&home, since, &r);
    let decisions = decided(&events, &r);
    assert_eq!(decisions.len(), 1, "{events:#?}");
    let by = decisions[0]["data"]["by"].as_str().unwrap();
    match reply.code() {
        Some(0) => assert_eq!(by, "app"),
        _ => {
            assert_eq!(reply.error_code(), "too_late", "{reply:?}");
            assert_eq!(by, "timeout");
        }
    }
}

// --- rlxd's own rules --------------------------------------------------------------------------

/// Starts `params` and waits for its end: the call must be denied by rlxd, the app never
/// asked. The reason.
fn denied_by_rlxd(home: &TestHome, params: &Value) -> String {
    let since = seq(home);
    let r = start(home, params);
    let events = end_of(home, since, &r);
    assert!(
        of(&events, &r, Some("run.tool_call")).is_empty(),
        "{params}: {events:#?}"
    );
    let decisions = decided(&events, &r);
    assert_eq!(decisions.len(), 1, "{params}: {events:#?}");
    assert_eq!(decisions[0]["data"]["by"], "rlxd", "{params}");
    assert_eq!(decisions[0]["data"]["decision"], "deny", "{params}");
    assert_eq!(replies(home, &r)[0]["decision"], "deny", "{params}");
    assert_eq!(tool_runs(home, &r), 0, "{params}");
    assert_eq!(entry(&ledger(home, &r), CALL_ID)["by"], "rlxd");
    decisions[0]["data"]["reason"].as_str().unwrap().to_owned()
}

#[test]
fn rlxds_rules_deny_first_mount_kind_and_pinned_call() {
    let home = home();
    let _app = App::open(&home);
    let slack = json!({"kind": "connectors", "connectors": ["claude.ai Slack"]});
    // The mount.
    let mut no_tools = task("slack-read");
    no_tools["mount"] = json!({"kind": "no_tools"});
    assert!(denied_by_rlxd(&home, &no_tools).contains("no_tools"));
    let mut gmail = task("slack-read");
    gmail["mount"] = json!({"kind": "connectors", "connectors": ["claude.ai Gmail"]});
    assert!(denied_by_rlxd(&home, &gmail).contains("mount"));
    // The kind: widget_refresh and watch make read calls only; a null hint is a write.
    let refresh =
        |scenario: &str| json!({"kind": "widget_refresh", "prompt": scenario, "mount": slack});
    assert!(denied_by_rlxd(&home, &refresh("slack-send")).contains("read-only"));
    assert!(denied_by_rlxd(&home, &refresh("write-call")).contains("read-only"));
    let watch = json!({"kind": "watch", "prompt": "slack-send", "mount": slack,
        "pinned_call": {"tool": READ, "input_sha256": READ_SHA256}});
    assert!(denied_by_rlxd(&home, &watch).contains("read-only"));
    // The pinned call: another input is not it.
    let send = json!({"kind": "send_executor", "prompt": "slack-send", "mount": slack,
        "pinned_call": {"tool": SEND, "input_sha256": OTHER_SHA256}});
    assert!(denied_by_rlxd(&home, &send).contains("pinned"));
    // A read in a widget refresh goes on to the app.
    let since = seq(&home);
    let r = start(&home, &refresh("slack-read"));
    calls_out(&home, since, &r, 1);
    assert_eq!(
        decide(&home, &r, CALL_ID, "allow", "a read").code(),
        Some(0)
    );
    end_of(&home, since, &r);
    assert_eq!(tool_runs(&home, &r), 1);
}

#[test]
fn a_pinned_call_is_made_once_and_the_ledger_keeps_that() {
    let home = home();
    let _app = App::open(&home);
    for kind in ["send_executor", "watch"] {
        let params = json!({"run_id": run_id(), "kind": kind, "prompt": "x",
            "mount": {"kind": "connectors", "connectors": ["claude.ai Slack"]}});
        let run = home.rlxd(&["call", "run.start", &params.to_string()]);
        assert_eq!(run.error_code(), "pinned_call_required", "{kind}: {run:?}");
    }
    let since = seq(&home);
    let params = json!({"kind": "send_executor", "prompt": "send-twice",
        "mount": {"kind": "connectors", "connectors": ["claude.ai Slack"]},
        "pinned_call": {"tool": SEND, "input_sha256": INPUT_SHA256}});
    let r = start(&home, &params);
    calls_out(&home, since, &r, 1);
    assert_eq!(
        decide(&home, &r, CALL_ID, "allow", "send it").code(),
        Some(0)
    );
    let events = end_of(&home, since, &r);
    // The second call never reached the app.
    assert_eq!(
        of(&events, &r, Some("run.tool_call")).len(),
        1,
        "{events:#?}"
    );
    let decisions = decided(&events, &r);
    assert_eq!(decisions.len(), 2, "{events:#?}");
    assert_eq!(decisions[1]["data"]["call_id"], SECOND_CALL_ID);
    assert_eq!(decisions[1]["data"]["by"], "rlxd");
    assert_eq!(decisions[1]["data"]["reason"], "pinned_call_used");
    let answers = replies(&home, &r);
    assert_eq!(answers[0]["decision"], "allow");
    assert_eq!(answers[1]["decision"], "deny");
    assert_eq!(tool_runs(&home, &r), 1);
    assert_eq!(record(&home, &r)["pinned_call_used"], true);
    // After a restart too.
    home.stop();
    home.start_dev();
    assert_eq!(record(&home, &r)["pinned_call_used"], true);
    let calls = ledger(&home, &r);
    assert_eq!(entry(&calls, CALL_ID)["state"], "ok");
    assert_eq!(entry(&calls, SECOND_CALL_ID)["by"], "rlxd");
}

#[test]
fn parallel_calls_on_a_pinned_run_get_one_allow() {
    let home = home();
    let _app = App::open(&home);
    let since = seq(&home);
    let params = json!({"kind": "send_executor", "prompt": "send-parallel",
        "mount": {"kind": "connectors", "connectors": ["claude.ai Slack"]},
        "pinned_call": {"tool": SEND, "input_sha256": INPUT_SHA256}});
    let r = start(&home, &params);
    // Both wait for the app together: each is the pinned call, until one is allowed.
    calls_out(&home, since, &r, 2);
    let first = decide(&home, &r, CALL_ID, "allow", "send it");
    assert_eq!(first.data()["decision"], "allow", "{first:?}");
    let second = decide(&home, &r, SECOND_CALL_ID, "allow", "send it too");
    assert_eq!(second.code(), Some(0), "{second:?}");
    assert_eq!(
        second.data()["decision"],
        "deny",
        "rlxd's deny was taken: {second:?}"
    );
    let events = end_of(&home, since, &r);
    let decisions = decided(&events, &r);
    assert_eq!(decisions.len(), 2, "{events:#?}");
    assert_eq!(decisions[1]["data"]["by"], "rlxd");
    assert_eq!(decisions[1]["data"]["reason"], "pinned_call_used");
    let answers = replies(&home, &r);
    let allowed = answers.iter().filter(|a| a["decision"] == "allow").count();
    assert_eq!(allowed, 1, "{answers:#?}");
    assert_eq!(tool_runs(&home, &r), 1);
    let calls = ledger(&home, &r);
    assert_eq!(entry(&calls, CALL_ID)["state"], "ok");
    assert_eq!(entry(&calls, SECOND_CALL_ID)["by"], "rlxd");
}

#[test]
fn a_tool_of_a_look_alike_server_is_not_the_mounted_connectors() {
    let home = home();
    let _app = App::open(&home);
    // A local server listed beside the connector (or alone) whose name Claude Code writes
    // as the connector's, or as its with more after `__`: denied by rlxd, never the app's.
    // So is a local server named exactly `claude.ai Slack`: a claude.ai connector is one
    // whose init source is claude.ai's.
    for scenario in [
        "lookalike-nested",
        "lookalike-evil",
        "lookalike-same",
        "lookalike-local",
        "impostor",
    ] {
        let reason = denied_by_rlxd(&home, &task(scenario));
        assert!(
            reason.contains("more than one") || reason.contains("not a tool of the connectors"),
            "{scenario}: {reason}"
        );
    }
    // A connector whose name Claude Code collapses (`claude_ai_Slack_Beta`) is its own.
    let since = seq(&home);
    let mut beta = task("beta-send");
    beta["mount"] = json!({"kind": "connectors", "connectors": ["claude.ai Slack (Beta)"]});
    let r = start(&home, &beta);
    let events = calls_out(&home, since, &r, 1);
    let tool = &of(&events, &r, Some("run.tool_call"))[0]["data"]["tool"];
    assert_eq!(tool, "mcp__claude_ai_Slack_Beta__send");
    assert_eq!(
        decide(&home, &r, CALL_ID, "deny", "not now").code(),
        Some(0)
    );
    end_of(&home, since, &r);
}

#[test]
fn a_tool_the_runs_init_did_not_list_is_denied_tool_not_in_init() {
    let home = home();
    let _app = App::open(&home);
    // Only `claude.ai Slack` is listed, and the call's tool, though its name reads as a
    // Slack tool's, isn't in the init's tools: rlxd denies it, the app never sees it.
    let reason = denied_by_rlxd(&home, &task("evil-unlisted"));
    assert_eq!(reason, "tool_not_in_init");
    // A tool the init listed, of the mounted connector, reaches the app.
    let since = seq(&home);
    let r = start(&home, &task("slack-send"));
    let events = calls_out(&home, since, &r, 1);
    assert_eq!(
        of(&events, &r, Some("run.tool_call"))[0]["data"]["tool"],
        SEND
    );
    assert_eq!(
        decide(&home, &r, CALL_ID, "deny", "not now").code(),
        Some(0)
    );
    end_of(&home, since, &r);
}

#[test]
fn an_allow_whose_ledger_entry_cant_be_written_is_a_deny() {
    let home = home_with(&[("RLXD_TEST_FAIL_LEDGER", "ledgerless")]);
    let _app = App::open(&home);
    let since = seq(&home);
    let mut params = task("slack-send");
    params["space"] = json!("ledgerless");
    let r = start(&home, &params);
    calls_out(&home, since, &r, 1);
    let reply = decide(&home, &r, CALL_ID, "allow", "send it");
    assert_eq!(reply.code(), Some(0), "{reply:?}");
    assert_eq!(
        reply.data()["decision"],
        "deny",
        "rlxd's deny was taken: {reply:?}"
    );
    let events = end_of(&home, since, &r);
    let decisions = decided(&events, &r);
    assert_eq!(decisions.len(), 1, "{events:#?}");
    assert_eq!(decisions[0]["data"]["decision"], "deny");
    assert_eq!(decisions[0]["data"]["by"], "rlxd");
    assert!(
        decisions[0]["data"]["reason"]
            .as_str()
            .unwrap()
            .contains("ledger"),
        "{events:#?}"
    );
    assert_eq!(replies(&home, &r)[0]["decision"], "deny");
    assert_eq!(tool_runs(&home, &r), 0);
    assert!(home.log().contains("the ledger was not written"));
    // The deny replaced the allow's entry: one entry, deny by rlxd, decided, in run.get and
    // in run.json (the run's end wrote it), and still so after a restart. Never "allow by
    // the app" for a call that never ran.
    let denied = |entry: &Value| {
        entry["decision"] == "deny" && entry["by"] == "rlxd" && entry["state"] == "decided"
    };
    let calls = ledger(&home, &r);
    assert!(denied(entry(&calls, CALL_ID)), "{calls:#?}");
    let written = record(&home, &r)["calls"].as_array().cloned().unwrap();
    assert!(denied(entry(&written, CALL_ID)), "{written:#?}");
    let first = r.clone();

    // A pinned run: its pinned call stays used, so the next call is denied too.
    let since = seq(&home);
    let pinned = json!({"kind": "send_executor", "prompt": "send-twice", "space": "ledgerless",
        "mount": {"kind": "connectors", "connectors": ["claude.ai Slack"]},
        "pinned_call": {"tool": SEND, "input_sha256": INPUT_SHA256}});
    let p = start(&home, &pinned);
    calls_out(&home, since, &p, 1);
    assert_eq!(
        decide(&home, &p, CALL_ID, "allow", "send it").data()["decision"],
        "deny"
    );
    let events = end_of(&home, since, &p);
    assert_eq!(
        of(&events, &p, Some("run.tool_call")).len(),
        1,
        "{events:#?}"
    );
    let decisions = decided(&events, &p);
    assert_eq!(
        decisions[1]["data"]["reason"], "pinned_call_used",
        "{events:#?}"
    );
    assert_eq!(tool_runs(&home, &p), 0);

    home.stop();
    home.start_dev();
    let calls = ledger(&home, &first);
    assert!(
        denied(entry(&calls, CALL_ID)),
        "after a restart: {calls:#?}"
    );
    let calls = ledger(&home, &p);
    assert!(
        denied(entry(&calls, CALL_ID)),
        "after a restart: {calls:#?}"
    );
}

#[test]
fn a_pinned_call_used_before_a_restart_stays_used_in_a_resumed_attempt() {
    let home = home();
    let app = App::open(&home);
    let since = seq(&home);
    let params = json!({"kind": "watch", "prompt": "slack-read", "persist_session": true,
        "mount": {"kind": "connectors", "connectors": ["claude.ai Slack"]},
        "pinned_call": {"tool": READ, "input_sha256": READ_SHA256}});
    let r = start(&home, &params);
    calls_out(&home, since, &r, 1);
    assert_eq!(
        decide(&home, &r, CALL_ID, "allow", "read it").code(),
        Some(0)
    );
    end_of(&home, since, &r);
    drop(app);
    home.stop();
    home.start_dev();
    // The record held it, and so does the rlxd that read it: attempt 2's call never
    // reaches the app.
    let _app = App::open(&home);
    let since = seq(&home);
    let resumed = call(
        &home,
        "run.resume",
        &json!({"run_id": r, "reply": "slack-read"}),
    );
    assert_eq!(resumed["attempt"], 2);
    let events = wait_until(&home, since, 60, "attempt 2's end", |e| {
        of(e, &r, Some("run.done")).len() == 1
    });
    assert!(
        of(&events, &r, Some("run.tool_call")).is_empty(),
        "{events:#?}"
    );
    let decisions = decided(&events, &r);
    assert_eq!(decisions.len(), 1, "{events:#?}");
    assert_eq!(decisions[0]["data"]["call_id"], RESUMED_CALL_ID);
    assert_eq!(decisions[0]["data"]["reason"], "pinned_call_used");
    assert_eq!(tool_runs(&home, &r), 1, "only attempt 1's call ran");
}

/// The tester's live repro: the run's folder made read-only while its call waits, so the
/// ledger can't be written as the app allows it.
#[cfg(unix)]
#[test]
fn an_allow_is_a_deny_when_the_runs_folder_takes_no_write() {
    use std::os::unix::fs::PermissionsExt;

    let home = home();
    let _app = App::open(&home);
    let since = seq(&home);
    let r = start(&home, &task("tool-hang"));
    calls_out(&home, since, &r, 1);
    let folder = home.root().join("rlxd").join("runs").join(&r);
    let set = |mode| std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(mode));
    set(0o500).unwrap();
    let reply = decide(&home, &r, CALL_ID, "allow", "send it");
    set(0o700).unwrap();
    assert_eq!(reply.code(), Some(0), "{reply:?}");
    assert_eq!(reply.data()["decision"], "deny", "{reply:?}");
    let events = end_of(&home, since, &r);
    let decisions = decided(&events, &r);
    assert_eq!(decisions[0]["data"]["by"], "rlxd", "{events:#?}");
    assert_eq!(replies(&home, &r)[0]["decision"], "deny");
    assert_eq!(tool_runs(&home, &r), 0, "the tool never ran");
}

// --- Failing closed ----------------------------------------------------------------------------

#[test]
fn a_hook_connection_dropped_mid_call_fails_closed() {
    let home = home();
    let _app = App::open(&home);
    let since = seq(&home);
    let r = start(&home, &task("slack-send"));
    calls_out(&home, since, &r, 1);
    let dropped = call(&home, "debug.drop_hook_connections", &json!({}));
    assert_eq!(dropped["dropped"], 1, "{dropped}");
    // Decided by rlxd as the connection ends, not left for the run's end.
    let events = wait_until(&home, since, 30, "the dropped call's decision", |e| {
        !decided(e, &r).is_empty()
    });
    let decisions = decided(&events, &r);
    assert_eq!(
        decisions[0]["data"]["reason"], "the hook's connection ended before the call was decided",
        "{events:#?}"
    );
    let events = end_of(&home, since, &r);
    let decisions = decided(&events, &r);
    assert_eq!(decisions.len(), 1, "{events:#?}");
    assert_eq!(decisions[0]["data"]["by"], "rlxd");
    assert_eq!(decisions[0]["data"]["decision"], "deny");
    let answers = replies(&home, &r);
    assert_eq!(answers[0]["decision"], "deny", "{answers:#?}");
    assert_eq!(tool_runs(&home, &r), 0);
    let late = decide(&home, &r, CALL_ID, "allow", "too late");
    assert_eq!(late.error_code(), "already_decided", "{late:?}");
}

#[test]
fn the_hook_denies_when_rlxd_is_not_running_or_stops_mid_call() {
    let home = home();
    let _app = App::open(&home);
    let since = seq(&home);
    let r = start(&home, &task("slack-send"));
    calls_out(&home, since, &r, 1);
    home.stop();
    // The run ended with rlxd; whatever its hook got, it was no allow, and no tool ran.
    let deadline = Instant::now() + Duration::from_secs(30);
    while starts_of(&home, &r).iter().any(|s| s["exit"].is_null()) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(replies(&home, &r).iter().all(|a| a["decision"] != "allow"));
    assert_eq!(tool_runs(&home, &r), 0);
    // With no rlxd at all, the hook itself denies.
    let stdin = json!({"hook_event_name": "PreToolUse", "tool_name": SEND,
        "tool_input": {"x": 1}, "tool_use_id": "toolu_x"});
    let mut command = home.command(&["__test-hook", "pre-tool-use", "--run", &r, "--attempt", "1"]);
    command
        .env("RLXD_HOOK_KEY", "0".repeat(64))
        .stdin(Stdio::piped());
    let run = common::run(command, Some(&stdin.to_string()));
    assert_eq!(run.code(), Some(0), "{run:?}");
    let output: Value = serde_json::from_str(run.stdout.trim()).unwrap();
    assert_eq!(
        output["hookSpecificOutput"]["permissionDecision"], "deny",
        "{run:?}"
    );
}

#[test]
fn a_call_waiting_as_its_run_is_cancelled_is_denied_first() {
    let home = home();
    let _app = App::open(&home);
    let since = seq(&home);
    let r = start(&home, &task("slack-send"));
    calls_out(&home, since, &r, 1);
    call(&home, "run.cancel", &json!({"run_id": r}));
    let events = end_of(&home, since, &r);
    let end = end_event(&events, &r);
    assert_eq!(end["data"]["reason"], "cancelled", "{events:#?}");
    let decisions = decided(&events, &r);
    assert_eq!(decisions.len(), 1, "{events:#?}");
    assert_eq!(decisions[0]["data"]["by"], "rlxd");
    let at = |e: &Value| events.iter().position(|each| each == e).unwrap();
    assert!(at(decisions[0]) < at(end), "denied before the run's end");
    // Claude Code starts its hooks detached: on macOS and Linux the hook outlives the
    // run's process group, its connection still open, and the run's end denies its call.
    // On Windows the job takes it, and its connection's end may deny the call first.
    if cfg!(unix) {
        assert_eq!(
            decisions[0]["data"]["reason"], "the run ended before the call was decided",
            "{events:#?}"
        );
    }
    let late = decide(&home, &r, CALL_ID, "allow", "after the end");
    assert_eq!(late.error_code(), "already_decided", "{late:?}");
    assert_eq!(entry(&ledger(&home, &r), CALL_ID)["state"], "decided");
    assert_eq!(tool_runs(&home, &r), 0);
}

#[test]
fn a_stop_denies_the_runs_other_waiting_calls_too() {
    let home = home();
    let _app = App::open(&home);
    let since = seq(&home);
    let r = start(&home, &task("send-parallel"));
    calls_out(&home, since, &r, 2);
    let params = json!({"run_id": r, "call_id": CALL_ID, "decision": "deny",
        "reason": "ask me first", "stop": true});
    assert_eq!(as_app(&home, "run.decide", &params).code(), Some(0));
    let events = end_of(&home, since, &r);
    let end = end_event(&events, &r);
    assert_eq!(end["type"], "run.needs_you", "{events:#?}");
    assert_eq!(end["data"]["call_id"], CALL_ID);
    let decisions = decided(&events, &r);
    assert_eq!(decisions.len(), 2, "{events:#?}");
    let second = decisions
        .iter()
        .find(|d| d["data"]["call_id"] == SECOND_CALL_ID)
        .unwrap();
    assert_eq!(second["data"]["by"], "rlxd");
    assert_eq!(second["data"]["reason"], "the app stopped the run");
    let late = decide(&home, &r, SECOND_CALL_ID, "allow", "too late");
    assert_eq!(late.error_code(), "already_decided", "{late:?}");
    assert_eq!(tool_runs(&home, &r), 0);
}

#[test]
fn a_crash_in_a_call_leaves_it_unknown() {
    let home = home();
    let _app = App::open(&home);
    let since = seq(&home);
    let r = start(&home, &task("crash-in-call"));
    calls_out(&home, since, &r, 1);
    assert_eq!(decide(&home, &r, CALL_ID, "allow", "go").code(), Some(0));
    let events = end_of(&home, since, &r);
    assert_eq!(end_event(&events, &r)["type"], "run.failed");
    assert_eq!(tool_runs(&home, &r), 1);
    let calls = ledger(&home, &r);
    assert_eq!(entry(&calls, CALL_ID)["state"], "unknown");
    assert_eq!(record(&home, &r)["calls"], json!(calls));
}

#[test]
fn kill_9_of_rlxd_with_a_tool_hanging_leaves_the_call_unknown() {
    let home = home();
    let app = App::open(&home);
    let since = seq(&home);
    let r = start(&home, &task("tool-hang"));
    calls_out(&home, since, &r, 1);
    assert_eq!(decide(&home, &r, CALL_ID, "allow", "go").code(), Some(0));
    let deadline = Instant::now() + Duration::from_secs(30);
    while tool_runs(&home, &r) == 0 {
        assert!(Instant::now() < deadline, "the tool never ran");
        thread::sleep(Duration::from_millis(50));
    }
    // Executing, and so on disk, before the hook had its answer. The gate's write keeps the
    // run's last state event, which the record holds until the log is fsynced (7.5).
    let written = record(&home, &r);
    assert_eq!(written["calls"][0]["state"], "executing");
    assert_eq!(written["last_event"]["type"], "run.started", "{written:#}");
    let pid = home.status()["pid"].as_u64().unwrap();
    drop(app);
    common::kill_hard(pid);
    common::eventually("rlxd is gone", || {
        home.rlxd(&["call", "status"]).code() == Some(3)
    });
    home.start_dev();
    let got = call(&home, "run.get", &json!({"run_id": r}));
    assert_eq!(got["state"], "failed", "{got}");
    assert_eq!(
        entry(got["calls"].as_array().unwrap(), CALL_ID)["state"],
        "unknown"
    );
    assert_eq!(record(&home, &r)["calls"][0]["state"], "unknown");
}

// --- Attempts, spaces, binding -------------------------------------------------------------------

#[test]
fn a_deny_that_stops_the_run_needs_you_and_the_resumed_attempt_is_gated() {
    let home = home();
    let _app = App::open(&home);
    let since = seq(&home);
    let r = start(&home, &task("slack-send"));
    calls_out(&home, since, &r, 1);
    let params = json!({"run_id": r, "call_id": CALL_ID, "decision": "deny",
        "reason": "ask me first", "stop": true});
    assert_eq!(as_app(&home, "run.decide", &params).code(), Some(0));
    let events = end_of(&home, since, &r);
    let end = end_event(&events, &r);
    assert_eq!(end["type"], "run.needs_you", "{events:#?}");
    assert_eq!(end["data"]["reason"], "denied");
    assert_eq!(end["data"]["call_id"], CALL_ID);
    assert_valid("run-event.json", end);

    let resumed = call(&home, "run.resume", &json!({"run_id": r, "reply": "go on"}));
    assert_eq!(resumed["attempt"], 2);
    wait_until(&home, since, 60, "attempt 2's call", |e| {
        of(e, &r, Some("run.tool_call"))
            .iter()
            .any(|c| c["data"]["call_id"] == RESUMED_CALL_ID)
    });
    let settings = home
        .root()
        .join("rlxd")
        .join("runs")
        .join(&r)
        .join("2")
        .join("settings.json");
    let text = std::fs::read_to_string(settings).unwrap();
    assert!(text.contains(&format!("--run {r} --attempt 2")), "{text}");
    assert_eq!(
        decide(&home, &r, RESUMED_CALL_ID, "allow", "now yes").code(),
        Some(0)
    );
    let events = wait_until(&home, since, 60, "attempt 2's end", |e| {
        of(e, &r, Some("run.done")).len() == 1
    });
    assert_eq!(of(&events, &r, Some("run.done"))[0]["data"]["attempt"], 2);
    let answers = replies(&home, &r);
    assert_eq!(answers.last().unwrap()["decision"], "allow", "{answers:#?}");
    let calls = ledger(&home, &r);
    assert_eq!(entry(&calls, CALL_ID)["decision"], "deny");
    assert_eq!(entry(&calls, RESUMED_CALL_ID)["state"], "ok");
}

#[test]
fn two_runs_in_one_space_are_gated_apart() {
    let home = home();
    let _app = App::open(&home);
    let since = seq(&home);
    let a = start(&home, &task("slack-send"));
    let b = start(&home, &task("slack-send"));
    calls_out(&home, since, &a, 1);
    calls_out(&home, since, &b, 1);
    // The same call id in each: each run's own.
    assert_eq!(decide(&home, &a, CALL_ID, "allow", "a yes").code(), Some(0));
    assert_eq!(decide(&home, &b, CALL_ID, "deny", "b no").code(), Some(0));
    end_of(&home, since, &a);
    end_of(&home, since, &b);
    assert_eq!(replies(&home, &a)[0]["decision"], "allow");
    assert_eq!(replies(&home, &b)[0]["decision"], "deny");
    assert_eq!(tool_runs(&home, &a), 1);
    assert_eq!(tool_runs(&home, &b), 0);
    assert_eq!(entry(&ledger(&home, &a), CALL_ID)["state"], "ok");
    assert_eq!(entry(&ledger(&home, &b), CALL_ID)["state"], "decided");
}

#[test]
fn a_hook_request_not_bound_to_the_run_is_refused_before_the_app() {
    // Each start's key, kept outside the home: an outsider that somehow has one.
    let kept = KeyFile::new();
    let keys = &kept.0;
    let home = home_with(&[("FAKE_CLAUDE_KEY_OUT", keys.to_str().unwrap())]);
    let _app = App::open(&home);
    let since = seq(&home);
    // Run B waits on its call; run A, in the same space, knows B's id.
    let b = start(&home, &task("slack-send"));
    calls_out(&home, since, &b, 1);
    let space = call(&home, "space.ensure", &json!({"name": "default"}));
    let space = PathBuf::from(space["path"].as_str().unwrap());
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
    // Each refused for its own binding: the key, the attempt, the key (A's is not B's).
    for (answer, why) in answers.iter().zip(["key", "key", "attempt 2", "key"]) {
        assert_eq!(answer["decision"], "deny", "{answer}");
        let reason = answer["reason"].as_str().unwrap();
        assert!(
            reason.contains("refused") && reason.contains(why),
            "{answer}"
        );
    }
    // None reached the app, or any ledger: A's real call and B's are the only calls.
    let tool_calls: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "run.tool_call")
        .collect();
    assert_eq!(tool_calls.len(), 2, "{events:#?}");
    assert!(tool_calls.iter().all(|c| c["data"]["call_id"] == CALL_ID));
    assert!(decided(&events, &a).is_empty() && decided(&events, &b).is_empty());
    // A process outside every run reaches nothing either: with a key it guessed, or with
    // B's own key (B started first, so its key is the first kept).
    let b_key = std::fs::read_to_string(keys)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_owned();
    for (key, why) in [("1".repeat(64), "key"), (b_key, "not inside the run")] {
        let stdin = json!({"hook_event_name": "PreToolUse", "tool_name": SEND,
            "tool_input": {}, "tool_use_id": "toolu_outsider"});
        let mut command =
            home.command(&["__test-hook", "pre-tool-use", "--run", &b, "--attempt", "1"]);
        command.env("RLXD_HOOK_KEY", key).stdin(Stdio::piped());
        let run = common::run(command, Some(&stdin.to_string()));
        let output: Value = serde_json::from_str(run.stdout.trim()).unwrap();
        let output = &output["hookSpecificOutput"];
        assert_eq!(output["permissionDecision"], "deny", "{run:?}");
        let reason = output["permissionDecisionReason"].as_str().unwrap();
        assert!(reason.contains(why), "{why}: {run:?}");
    }
    // A's real call is the app's.
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
}

#[test]
fn a_request_forged_inside_the_run_never_allows_claude_codes_call() {
    let home = home();
    let _app = App::open(&home);
    let since = seq(&home);
    let r = start(&home, &task("forge"));
    // The forged request comes first, with the real call's id: the app sees it, as a call.
    calls_out(&home, since, &r, 1);
    assert_eq!(
        decide(&home, &r, CALL_ID, "allow", "the app allowed what it saw").code(),
        Some(0)
    );
    let events = end_of(&home, since, &r);
    let answers = replies(&home, &r);
    assert_eq!(answers.len(), 2, "{answers:#?}");
    assert_eq!(answers[0]["label"], "forged");
    assert_eq!(
        answers[0]["decision"], "allow",
        "the forger gets its own answer"
    );
    assert_eq!(answers[1]["label"], "call");
    assert_eq!(
        answers[1]["decision"], "deny",
        "Claude Code's call is not allowed"
    );
    assert_eq!(tool_runs(&home, &r), 0);
    assert_eq!(
        of(&events, &r, Some("run.tool_call")).len(),
        1,
        "{events:#?}"
    );
}

#[test]
fn the_hook_key_is_never_logged_published_or_written() {
    let kept = KeyFile::new();
    let out = &kept.0;
    let home = home_with(&[("FAKE_CLAUDE_KEY_OUT", out.to_str().unwrap())]);
    let _app = App::open(&home);
    let since = seq(&home);
    let r = start(&home, &task("slack-send"));
    calls_out(&home, since, &r, 1);
    assert_eq!(decide(&home, &r, CALL_ID, "allow", "fine").code(), Some(0));
    end_of(&home, since, &r);
    // A refused request too, which rlxd logs.
    let b = start(&home, &task("bad-hooks"));
    calls_out(&home, since, &b, 1);
    assert_eq!(decide(&home, &b, CALL_ID, "deny", "no").code(), Some(0));
    let events = end_of(&home, since, &b);
    let keys: Vec<String> = std::fs::read_to_string(out)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(keys.len(), 2, "{keys:?}");
    for key in &keys {
        assert_eq!(key.len(), 64, "{key}");
        assert!(key.bytes().all(|b| b.is_ascii_hexdigit()), "{key}");
    }
    assert_ne!(keys[0], keys[1], "a key per attempt");
    // Nowhere in the home: rlxd's log, run.json, the run logs, settings, the fixture log.
    let mut files = 0;
    let mut stack = vec![home.root().to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() {
                let Ok(bytes) = std::fs::read(&path) else {
                    continue;
                };
                files += 1;
                for key in &keys {
                    let found = bytes.windows(key.len()).any(|w| w == key.as_bytes());
                    assert!(!found, "the key is in {}", path.display());
                }
            }
        }
    }
    assert!(files > 5, "{files} files");
    assert!(home.log().contains("refused"), "the refusal was logged");
    let published = serde_json::to_string(&events).unwrap();
    for key in &keys {
        assert!(!published.contains(key.as_str()));
    }
}

// --- Outside the gate --------------------------------------------------------------------------

#[test]
fn pty_spawn_is_outside_the_gate() {
    let home = home();
    assert_eq!(home.app_clients(), 0);
    let since = seq(&home);
    let argv = if cfg!(windows) {
        json!(["cmd", "/c", "echo ready & timeout /t 30"])
    } else {
        json!(["sh", "-c", "echo ready; sleep 30"])
    };
    let spawned = call(
        &home,
        "pty.spawn",
        &json!({"argv": argv, "cols": 80, "rows": 24}),
    );
    let id = spawned["pty_id"].as_str().unwrap().to_owned();
    call(&home, "pty.kill", &json!({"pty_id": id}));
    let events = replay(&home, since);
    assert!(
        events
            .iter()
            .all(|e| !e["type"].as_str().unwrap_or("").starts_with("run.tool")),
        "{events:#?}"
    );
}
