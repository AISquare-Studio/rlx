//! `connectors.call`'s checks (handoff 0.10.3, 5.3 phase 2; spike #5, route a), through the
//! real socket, with fake-claude's `mcp-direct` standing in for Claude Code on every OS.
//! The `[real]` check, with a real Claude Code and connector, is the owner's to run: until
//! it confirms the route, a release build serves no `connectors.call`, and a test build
//! serves it only with `RLXD_TEST_CONNECTORS_CALL=1`.
//!
//! `mcp-direct` answers rlxd's control requests as Claude Code 2.1.332 does, and logs every
//! request rlxd sent it (`control_requests`), every answer rlxd gave a `can_use_tool`
//! (`control_answers`), and each input a tool ran with (`tool_inputs`, `tool_runs`).
//!
//! The app is a test client in gate dev mode: an `rlxd events --as-app` capture holds the
//! app role while it runs ([`App`]), and `rlxd call --as-app run.decide` answers.

mod common;

use std::process::Child;
use std::thread;
use std::time::{Duration, Instant};

use common::runs::{fixture_log, home_with, link, run_id, seq};
use common::{TestHome, assert_valid, finish};
use serde_json::{Value, json};

const GITHUB: &str = "claude.ai GitHub";
const READ: &str = "mcp__claude_ai_GitHub__get_check_runs";
const WRITE: &str = "mcp__claude_ai_GitHub__create_issue";

/// `{"ref":"main"}`'s RFC 8785 SHA-256.
const MAIN_SHA256: &str = "a934b65d9e929e7a924f901e05035152e791f1d2575f4b2adcb3721ec55d5e93";

/// The handoff's 5.3 Accept lines' call: this connector, this tool, this input.
const SLACK: &str = "claude.ai Slack";
const SLACK_READ: &str = "mcp__claude_ai_Slack__slack_search_public";

/// The Accept lines' `h`: `{"query":"probe"}`'s RFC 8785 SHA-256.
const H: &str = "ecd24fed4577faae6517c6d884030c22d05f35f86e47538d13cfbce7679d0e95";

/// The Accept lines' input.
fn probe() -> Value {
    json!({"query": "probe"})
}

/// The Accept lines' params, exactly, as the run `r`.
fn accept_params(r: &str) -> Value {
    json!({"run_id": r, "connector": SLACK, "tool": SLACK_READ, "input": probe()})
}

/// The capability's switch, as a test build reads it.
const ON: (&str, &str) = ("RLXD_TEST_CONNECTORS_CALL", "1");

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

/// A home whose rlxd plays `scenario` with the capability on, `envs` set too.
fn home(scenario: &str, envs: &[(&str, &str)]) -> TestHome {
    let mut all = vec![ON];
    all.extend_from_slice(envs);
    let home = home_with(scenario, &all);
    common::eventually("the pairing call is gone", || home.app_clients() == 0);
    home
}

/// [`home`], its connectors' cache planted first: `cache` as `connectors.json` holds it.
fn home_with_cache(scenario: &str, cache: &Value) -> TestHome {
    let home = TestHome::new();
    let claude = link(home.root(), scenario);
    let config = home.root().join("claude-config");
    let user = home.root().join("user-home");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&user).unwrap();
    let rlxd = home.root().join("rlxd");
    std::fs::create_dir_all(&rlxd).unwrap();
    std::fs::write(rlxd.join("connectors.json"), cache.to_string()).unwrap();
    let home = home
        .with_claude(claude)
        .with_env(ON.0, ON.1)
        .with_env("CLAUDE_CONFIG_DIR", config.to_str().unwrap())
        .with_env("HOME", user.to_str().unwrap())
        .with_env("USERPROFILE", user.to_str().unwrap());
    home.start_dev();
    let paired = home.rlxd(&["call", "--as-app", "status"]);
    assert_eq!(paired.code(), Some(0), "{paired:?}");
    common::eventually("the pairing call is gone", || home.app_clients() == 0);
    home
}

/// The call of `tool` with `input` as the run `r`.
fn params(r: &str, tool: &str, input: &Value) -> Value {
    json!({"run_id": r, "connector": GITHUB, "tool": tool, "input": input})
}

/// `rlxd call connectors.call`, started and left running: [`finish`] waits for it.
fn call_in_background(home: &TestHome, params: &Value) -> Child {
    home.spawn(&["call", "connectors.call", &params.to_string()])
}

/// `rlxd call connectors.call`, run to its end.
fn call(home: &TestHome, params: &Value) -> common::Run {
    home.rlxd(&["call", "connectors.call", &params.to_string()])
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

/// Run `r`'s events, of `type` when given.
fn of<'a>(events: &'a [Value], r: &str, r#type: Option<&str>) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|e| e["data"]["run_id"] == r)
        .filter(|e| r#type.is_none_or(|t| e["type"] == t))
        .collect()
}

/// Run `r`'s one event of `type`.
fn one<'a>(events: &'a [Value], r: &str, r#type: &str) -> &'a Value {
    let found = of(events, r, Some(r#type));
    assert_eq!(found.len(), 1, "{type} of {r}: {events:#?}");
    found[0]
}

/// The app's `run.decide`.
fn decide(home: &TestHome, r: &str, call_id: &Value, decision: &str) -> common::Run {
    let params = json!({"run_id": r, "call_id": call_id, "decision": decision,
        "reason": "the test said so"});
    home.rlxd(&["call", "--as-app", "run.decide", &params.to_string()])
}

/// Run `r`'s ledger, as `run.get` gives it.
fn ledger(home: &TestHome, r: &str) -> Vec<Value> {
    let run = home.rlxd(&["call", "run.get", &json!({"run_id": r}).to_string()]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    run.data()["calls"].as_array().cloned().unwrap_or_default()
}

/// The fixture log's one start: each home makes one call.
fn the_start(home: &TestHome) -> Value {
    let log = fixture_log(home);
    let starts = log["starts"].as_array().unwrap();
    assert_eq!(starts.len(), 1, "{log:#}");
    starts[0].clone()
}

/// The start's control requests of `subtype`.
fn requests<'a>(start: &'a Value, subtype: &str) -> Vec<&'a Value> {
    start["control_requests"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["subtype"] == subtype)
        .collect()
}

/// `mcp-direct/content.json`.
fn content() -> Value {
    let path = common::workspace().join("tests/fixtures/fake-claude/mcp-direct/content.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// A refused call's code and detail.
fn refusal(run: &common::Run) -> (String, Value) {
    assert_eq!(run.code(), Some(1), "{run:?}");
    let json = run.json();
    (run.error_code(), json["error"]["detail"].clone())
}

// --- The capability -------------------------------------------------------------------------

#[test]
fn without_the_capability_a_call_is_no_route_and_starts_nothing() {
    let home = home_with("mcp-direct", &[]);
    let capabilities = home.status()["capabilities"].clone();
    assert!(
        !capabilities
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "connectors.call"),
        "{capabilities}"
    );
    let r = run_id();
    let run = call(&home, &accept_params(&r));
    let (code, _) = refusal(&run);
    assert_eq!(code, "no_route", "{run:?}");
    // No run, and no Claude Code.
    let get = home.rlxd(&["call", "run.get", &json!({"run_id": r}).to_string()]);
    assert_eq!(get.error_code(), "unknown_run", "{get:?}");
    assert!(!home.root().join("fake-claude.log.json").exists());
}

#[test]
fn with_the_capability_it_is_announced_and_remote_may_not_call() {
    let home = home("mcp-direct", &[]);
    let capabilities = home.status()["capabilities"].clone();
    assert!(
        capabilities
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "connectors.call"),
        "{capabilities}"
    );
    let p = params(&run_id(), READ, &json!({"ref": "main"}));
    let remote = home.rlxd(&[
        "call",
        "--role",
        "remote",
        "connectors.call",
        &p.to_string(),
    ]);
    assert_eq!(remote.error_code(), "role_not_allowed", "{remote:?}");
    // A tool that is not the connector's, and params the schema refuses.
    let slack = params(&run_id(), "mcp__claude_ai_Slack__search", &json!({}));
    assert_eq!(call(&home, &slack).error_code(), "invalid_params");
    let mut no_input = params(&run_id(), READ, &json!({}));
    no_input["input"] = json!("main");
    assert_eq!(call(&home, &no_input).error_code(), "invalid_params");
    assert!(!home.root().join("fake-claude.log.json").exists());
}

// --- The gate decides the call ------------------------------------------------------------

#[test]
fn with_no_app_the_call_is_denied_unreachable_with_a_ledger_entry() {
    let home = home("mcp-direct", &[]);
    let since = seq(&home);
    let r = run_id();
    let run = call(&home, &accept_params(&r));
    let (code, detail) = refusal(&run);
    assert_eq!(code, "denied", "{run:?}");
    assert_eq!(detail["by"], "unreachable", "{run:?}");
    assert_eq!(detail["run_id"], r.as_str());
    let calls = ledger(&home, &r);
    assert_eq!(calls.len(), 1, "{calls:#?}");
    assert_eq!(calls[0]["decision"], "deny");
    assert_eq!(calls[0]["by"], "unreachable");
    assert_eq!(calls[0]["tool"], SLACK_READ);
    assert_eq!(calls[0]["input_sha256"], H);
    let events = wait_until(&home, since, "the run ends", |e| {
        !of(e, &r, Some("run.failed")).is_empty()
    });
    let failed = one(&events, &r, "run.failed");
    assert_valid("run-event.json", failed);
    assert_eq!(failed["data"]["reason"], "denied");
    assert_eq!(failed["data"]["kind"], "watch");
    // Claude Code was told no, and ran nothing.
    let start = the_start(&home);
    assert_eq!(start["tool_runs"], 0, "{start:#}");
    assert_eq!(start["control_answers"][0]["response"]["behavior"], "deny");
}

#[test]
fn through_the_gate_an_allow_gives_the_content_with_no_model_turn() {
    let home = home("mcp-direct", &[]);
    let _app = App::open(&home);
    let since = seq(&home);
    let r = run_id();
    let input = probe();
    let reply = call_in_background(&home, &accept_params(&r));
    let events = wait_until(&home, since, "the call reaches the app", |e| {
        !of(e, &r, Some("run.tool_call")).is_empty()
    });
    let asked = one(&events, &r, "run.tool_call");
    assert_valid("run-event.json", asked);
    assert_eq!(asked["data"]["kind"], "watch");
    assert_eq!(asked["data"]["tool"], SLACK_READ);
    assert_eq!(asked["data"]["input"], input);
    assert_eq!(asked["data"]["input_sha256"], H);
    let call_id = asked["data"]["call_id"].clone();
    assert!(
        call_id.as_str().unwrap().starts_with("toolu_mcp_call_"),
        "{call_id}"
    );
    let allowed = decide(&home, &r, &call_id, "allow");
    assert_eq!(allowed.code(), Some(0), "{allowed:?}");
    let run = finish(reply);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let data = run.data();
    assert_valid("connectors-call-reply.json", &data);
    assert_eq!(data["run_id"], r.as_str());
    assert_eq!(data["decision"], "allow");
    assert_eq!(data["content"], content());
    assert_eq!(data["is_error"], false);
    assert_eq!(data["model_turns"], 0);
    // The ledger: allowed by the app, and ran.
    let calls = ledger(&home, &r);
    assert_eq!(calls.len(), 1, "{calls:#?}");
    assert_eq!(calls[0]["state"], "ok");
    assert_eq!(calls[0]["by"], "app");
    // The run's events: started, the call, its decision, its result, done with no turn.
    let events = wait_until(&home, since, "the run ends", |e| {
        !of(e, &r, Some("run.done")).is_empty()
    });
    let types: Vec<&str> = of(&events, &r, None)
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        [
            "run.started",
            "run.tool_call",
            "run.tool_decided",
            "run.tool_result",
            "run.done"
        ],
        "{events:#?}"
    );
    for event in of(&events, &r, None) {
        assert_valid("run-event.json", event);
        assert_eq!(event["data"]["kind"], "watch", "{event}");
    }
    let result = one(&events, &r, "run.tool_result");
    assert_eq!(result["data"]["content"], content());
    assert_eq!(result["data"]["call_id"], call_id);
    let done = one(&events, &r, "run.done");
    assert_eq!(done["data"]["num_turns"], 0);
    assert_eq!(done["data"]["session"], Value::Null);
    let started = one(&events, &r, "run.started");
    assert!(
        started["data"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t == SLACK_READ),
        "{started}"
    );
    // What Claude Code was told: initialize, mcp_status, one mcp_call with
    // check_permissions, the allow with the pinned input, and never a user line.
    let start = the_start(&home);
    let argv: Vec<&str> = start["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    for flag in ["--permission-prompt-tool", "--no-session-persistence"] {
        assert!(argv.contains(&flag), "{argv:?}");
    }
    for flag in ["--settings", "--session-id", "--resume", "--allowedTools"] {
        assert!(!argv.contains(&flag), "{argv:?}");
    }
    assert_eq!(requests(&start, "initialize").len(), 1, "{start:#}");
    assert!(!requests(&start, "mcp_status").is_empty(), "{start:#}");
    let mcp_calls = requests(&start, "mcp_call");
    assert_eq!(mcp_calls.len(), 1, "{start:#}");
    assert_eq!(mcp_calls[0]["tool"], SLACK_READ);
    assert_eq!(mcp_calls[0]["arguments"], input);
    assert_eq!(mcp_calls[0]["check_permissions"], true);
    assert_eq!(mcp_calls[0]["result_not_for_model"], true);
    let answer = &start["control_answers"][0]["response"];
    assert_eq!(answer["behavior"], "allow", "{start:#}");
    assert_eq!(answer["updatedInput"], input);
    assert_eq!(start["tool_runs"], 1);
    assert_eq!(start["tool_inputs"], json!([input]));
    let stdin = start["stdin"].as_array().unwrap();
    assert!(stdin.iter().all(|line| line["type"] != "user"), "{stdin:?}");
    // And it is never resumed.
    let resume = home.rlxd(&["call", "run.resume", &json!({"run_id": r}).to_string()]);
    assert_eq!(resume.error_code(), "not_resumable", "{resume:?}");
}

#[test]
fn a_tools_own_error_is_the_replys_is_error() {
    let home = home("mcp-direct", &[]);
    let _app = App::open(&home);
    let since = seq(&home);
    let r = run_id();
    let reply = call_in_background(&home, &params(&r, READ, &json!({"ref": "missing"})));
    let events = wait_until(&home, since, "the call reaches the app", |e| {
        !of(e, &r, Some("run.tool_call")).is_empty()
    });
    let call_id = one(&events, &r, "run.tool_call")["data"]["call_id"].clone();
    assert_eq!(decide(&home, &r, &call_id, "allow").code(), Some(0));
    let run = finish(reply);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let data = run.data();
    assert_eq!(data["is_error"], true);
    assert_eq!(
        data["content"],
        json!([{"type": "text", "text": "No commit found for the ref missing"}])
    );
    assert_eq!(ledger(&home, &r)[0]["state"], "error");
}

#[test]
fn an_apps_deny_ends_the_run_failed_denied() {
    let home = home("mcp-direct", &[]);
    let _app = App::open(&home);
    let since = seq(&home);
    let r = run_id();
    let reply = call_in_background(&home, &params(&r, READ, &json!({"ref": "main"})));
    let events = wait_until(&home, since, "the call reaches the app", |e| {
        !of(e, &r, Some("run.tool_call")).is_empty()
    });
    let call_id = one(&events, &r, "run.tool_call")["data"]["call_id"].clone();
    assert_eq!(decide(&home, &r, &call_id, "deny").code(), Some(0));
    let run = finish(reply);
    let (code, detail) = refusal(&run);
    assert_eq!(code, "denied", "{run:?}");
    assert_eq!(detail["by"], "app");
    assert_eq!(detail["reason"], "the test said so");
    assert_eq!(detail["call_id"], call_id);
    let events = wait_until(&home, since, "the run ends", |e| {
        !of(e, &r, Some("run.failed")).is_empty()
    });
    assert_eq!(one(&events, &r, "run.failed")["data"]["reason"], "denied");
    assert!(of(&events, &r, Some("run.tool_result")).is_empty());
    let calls = ledger(&home, &r);
    assert_eq!(calls[0]["decision"], "deny");
    assert_eq!(calls[0]["by"], "app");
    let start = the_start(&home);
    assert_eq!(start["tool_runs"], 0, "{start:#}");
    assert_eq!(
        start["control_answers"][0]["response"]["message"],
        "the test said so"
    );
}

#[test]
fn a_write_tool_is_denied_by_rlxd_before_the_app_sees_it() {
    let home = home("mcp-direct", &[]);
    let _app = App::open(&home);
    let since = seq(&home);
    let r = run_id();
    let run = call(&home, &params(&r, WRITE, &json!({"title": "x"})));
    let (code, detail) = refusal(&run);
    assert_eq!(code, "denied", "{run:?}");
    assert_eq!(detail["by"], "rlxd");
    assert!(
        detail["reason"]
            .as_str()
            .unwrap()
            .contains("not marked read-only"),
        "{detail}"
    );
    assert_eq!(detail["call_id"], "rlxd_mcp_call");
    let events = wait_until(&home, since, "the run ends", |e| {
        !of(e, &r, Some("run.failed")).is_empty()
    });
    assert!(
        of(&events, &r, Some("run.tool_call")).is_empty(),
        "{events:#?}"
    );
    let decided = one(&events, &r, "run.tool_decided");
    assert_eq!(decided["data"]["by"], "rlxd");
    assert_eq!(one(&events, &r, "run.failed")["data"]["reason"], "denied");
    // rlxd's rules ran before anything was sent: Claude Code never got the call.
    let start = the_start(&home);
    assert!(requests(&start, "mcp_call").is_empty(), "{start:#}");
    assert_eq!(start["tool_runs"], 0);
    let calls = ledger(&home, &r);
    assert_eq!(calls.len(), 1, "{calls:#?}");
    assert_eq!(calls[0]["call_id"], "rlxd_mcp_call");
    assert_eq!(calls[0]["tool"], WRITE);
    assert_eq!(calls[0]["decision"], "deny");
    assert_eq!(calls[0]["by"], "rlxd");
}

#[test]
fn a_write_tool_never_reaches_a_claude_code_that_would_skip_its_ask() {
    // mcp-direct-unasked runs any call it is sent with no can_use_tool, as a Claude Code
    // whose managed settings allowed it would: rlxd's rules must keep the call from it.
    let home = home("mcp-direct-unasked", &[]);
    let r = run_id();
    let run = call(&home, &params(&r, WRITE, &json!({"title": "x"})));
    let (code, detail) = refusal(&run);
    assert_eq!(code, "denied", "{run:?}");
    assert_eq!(detail["by"], "rlxd");
    let start = the_start(&home);
    assert!(requests(&start, "mcp_call").is_empty(), "{start:#}");
    assert_eq!(start["tool_runs"], 0, "{start:#}");
}

#[test]
fn a_tool_the_cache_knows_writes_is_denied_before_claude_code_starts() {
    let cache = json!({"as_of": "2026-11-27T00:00:00.000Z", "connectors": [
        {"id": GITHUB, "source": "claude.ai", "status": "connected", "tools": [
            {"name": READ, "read_only_hint": true},
            {"name": WRITE, "read_only_hint": false}]}]});
    let home = home_with_cache("mcp-direct", &cache);
    let since = seq(&home);
    let r = run_id();
    let run = call(&home, &params(&r, WRITE, &json!({"title": "x"})));
    let (code, detail) = refusal(&run);
    assert_eq!(code, "denied", "{run:?}");
    assert_eq!(detail["by"], "rlxd");
    assert!(
        detail["reason"].as_str().unwrap().contains("not read-only"),
        "{detail}"
    );
    let events = wait_until(&home, since, "the run ends", |e| {
        !of(e, &r, Some("run.failed")).is_empty()
    });
    assert_eq!(one(&events, &r, "run.failed")["data"]["reason"], "denied");
    assert_eq!(ledger(&home, &r)[0]["by"], "rlxd");
    // No Claude Code was started for it.
    assert!(!home.root().join("fake-claude.log.json").exists());
}

#[test]
fn the_allow_answers_with_the_pinned_input_never_the_requests() {
    // Claude Code asks with the input's numbers spelled `1.0`: the same canonical input,
    // so the gate allows it; the allow still carries rlxd's own input.
    let home = home("mcp-direct-respelled", &[]);
    let _app = App::open(&home);
    let since = seq(&home);
    let r = run_id();
    let input = json!({"ref": "main", "n": 1});
    let reply = call_in_background(&home, &params(&r, READ, &input));
    let events = wait_until(&home, since, "the call reaches the app", |e| {
        !of(e, &r, Some("run.tool_call")).is_empty()
    });
    let asked = one(&events, &r, "run.tool_call");
    assert!(asked["data"]["input"]["n"].is_f64(), "{asked}");
    let call_id = asked["data"]["call_id"].clone();
    assert_eq!(decide(&home, &r, &call_id, "allow").code(), Some(0));
    let run = finish(reply);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let start = the_start(&home);
    let updated = &start["control_answers"][0]["response"]["updatedInput"];
    assert_eq!(*updated, input, "{start:#}");
    assert!(updated["n"].is_u64(), "{updated}");
    assert_eq!(start["tool_inputs"], json!([input]));
}

#[test]
fn a_deny_that_comes_after_the_steps_waits_still_ends_denied() {
    // Each step waits 1.5 s; the app takes 3 s. The deny is the run's end all the same.
    let home = home("mcp-direct", &[("RLXD_TEST_CALL_WAIT_MS", "1500")]);
    let _app = App::open(&home);
    let since = seq(&home);
    let r = run_id();
    let reply = call_in_background(&home, &params(&r, READ, &json!({"ref": "main"})));
    let events = wait_until(&home, since, "the call reaches the app", |e| {
        !of(e, &r, Some("run.tool_call")).is_empty()
    });
    let call_id = one(&events, &r, "run.tool_call")["data"]["call_id"].clone();
    thread::sleep(Duration::from_secs(3));
    assert_eq!(decide(&home, &r, &call_id, "deny").code(), Some(0));
    let run = finish(reply);
    let (code, detail) = refusal(&run);
    assert_eq!(code, "denied", "{run:?}");
    assert_eq!(detail["by"], "app");
    let events = wait_until(&home, since, "the run ends", |e| {
        !of(e, &r, Some("run.failed")).is_empty()
    });
    assert_eq!(one(&events, &r, "run.failed")["data"]["reason"], "denied");
    // It ended as Claude Code ended it, not killed.
    let start = the_start(&home);
    assert_eq!(start["exit"], 0, "{start:#}");
}

#[test]
fn a_deny_by_timeout_ends_the_run_failed_denied() {
    // The app has 3 s to decide, each step waits 1.5 s, and the app never answers.
    let home = home(
        "mcp-direct",
        &[
            ("RLXD_TEST_CALL_WAIT_MS", "1500"),
            ("RLXD_TEST_CALL_DEADLINE_MS", "3000"),
        ],
    );
    let _app = App::open(&home);
    let since = seq(&home);
    let r = run_id();
    let began = Instant::now();
    let run = call(&home, &params(&r, READ, &json!({"ref": "main"})));
    let (code, detail) = refusal(&run);
    assert_eq!(code, "denied", "{run:?}");
    assert_eq!(detail["by"], "timeout", "{run:?}");
    assert!(
        began.elapsed() < Duration::from_secs(30),
        "{:?}",
        began.elapsed()
    );
    let events = wait_until(&home, since, "the run ends", |e| {
        !of(e, &r, Some("run.failed")).is_empty()
    });
    let failed = one(&events, &r, "run.failed");
    assert_eq!(failed["data"]["reason"], "denied", "{failed}");
    assert_eq!(
        one(&events, &r, "run.tool_call")["data"]["deadline_ms"],
        3000
    );
    let calls = ledger(&home, &r);
    assert_eq!(calls[0]["by"], "timeout");
    assert_eq!(calls[0]["decision"], "deny");
    let start = the_start(&home);
    assert_eq!(start["tool_runs"], 0, "{start:#}");
    assert_eq!(start["control_answers"][0]["response"]["behavior"], "deny");
}

/// A call to `scenario`, which asks `can_use_tool` again once allowed: the second ask is
/// denied by rlxd, never the app's, and the run is ended `call_failed`, the tree killed.
fn asked_twice(scenario: &str, second_tool: &str) {
    let home = home(scenario, &[]);
    let _app = App::open(&home);
    let since = seq(&home);
    let r = run_id();
    let reply = call_in_background(&home, &params(&r, READ, &json!({"ref": "main"})));
    let events = wait_until(&home, since, "the call reaches the app", |e| {
        !of(e, &r, Some("run.tool_call")).is_empty()
    });
    let call_id = one(&events, &r, "run.tool_call")["data"]["call_id"].clone();
    assert_eq!(decide(&home, &r, &call_id, "allow").code(), Some(0));
    let run = finish(reply);
    let (code, detail) = refusal(&run);
    assert_eq!(code, "call_failed", "{run:?}");
    assert_eq!(detail["reason"], "error", "{run:?}");
    // Nothing of content.json's (its head_sha) is in the reply.
    assert!(!run.stdout.contains("3d9a1b7"), "{run:?}");
    let events = wait_until(&home, since, "the run ends", |e| {
        !of(e, &r, Some("run.failed")).is_empty()
    });
    let failed = one(&events, &r, "run.failed");
    assert!(
        failed["data"]["message"]
            .as_str()
            .unwrap()
            .contains("for a second call"),
        "{failed}"
    );
    // The second ask never reached the app: one run.tool_call, two decisions.
    assert_eq!(
        of(&events, &r, Some("run.tool_call")).len(),
        1,
        "{events:#?}"
    );
    assert!(
        of(&events, &r, Some("run.tool_result")).is_empty(),
        "{events:#?}"
    );
    let decided = of(&events, &r, Some("run.tool_decided"));
    assert_eq!(decided.len(), 2, "{events:#?}");
    assert_eq!(decided[1]["data"]["decision"], "deny");
    assert_eq!(decided[1]["data"]["by"], "rlxd");
    let calls = ledger(&home, &r);
    assert_eq!(calls.len(), 2, "{calls:#?}");
    // The allowed call may have run: unknown, as the run ended with it executing.
    assert_eq!(calls[0]["call_id"], call_id);
    assert_eq!(calls[0]["input_sha256"], MAIN_SHA256);
    assert_eq!(calls[0]["state"], "unknown");
    assert_eq!(calls[1]["tool"], second_tool);
    assert_eq!(calls[1]["by"], "rlxd");
    assert_eq!(calls[1]["decision"], "deny");
    // Killed: it never got to exit by itself.
    let start = the_start(&home);
    assert_eq!(start["exit"], Value::Null, "{start:#}");
    assert_eq!(start["tool_runs"], 1, "{start:#}");
}

#[test]
fn a_second_can_use_tool_for_the_same_call_ends_the_run() {
    asked_twice("mcp-direct-twice", READ);
}

#[test]
fn a_second_can_use_tool_for_another_tool_ends_the_run() {
    asked_twice("mcp-direct-twice-other", WRITE);
}

// --- No route, and what rlxd does not trust -----------------------------------------------

#[test]
fn a_claude_code_without_check_permissions_is_no_route_and_gets_no_call() {
    let home = home("mcp-direct-old", &[]);
    let _app = App::open(&home);
    let since = seq(&home);
    let r = run_id();
    let run = call(&home, &params(&r, READ, &json!({"ref": "main"})));
    let (code, detail) = refusal(&run);
    assert_eq!(code, "no_route", "{run:?}");
    assert_eq!(detail["run_id"], r.as_str());
    let events = wait_until(&home, since, "the run ends", |e| {
        !of(e, &r, Some("run.failed")).is_empty()
    });
    let failed = one(&events, &r, "run.failed");
    assert_eq!(failed["data"]["reason"], "error");
    assert!(
        failed["data"]["message"]
            .as_str()
            .unwrap()
            .contains("mcp_call_check_permissions_v1"),
        "{failed}"
    );
    let start = the_start(&home);
    assert!(requests(&start, "mcp_call").is_empty(), "{start:#}");
    assert_eq!(start["tool_runs"], 0);
}

#[test]
fn a_call_made_without_can_use_tool_is_a_failure_and_its_content_is_dropped() {
    let home = home("mcp-direct-unasked", &[]);
    let _app = App::open(&home);
    let since = seq(&home);
    let r = run_id();
    let run = call(&home, &params(&r, READ, &json!({"ref": "main"})));
    let (code, detail) = refusal(&run);
    assert_eq!(code, "call_failed", "{run:?}");
    assert_eq!(detail["reason"], "error");
    // Nothing of content.json's (its head_sha) is in the reply.
    assert!(!run.stdout.contains("3d9a1b7"), "{run:?}");
    let events = wait_until(&home, since, "the run ends", |e| {
        !of(e, &r, Some("run.failed")).is_empty()
    });
    let failed = one(&events, &r, "run.failed");
    assert!(
        failed["data"]["message"]
            .as_str()
            .unwrap()
            .contains("without asking rlxd's gate"),
        "{failed}"
    );
    assert!(of(&events, &r, Some("run.tool_result")).is_empty());
    // The ledger keeps it: no one allowed it, and it may have run.
    let calls = ledger(&home, &r);
    assert_eq!(calls.len(), 1, "{calls:#?}");
    assert_eq!(calls[0]["call_id"], "rlxd_mcp_call");
    assert_eq!(calls[0]["by"], "rlxd");
    assert_eq!(calls[0]["state"], "unknown");
    assert_eq!(the_start(&home)["tool_runs"], 1);
}

#[test]
fn a_connector_that_needs_auth_fails_the_run_connectors() {
    let home = home("mcp-direct-needs-auth", &[]);
    let since = seq(&home);
    let r = run_id();
    let run = call(&home, &params(&r, READ, &json!({"ref": "main"})));
    let (code, detail) = refusal(&run);
    assert_eq!(code, "call_failed", "{run:?}");
    assert_eq!(detail["reason"], "connectors");
    let events = wait_until(&home, since, "the run ends", |e| {
        !of(e, &r, Some("run.failed")).is_empty()
    });
    let failed = one(&events, &r, "run.failed");
    assert_eq!(
        failed["data"]["connectors"],
        json!([{"id": GITHUB, "status": "needs-auth"}])
    );
    assert!(requests(&the_start(&home), "mcp_call").is_empty());
}

// --- No settings but rlxd's ---------------------------------------------------------------

/// Writes `settings` to `path`, making its folder.
fn plant(path: &std::path::Path, settings: &Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, settings.to_string()).unwrap();
}

#[test]
fn allow_rules_planted_in_the_space_and_the_users_settings_cant_skip_can_use_tool() {
    // Claude Code runs a call its settings allow with no can_use_tool, and so past every
    // rule of rlxd's: the call's Claude Code must load none of these.
    let home = home("mcp-direct", &[]);
    let rule = json!({"permissions": {"allow": [READ, "mcp__claude_ai_GitHub"]}});
    let space = home.root().join("spaces").join("default").join(".claude");
    plant(&space.join("settings.json"), &rule);
    plant(&space.join("settings.local.json"), &rule);
    plant(
        &home.root().join("claude-config").join("settings.json"),
        &rule,
    );
    plant(
        &home
            .root()
            .join("user-home")
            .join(".claude")
            .join("settings.json"),
        &rule,
    );
    let r = run_id();
    // No app: only the gate's ask can tell the call no, and it must be asked.
    let run = call(&home, &params(&r, READ, &json!({"ref": "main"})));
    let (code, detail) = refusal(&run);
    assert_eq!(code, "denied", "{run:?}");
    assert_eq!(detail["by"], "unreachable", "{run:?}");
    let start = the_start(&home);
    assert_eq!(start["tool_runs"], 0, "{start:#}");
    assert_eq!(start["allowed_by"], Value::Null, "{start:#}");
    assert_eq!(start["control_answers"][0]["response"]["behavior"], "deny");
    // Its working folder is the run's own, empty one: not the space.
    let cwd = std::path::PathBuf::from(start["cwd"].as_str().unwrap());
    assert!(
        cwd.ends_with(std::path::Path::new("runs").join(&r).join("call")),
        "{}",
        cwd.display()
    );
    assert!(!cwd.join(".claude").exists(), "{}", cwd.display());
}

#[test]
fn an_allow_rule_planted_after_claude_code_starts_is_refused_before_the_call() {
    // mcp-direct-plant writes an allow rule into its own working folder once it has
    // answered initialize: rlxd finds it before it sends the call, and makes none.
    let home = home("mcp-direct-plant", &[]);
    let _app = App::open(&home);
    let since = seq(&home);
    let r = run_id();
    let run = call(&home, &params(&r, READ, &json!({"ref": "main"})));
    let (code, detail) = refusal(&run);
    assert_eq!(code, "untrusted_settings", "{run:?}");
    assert_eq!(detail["run_id"], r.as_str());
    let path = std::path::PathBuf::from(detail["path"].as_str().unwrap());
    assert!(
        path.ends_with(std::path::Path::new(".claude").join("settings.json")),
        "{}",
        path.display()
    );
    let events = wait_until(&home, since, "the run ends", |e| {
        !of(e, &r, Some("run.failed")).is_empty()
    });
    let failed = one(&events, &r, "run.failed");
    assert_eq!(failed["data"]["reason"], "error");
    assert!(
        failed["data"]["message"]
            .as_str()
            .unwrap()
            .contains("untrusted_settings"),
        "{failed}"
    );
    assert!(
        of(&events, &r, Some("run.tool_call")).is_empty(),
        "{events:#?}"
    );
    let start = the_start(&home);
    assert!(requests(&start, "mcp_call").is_empty(), "{start:#}");
    assert_eq!(start["tool_runs"], 0);
    // The rule was there: the call would have run unasked.
    let cwd = std::path::PathBuf::from(start["cwd"].as_str().unwrap());
    assert!(cwd.join(".claude").join("settings.json").is_file());
    assert!(ledger(&home, &r).is_empty());
}

// --- Every read is bounded ----------------------------------------------------------------

/// A call to `scenario` that must fail `call_failed`, its message saying `says`, within
/// `seconds`, with Claude Code's tree killed: the run's `run.failed`.
fn bounded(scenario: &str, says: &str, seconds: u64, app: bool) -> (TestHome, String, Value) {
    let home = home(scenario, &[("RLXD_TEST_CALL_WAIT_MS", "1500")]);
    let app = app.then(|| App::open(&home));
    let since = seq(&home);
    let r = run_id();
    let began = Instant::now();
    let reply = call_in_background(&home, &params(&r, READ, &json!({"ref": "main"})));
    if app.is_some() {
        let events = wait_until(&home, since, "the call reaches the app", |e| {
            !of(e, &r, Some("run.tool_call")).is_empty()
        });
        let call_id = one(&events, &r, "run.tool_call")["data"]["call_id"].clone();
        assert_eq!(decide(&home, &r, &call_id, "allow").code(), Some(0));
    }
    let run = finish(reply);
    let took = began.elapsed();
    let (code, detail) = refusal(&run);
    assert_eq!(code, "call_failed", "{run:?}");
    assert_eq!(detail["reason"], "error", "{run:?}");
    assert!(took < Duration::from_secs(seconds), "{took:?}");
    let events = wait_until(&home, since, "the run ends", |e| {
        !of(e, &r, Some("run.failed")).is_empty()
    });
    let failed = one(&events, &r, "run.failed").clone();
    assert!(
        failed["data"]["message"].as_str().unwrap().contains(says),
        "{failed}"
    );
    // Killed: it never got to exit by itself.
    let start = the_start(&home);
    assert_eq!(start["exit"], Value::Null, "{start:#}");
    drop(app);
    (home, r, failed)
}

#[test]
fn a_claude_code_that_never_answers_initialize_is_killed() {
    bounded("mcp-direct-silent", "answer initialize in time", 30, false);
}

#[test]
fn a_control_line_past_the_bound_is_refused_and_the_tree_killed() {
    bounded("mcp-direct-huge", "line_too_long", 30, false);
}

#[test]
fn a_model_turn_on_the_control_channel_is_refused() {
    bounded("mcp-direct-turn", "model turn", 30, false);
}

#[test]
fn a_call_whose_result_never_comes_is_killed_and_left_unknown() {
    let (home, r, _) = bounded(
        "mcp-direct-hang",
        "give the call's result in time",
        40,
        true,
    );
    let calls = ledger(&home, &r);
    assert_eq!(calls.len(), 1, "{calls:#?}");
    assert_eq!(calls[0]["state"], "unknown");
}
