//! The connector checks (handoff 0.10.3, 2.4, 5.3's listing, 5.5 and 7.7), through the
//! real socket, with fake-claude standing in for Claude Code on every OS.
//!
//! The 5.5 gate checks run in real time, as the handoff's fixtures time Slack's warming up
//! (3 s, 8 s, never). Each is its own test, so the harness runs them side by side: the
//! binary takes about as long as its slowest, the route-1 `connectors-never` (37 s), not
//! their sum. Each runs on both routes: route 2, the default, and route 1, which a test
//! build takes with `RLXD_TEST_CONNECTOR_ROUTE=1`, announcing
//! `connectors.gate_before_turn`, while fake-claude plays a Claude Code that reports the
//! states before the first message.
//!
//! The 7.7 checks that need two scenarios in one home (a change, a login after a run)
//! switch fake-claude's scenario between runs with `FAKE_CLAUDE_SCENARIO_FILE`.
//!
//! The refresh probe and a login are Claude Code too: they start the `claude` the app's
//! environment resolved to, with that environment, as a run does (4.3).

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use common::runs::{
    fixture_log, gated_start, home_with, home_with_scenario_file, home_with_scenario_file_and,
    resumed, run_id, seq, session_file, start,
};
use common::{TestHome, assert_valid, eventually};
use serde_json::{Value, json};

const SLACK: &str = "claude.ai Slack";

/// The handoff's gated run: a widget refresh mounting Slack, which needs it connected.
fn widget() -> Value {
    json!({"kind": "widget_refresh", "prompt": "digest",
        "mount": {"kind": "connectors", "connectors": [SLACK]},
        "needs_connectors": [SLACK], "space": "default"})
}

/// A task, whose session is kept, mounting Slack and needing it.
fn task() -> Value {
    json!({"kind": "task", "prompt": "digest",
        "mount": {"kind": "connectors", "connectors": [SLACK]},
        "needs_connectors": [SLACK], "space": "default"})
}

/// What `connectors-later-noisy` writes before a start can be confirmed.
const NOISE: &str = "Before the connectors are in: nothing to say yet.";

/// The `connector` topic's events after `since`.
fn connector_events(home: &TestHome, since: u64) -> Vec<Value> {
    home.rlxd(&[
        "events",
        "--topics",
        "connector",
        "--since",
        &since.to_string(),
    ])
    .lines()
}

/// A run that mounts Slack and doesn't need it, as 5.3's list check starts.
fn mounting() -> Value {
    json!({"kind": "widget_refresh", "prompt": "digest",
        "mount": {"kind": "connectors", "connectors": [SLACK]}, "space": "default"})
}

/// The route-1 variable, for `route1`.
fn route(route1: bool) -> Vec<(&'static str, &'static str)> {
    if route1 {
        vec![("RLXD_TEST_CONNECTOR_ROUTE", "1")]
    } else {
        Vec::new()
    }
}

/// `status`'s capabilities hold `connectors.gate_before_turn` exactly on route 1.
fn assert_route(home: &TestHome, route1: bool) {
    let capabilities = home.status()["capabilities"].clone();
    let announced = capabilities
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c == "connectors.gate_before_turn");
    assert_eq!(announced, route1, "{capabilities}");
}

/// `id`'s events in `events`.
fn of<'a>(events: &'a [Value], id: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|e| e["data"]["run_id"] == id)
        .collect()
}

/// The check both Late scenarios share: one `run.started`, with Slack connected.
fn assert_one_started_with_slack(events: &[Value], id: &str) {
    let started: Vec<_> = of(events, id)
        .into_iter()
        .filter(|e| e["type"] == "run.started")
        .collect();
    assert_eq!(started.len(), 1, "{events:#?}");
    let slack: Vec<_> = started[0]["data"]["mcp_servers"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["name"] == SLACK)
        .collect();
    assert_eq!(slack.len(), 1, "{events:#?}");
    assert_eq!(slack[0]["status"], "connected", "{events:#?}");
    assert_eq!(
        of(events, id).last().unwrap()["type"],
        "run.done",
        "{events:#?}"
    );
}

/// The Late check of the fixture log, for the start `n` of `starts` starts: delivered,
/// Slack connected at a time, and the prompt sent no sooner.
fn assert_delivered_after_connected(log: &Value, starts: usize, n: usize) {
    let all = log["starts"].as_array().unwrap();
    assert_eq!(all.len(), starts, "{log:#}");
    let start = &all[n];
    assert_eq!(start["delivered"], true, "{log:#}");
    let connected = start["connected_at"]
        .as_u64()
        .unwrap_or_else(|| panic!("{log:#}"));
    let sent = start["stdin"][0]["at"]
        .as_u64()
        .unwrap_or_else(|| panic!("{log:#}"));
    assert!(sent >= connected, "{log:#}");
}

/// The Never and Needs-auth check: the run's last event is `run.failed {reason:
/// "connectors"}` naming Slack in `status`, and nothing of it was delivered.
fn assert_failed_naming_slack(events: &[Value], id: &str, status: &str) {
    let mine = of(events, id);
    let last = mine.last().unwrap_or_else(|| panic!("{events:#?}"));
    assert_eq!(last["type"], "run.failed", "{events:#?}");
    assert_eq!(last["data"]["reason"], "connectors", "{events:#?}");
    assert_eq!(
        last["data"]["connectors"],
        json!([{"id": SLACK, "status": status}]),
        "{events:#?}"
    );
    assert!(
        !mine
            .iter()
            .any(|e| e["type"] == "run.text" || e["type"] == "run.done"),
        "{events:#?}"
    );
    assert_valid("run-event.json", last);
}

// --- 5.5: the gate, on each route ---------------------------------------------------------

fn late(route1: bool) {
    let home = home_with("connectors-late", &route(route1));
    assert_route(&home, route1);
    let (id, events) = gated_start(&home, &widget(), 60).wait();
    assert_one_started_with_slack(&events, &id);
    let log = fixture_log(&home);
    assert_delivered_after_connected(&log, 1, 0);
    if !route1 {
        // The settle came first.
        assert!(
            log["starts"][0]["stdin"][0]["at"].as_u64().unwrap() >= 5000,
            "{log:#}"
        );
    }
}

#[test]
fn late_route_2_settles_then_delivers_the_first_try() {
    late(false);
}

#[test]
fn late_route_1_waits_for_slack_then_delivers_the_first_try() {
    late(true);
}

fn later(route1: bool) {
    let home = home_with("connectors-later", &route(route1));
    let (id, events) = gated_start(&home, &widget(), 60).wait();
    assert_one_started_with_slack(&events, &id);
    // Slack warming up is no change: the delivered try's init adds it, connected, and
    // nothing of a missed try's init is seen.
    let changes = connector_events(&home, 0);
    let types: Vec<&str> = changes
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert_eq!(types, ["connector.added"], "{changes:#?}");
    assert_eq!(changes[0]["data"]["id"], SLACK, "{changes:#?}");
    let list = home.rlxd(&["call", "connectors.list"]).data();
    assert_eq!(list["connectors"][0]["status"], "connected", "{list:#}");
    let log = fixture_log(&home);
    if route1 {
        // The first try waits it out.
        assert_delivered_after_connected(&log, 1, 0);
    } else {
        // The first try sends at 5 s, sees pending and is ended; the second delivers.
        let starts = log["starts"].as_array().unwrap();
        assert_eq!(starts.len(), 2, "{log:#}");
        assert_eq!(starts[0]["delivered"], false, "{log:#}");
        assert_eq!(starts[1]["delivered"], true, "{log:#}");
        assert!(
            starts[1]["stdin"][0]["at"].as_u64().unwrap() >= 8000,
            "{log:#}"
        );
    }
}

#[test]
fn later_route_2_misses_once_then_delivers_the_second_try() {
    later(false);
}

#[test]
fn later_route_1_waits_it_out_in_the_first_try() {
    later(true);
}

fn later_noisy(route1: bool) {
    let home = home_with("connectors-later-noisy", &route(route1));
    let (id, events) = gated_start(&home, &widget(), 60).wait();
    assert_one_started_with_slack(&events, &id);
    // Every start wrote text before it could be confirmed; none of it is the run's.
    let mine = of(&events, &id);
    assert!(mine.iter().any(|e| e["type"] == "run.text"), "{events:#?}");
    assert!(
        !mine.iter().any(|e| e.to_string().contains(NOISE)),
        "{events:#?}"
    );
}

#[test]
fn text_before_the_connectors_are_confirmed_is_never_delivered_route_2() {
    later_noisy(false);
}

#[test]
fn text_before_the_connectors_are_confirmed_is_never_delivered_route_1() {
    later_noisy(true);
}

/// The reviewer's and the tester's case (#2747, #2750): a task done, then resumed while
/// Slack never connects. Its tries are `--resume`s of the run's own session, so the
/// session file stays, as it was before the resume, and the run resumes on after.
fn resumed_miss(route1: bool) {
    let home = home_with_scenario_file_and("init-connectors", &route(route1));
    let (id, events) = gated_start(&home, &task(), 60).wait();
    assert_eq!(
        of(&events, &id).last().unwrap()["type"],
        "run.done",
        "{events:#?}"
    );
    let file = session_file(&home, &id).expect("the task's session file");
    let before = std::fs::read(&file).unwrap();
    assert!(!before.is_empty());
    std::fs::write(home.root().join("scenario"), "connectors-never").unwrap();
    let events = resumed(&home, &id, "again", 90);
    assert_failed_naming_slack(&events, &id, "pending");
    let log = fixture_log(&home);
    let starts = log["starts"].as_array().unwrap();
    assert_eq!(starts.len(), 4, "{log:#}");
    for start in &starts[1..] {
        assert_eq!(start["delivered"], false, "{log:#}");
        assert!(
            start["argv"]
                .as_array()
                .unwrap()
                .contains(&json!("--resume")),
            "{log:#}"
        );
    }
    if !route1 {
        // Each try was sent the reply, and wrote it to the session, as Claude Code does.
        assert!(
            starts[1..]
                .iter()
                .all(|s| s["stdin"].as_array().unwrap().len() == 1),
            "{log:#}"
        );
    }
    // The run's session is there still, holding nothing of the missed tries.
    assert_eq!(std::fs::read(&file).unwrap(), before, "{}", file.display());
    // And it goes on from it once Slack is connected.
    std::fs::write(home.root().join("scenario"), "init-connectors").unwrap();
    let events = resumed(&home, &id, "once more", 60);
    assert_eq!(
        of(&events, &id).last().unwrap()["type"],
        "run.done",
        "{events:#?}"
    );
    let after = std::fs::read(&file).unwrap();
    assert!(after.len() > before.len() && after.starts_with(&before));
}

#[test]
fn a_resumed_runs_missed_tries_keep_its_session_route_2() {
    resumed_miss(false);
}

#[test]
fn a_resumed_runs_missed_tries_keep_its_session_route_1() {
    resumed_miss(true);
}

#[test]
fn a_cancel_of_an_unconfirmed_try_takes_its_session_writes_back_too() {
    // Route 2: the resumed try sends the reply, which Claude Code writes to the session at
    // once, and its init is late; a cancel then ends it unconfirmed (review #3563, nit 1).
    let home = home_with_scenario_file("init-connectors");
    let (id, events) = gated_start(&home, &task(), 60).wait();
    assert_eq!(
        of(&events, &id).last().unwrap()["type"],
        "run.done",
        "{events:#?}"
    );
    let file = session_file(&home, &id).expect("the task's session file");
    let before = std::fs::read(&file).unwrap();
    std::fs::write(home.root().join("scenario"), "connectors-slow-init").unwrap();
    let since = seq(&home);
    let capture = home.spawn(&[
        "events",
        "--as-app",
        "--topics",
        "run",
        "--since",
        &since.to_string(),
        "--follow",
        "--until",
        "run.done,run.failed,run.needs_you",
        "--timeout",
        "60",
    ]);
    eventually("the capture holds the app role", || home.app_clients() >= 1);
    let resume = json!({"run_id": id, "reply": "again"}).to_string();
    let run = home.rlxd(&["call", "--as-app", "run.resume", &resume]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::fs::metadata(&file).unwrap().len() <= before.len() as u64 {
        assert!(
            std::time::Instant::now() < deadline,
            "the try never wrote its reply"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let cancel = home.rlxd(&[
        "call",
        "--as-app",
        "run.cancel",
        &json!({"run_id": id}).to_string(),
    ]);
    assert_eq!(cancel.code(), Some(0), "{cancel:?}");
    let ended = common::finish(capture);
    let events = ended.lines();
    let last = of(&events, &id).last().copied().cloned().unwrap();
    assert_eq!(last["type"], "run.failed", "{events:#?}");
    assert_eq!(last["data"]["reason"], "cancelled", "{events:#?}");
    assert_eq!(std::fs::read(&file).unwrap(), before, "{}", file.display());
}

#[test]
fn a_first_attempts_missed_tries_leave_no_session_file() {
    // Route 2: each try was sent the prompt, and wrote its session file.
    let home = home_with("connectors-never", &[]);
    let (id, events) = gated_start(&home, &task(), 60).wait();
    assert_failed_naming_slack(&events, &id, "pending");
    assert_eq!(session_file(&home, &id), None);
}

fn never(route1: bool) {
    let home = home_with("connectors-never", &route(route1));
    let (id, events) = gated_start(&home, &widget(), 60).wait();
    assert_failed_naming_slack(&events, &id, "pending");
    let log = fixture_log(&home);
    let starts = log["starts"].as_array().unwrap();
    assert_eq!(starts.len(), 3, "{log:#}");
    assert!(starts.iter().all(|s| s["delivered"] == false), "{log:#}");
    if !route1 {
        // Each try sent the prompt after its settle; none was delivered.
        assert!(
            starts
                .iter()
                .all(|s| s["stdin"].as_array().unwrap().len() == 1),
            "{log:#}"
        );
    } else {
        // No try sent a prompt: none saw Slack connected.
        assert!(
            starts
                .iter()
                .all(|s| s["stdin"].as_array().unwrap().is_empty()),
            "{log:#}"
        );
    }
    // The next try follows the backoff, 2 s then 5 s.
    let first = |s: &Value| s["stdin"][0]["at"].as_u64();
    if let (Some(a), Some(b), Some(c)) = (first(&starts[0]), first(&starts[1]), first(&starts[2])) {
        assert!(b >= a + 2000 && c >= b + 5000, "{log:#}");
    }
}

#[test]
fn never_route_2_fails_connectors_pending_after_three_tries() {
    never(false);
}

#[test]
fn never_route_1_fails_connectors_pending_after_three_tries() {
    never(true);
}

fn needs_auth(route1: bool) {
    let home = home_with("connectors-needs-auth", &route(route1));
    let (id, events) = gated_start(&home, &widget(), 60).wait();
    assert_failed_naming_slack(&events, &id, "needs-auth");
    let log = fixture_log(&home);
    assert_eq!(log["starts"].as_array().unwrap().len(), 1, "{log:#}");
    let raised: Vec<_> = events
        .iter()
        .filter(|e| {
            e["type"] == "needs_you.raised"
                && e["data"]["run_id"] == id
                && e["data"]["reason"] == "auth"
        })
        .collect();
    assert_eq!(raised.len(), 1, "{events:#?}");
    // Raised before the run's end, so a capture that stops at run.failed holds it.
    let at = |t: &str| events.iter().position(|e| e["type"] == t).unwrap();
    assert!(at("needs_you.raised") < at("run.failed"), "{events:#?}");
}

#[test]
fn needs_auth_route_2_stops_at_once_and_needs_you() {
    needs_auth(false);
}

#[test]
fn needs_auth_route_1_stops_at_once_and_needs_you() {
    needs_auth(true);
}

#[test]
fn a_lapsed_sign_ins_needs_you_is_cleared_once_slack_is_signed_in_again() {
    let home = home_with_scenario_file("connectors-needs-auth");
    let (id, events) = gated_start(&home, &widget(), 60).wait();
    assert_failed_naming_slack(&events, &id, "needs-auth");
    let raised = events
        .iter()
        .find(|e| e["type"] == "needs_you.raised" && e["data"]["reason"] == "auth")
        .unwrap_or_else(|| panic!("{events:#?}"));
    let needs = raised["data"]["id"].clone();
    // The run never started, so there is no session to resume: a sign-in settles it.
    let resume = json!({"run_id": id, "reply": "again"}).to_string();
    let refused = home.rlxd(&["call", "--as-app", "run.resume", &resume]);
    assert_eq!(refused.error_code(), "no_session", "{refused:?}");
    // It is out across a restart, kept in the run's record.
    home.stop();
    home.start_dev();
    let since = seq(&home);
    std::fs::write(home.root().join("scenario"), "mcp-login").unwrap();
    login(&home);
    let ended = login_events(&home, since);
    assert_eq!(
        ended.last().unwrap()["type"],
        "connector.login.done",
        "{ended:#?}"
    );
    let cleared = || -> Vec<Value> {
        home.rlxd(&[
            "events",
            "--as-app",
            "--topics",
            "needs_you",
            "--since",
            &since.to_string(),
        ])
        .lines()
        .into_iter()
        .filter(|e| e["type"] == "needs_you.cleared")
        .collect()
    };
    eventually("the needs-you is cleared", || !cleared().is_empty());
    let cleared = cleared();
    assert_eq!(cleared.len(), 1, "{cleared:#?}");
    assert_eq!(cleared[0]["data"]["id"], needs, "{cleared:#?}");
    // Cleared in the record too: the next rlxd has it out no more.
    let record = home
        .root()
        .join("rlxd")
        .join("runs")
        .join(&id)
        .join("run.json");
    let record: Value = serde_json::from_slice(&std::fs::read(record).unwrap()).unwrap();
    assert_eq!(record.get("auth"), None, "{record:#}");
}

#[test]
fn a_fired_job_whose_run_needs_connectors_goes_through_the_gate() {
    let home = home_with("connectors-needs-auth", &[]);
    let since = seq(&home);
    let capture = home.spawn(&[
        "events",
        "--as-app",
        "--topics",
        "run,needs_you",
        "--since",
        &since.to_string(),
        "--follow",
        "--until",
        "run.done,run.failed,run.needs_you",
        "--timeout",
        "60",
    ]);
    eventually("the capture holds the app role", || home.app_clients() >= 1);
    let job = json!({"job_id": "gated", "type": "run", "schedule": {"every_minutes": 60},
        "run": widget(), "catch_up": "skip", "enabled": true});
    let put = home.rlxd(&["call", "--as-app", "job.put", &job.to_string()]);
    assert_eq!(put.code(), Some(0), "{put:?}");
    let fired = home.rlxd(&["call", "--as-app", "job.run_now", r#"{"job_id":"gated"}"#]);
    assert_eq!(fired.code(), Some(0), "{fired:?}");
    let id = fired.data()["run_id"].as_str().unwrap().to_owned();
    let run = common::finish(capture);
    assert_eq!(run.code(), Some(0), "the capture: {run:?}");
    let events = run.lines();
    assert_failed_naming_slack(&events, &id, "needs-auth");
    let log = fixture_log(&home);
    assert_eq!(log["starts"].as_array().unwrap().len(), 1, "{log:#}");
}

#[test]
fn a_run_with_needs_connectors_and_no_tools_is_refused() {
    let home = home_with("init-connectors", &[]);
    let mut params = widget();
    params["kind"] = json!("task");
    params["mount"] = json!({"kind": "no_tools"});
    params["run_id"] = json!(run_id());
    let refused = home.rlxd(&["call", "run.start", &params.to_string()]);
    assert_eq!(refused.error_code(), "kind_mount_mismatch", "{refused:?}");
    params["mount"] = json!({"kind": "connectors", "connectors": [SLACK]});
    params["needs_connectors"] = json!(["a\u{7}b"]);
    let refused = home.rlxd(&["call", "run.start", &params.to_string()]);
    assert_eq!(refused.error_code(), "invalid_params", "{refused:?}");
}

fn stdin_held_open(route1: bool) {
    let home = home_with("stdin-eof-empty", &route(route1));
    let (id, events) = gated_start(&home, &widget(), 60).wait();
    assert_one_started_with_slack(&events, &id);
    // The start that delivered: under a loaded machine an earlier try may have missed its
    // wait and been ended, which is the gate working, not stdin closing early.
    let log = fixture_log(&home);
    let delivered: Vec<&Value> = log["starts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|start| start["delivered"] == true)
        .collect();
    assert_eq!(delivered.len(), 1, "{log:#}");
    assert_eq!(delivered[0]["stdin_eof_at"], Value::Null, "{log:#}");
}

#[test]
fn stdin_eof_empty_route_2_reaches_done_with_slack_as_stdin_is_held_open() {
    stdin_held_open(false);
}

#[test]
fn stdin_eof_empty_route_1_reaches_done_with_slack_as_stdin_is_held_open() {
    stdin_held_open(true);
}

#[test]
fn a_cancel_while_the_gate_waits_ends_the_run_and_starts_no_more_tries() {
    let home = home_with("connectors-never", &[]);
    let since = seq(&home);
    let id = run_id();
    start(&home, &widget(), &id);
    std::thread::sleep(Duration::from_millis(6500));
    let cancel = home.rlxd(&["call", "run.cancel", &json!({"run_id": id}).to_string()]);
    assert_eq!(cancel.code(), Some(0), "{cancel:?}");
    let events = home
        .rlxd(&["events", "--topics", "run", "--since", &since.to_string()])
        .lines();
    let last = of(&events, &id).last().copied().cloned().unwrap();
    assert_eq!(last["type"], "run.failed", "{events:#?}");
    assert_eq!(last["data"]["reason"], "cancelled", "{events:#?}");
    std::thread::sleep(Duration::from_secs(6));
    let log = fixture_log(&home);
    assert!(log["starts"].as_array().unwrap().len() <= 2, "{log:#}");
}

// --- 5.3 and 7.7: the list ----------------------------------------------------------------

/// The tool names `init-connectors/expected-tools.json` holds, sorted.
fn expected_tools() -> Vec<String> {
    let path = common::fake_claude("init-connectors").join("expected-tools.json");
    let mut names: Vec<String> =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    names.sort();
    names
}

#[test]
fn the_list_is_the_latest_inits_with_every_hint_and_valid_and_survives_a_restart() {
    let home = home_with("init-connectors", &[]);
    // A home with no run yet lists none.
    let empty = home.rlxd(&["call", "connectors.list"]);
    assert_eq!(empty.code(), Some(0), "{empty:?}");
    assert_eq!(empty.data()["connectors"], json!([]), "{empty:?}");
    assert_eq!(empty.data()["as_of"], Value::Null, "{empty:?}");
    assert_valid("connectors.json", &empty.data());
    let (id, events) = gated_start(&home, &mounting(), 60).wait();
    assert_eq!(
        of(&events, &id).last().unwrap()["type"],
        "run.done",
        "{events:#?}"
    );
    let list = home.rlxd(&["call", "connectors.list"]);
    assert_eq!(list.code(), Some(0), "{list:?}");
    let data = list.data();
    assert_valid("connectors.json", &data);
    assert_eq!(data["model_turns"], 0, "{data:#}");
    assert_eq!(data["from"], "init", "{data:#}");
    let tools: Vec<&Value> = data["connectors"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|c| c["tools"].as_array().unwrap())
        .collect();
    assert!(!tools.is_empty(), "{data:#}");
    // Claude Code's init lists names only: every hint is there, and null.
    assert!(
        tools
            .iter()
            .all(|t| t.get("read_only_hint") == Some(&Value::Null)),
        "{data:#}"
    );
    let mut names: Vec<String> = tools
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect();
    names.sort();
    assert_eq!(names, expected_tools());
    let slack = &data["connectors"][0];
    assert_eq!(slack["id"], SLACK);
    assert_eq!(slack["source"], "claude.ai");
    assert_eq!(slack["status"], "connected");
    // Kept on disk, 0600, and listed again by the next rlxd.
    let file = home.root().join("rlxd").join("connectors.json");
    assert!(file.is_file(), "{}", file.display());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{}", file.display());
    }
    home.stop();
    home.start_dev();
    let again = home.rlxd(&["call", "connectors.list"]);
    assert_eq!(again.data(), data, "{again:?}");
}

#[test]
fn a_refresh_has_no_route_unless_a_turn_is_allowed_and_the_probe_denies_every_call() {
    let tool = "mcp__claude_ai_Slack__slack_send_message";
    let home = home_with("init-connectors", &[("FAKE_CLAUDE_TRY_TOOL", tool)]);
    let capabilities = home.status()["capabilities"].clone();
    assert!(
        !capabilities
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "connectors.no_turn"),
        "{capabilities}"
    );
    let refused = home.rlxd(&["call", "connectors.list", r#"{"refresh":true}"#]);
    assert_eq!(refused.code(), Some(1), "{refused:?}");
    assert_eq!(refused.error_code(), "no_route", "{refused:?}");
    let since = seq(&home);
    let probed = home.rlxd(&[
        "call",
        "connectors.list",
        r#"{"refresh":true,"allow_turn":true}"#,
    ]);
    assert_eq!(probed.code(), Some(0), "{probed:?}");
    let data = probed.data();
    assert_valid("connectors.json", &data);
    assert_eq!(data["model_turns"], 1, "{data:#}");
    assert_eq!(data["connectors"][0]["id"], SLACK, "{data:#}");
    // rlxd's own process: one turn, its MCP servers loaded, every call denied by rlxd.
    let log = fixture_log(&home);
    let probe = log["starts"].as_array().unwrap().last().unwrap().clone();
    let argv: Vec<&str> = probe["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    let at = argv.iter().position(|a| *a == "--max-turns").unwrap();
    assert_eq!(argv[at + 1], "1", "{probe:#}");
    assert!(!argv.contains(&"--strict-mcp-config"), "{probe:#}");
    assert_eq!(
        probe["hook_replies"].as_array().unwrap().len(),
        1,
        "{probe:#}"
    );
    assert_eq!(probe["hook_replies"][0]["tool"], tool, "{probe:#}");
    assert_eq!(probe["hook_replies"][0]["decision"], "deny", "{probe:#}");
    assert!(
        probe["hook_replies"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("rlxd"),
        "{probe:#}"
    );
    assert_eq!(probe["tool_runs"], 0, "{probe:#}");
    // Not a run, and no run event.
    let runs = home.rlxd(&["call", "run.list"]);
    assert_eq!(runs.data()["runs"], json!([]), "{runs:?}");
    let events = home
        .rlxd(&[
            "events",
            "--topics",
            "run,needs_you",
            "--since",
            &since.to_string(),
        ])
        .lines();
    assert!(events.is_empty(), "{events:#?}");
    // Logged with when and model_turns: 1, and neither its prompt nor what it listed.
    let rlxd_log = home.log();
    assert!(rlxd_log.contains("model_turns: 1"), "{rlxd_log}");
    for content in ["Reply with", "Slack", "slack_"] {
        assert!(!rlxd_log.contains(content), "{content}: {rlxd_log}");
    }
}

#[test]
fn a_status_change_is_one_connector_changed() {
    let home = home_with_scenario_file("init-connectors");
    let (_, events) = gated_start(&home, &mounting(), 60).wait();
    assert!(
        events.iter().any(|e| e["type"] == "run.done"),
        "{events:#?}"
    );
    let s = seq(&home);
    std::fs::write(home.root().join("scenario"), "init-needs-auth").unwrap();
    let (_, events) = gated_start(&home, &mounting(), 60).wait();
    assert!(
        events.iter().any(|e| e["type"] == "run.done"),
        "{events:#?}"
    );
    // The same init again is no change.
    let (_, _) = gated_start(&home, &mounting(), 60).wait();
    let changes: Vec<Value> = home
        .rlxd(&["events", "--topics", "connector", "--since", &s.to_string()])
        .lines()
        .into_iter()
        .filter(|e| e["type"] == "connector.changed" && e["data"]["id"] == SLACK)
        .collect();
    assert_eq!(changes.len(), 1, "{changes:#?}");
    assert_eq!(changes[0]["data"]["status"], "needs-auth", "{changes:#?}");
    assert_eq!(
        changes[0]["data"]["previous_status"], "connected",
        "{changes:#?}"
    );
    assert_valid("connector-changed.json", &changes[0]["data"]);
    let list = home.rlxd(&["call", "connectors.list"]).data();
    assert_eq!(list["connectors"][0]["status"], "needs-auth", "{list:#}");
    // A needs-auth connector mounts no tools.
    assert_eq!(list["connectors"][0]["tools"], json!([]), "{list:#}");
}

#[test]
fn a_connector_an_init_no_longer_lists_is_removed_and_one_it_lists_new_is_added() {
    let home = home_with_scenario_file("init-connectors");
    let (_, events) = gated_start(&home, &mounting(), 60).wait();
    assert!(
        events.iter().any(|e| e["type"] == "run.done"),
        "{events:#?}"
    );
    let added = connector_events(&home, 0);
    assert_eq!(added.len(), 1, "{added:#?}");
    assert_eq!(added[0]["type"], "connector.added", "{added:#?}");
    assert_eq!(
        added[0]["data"],
        json!({"account": 1, "id": SLACK}),
        "{added:#?}"
    );
    assert_valid("connector-id.json", &added[0]["data"]);
    let since = seq(&home);
    std::fs::write(home.root().join("scenario"), "init-other").unwrap();
    let (_, events) = gated_start(&home, &mounting(), 60).wait();
    assert!(
        events.iter().any(|e| e["type"] == "run.done"),
        "{events:#?}"
    );
    let mut changes: Vec<(String, String)> = connector_events(&home, since)
        .iter()
        .map(|e| {
            assert_valid("connector-id.json", &e["data"]);
            (
                e["type"].as_str().unwrap().to_owned(),
                e["data"]["id"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    changes.sort();
    assert_eq!(
        changes,
        [
            ("connector.added".to_owned(), "fixture-local".to_owned()),
            ("connector.removed".to_owned(), SLACK.to_owned())
        ]
    );
    let list = home.rlxd(&["call", "connectors.list"]).data();
    assert_eq!(list["connectors"].as_array().unwrap().len(), 1, "{list:#}");
    assert_eq!(list["connectors"][0]["id"], "fixture-local", "{list:#}");
    // A local server, as init's source says.
    assert_eq!(list["connectors"][0]["source"], "mcp", "{list:#}");
    assert_eq!(
        list["connectors"][0]["tools"],
        json!([{"name": "mcp__fixture-local__echo", "read_only_hint": null}]),
        "{list:#}"
    );
}

// --- 7.7: the login -----------------------------------------------------------------------

/// The `connector.login.*` events after `since`, once one ends the login.
fn login_events(home: &TestHome, since: u64) -> Vec<Value> {
    let run = home.rlxd(&[
        "events",
        "--topics",
        "connector",
        "--since",
        &since.to_string(),
        "--follow",
        "--until",
        "connector.login.done,connector.login.error",
        "--timeout",
        "30",
    ]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    run.lines()
        .into_iter()
        .filter(|e| {
            e["type"]
                .as_str()
                .unwrap_or("")
                .starts_with("connector.login.")
        })
        .collect()
}

/// `connectors.login` of Slack, from a `cli` connection: its `login_id`.
fn login(home: &TestHome) -> String {
    let run = home.rlxd(&[
        "call",
        "connectors.login",
        &json!({"id": SLACK}).to_string(),
    ]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_valid("connectors-login-reply.json", &run.data());
    run.data()["login_id"].as_str().unwrap().to_owned()
}

/// The made-up secrets in fake-claude's login URLs' queries.
const SECRETS: &[&str] = &["fake-state-5e3c7a", "fake-challenge-91d2", "client_id="];

fn assert_no_query_in_log(home: &TestHome) {
    let log = home.log();
    for secret in SECRETS {
        assert!(!log.contains(secret), "{secret}: {log}");
    }
}

#[test]
fn a_login_opens_its_https_url_waits_and_is_done_and_its_query_is_never_logged() {
    // A run first, whose init shows Slack needs-auth, so the login's end is a change.
    let home = home_with_scenario_file("init-needs-auth");
    let (_, events) = gated_start(&home, &mounting(), 60).wait();
    assert!(
        events.iter().any(|e| e["type"] == "run.done"),
        "{events:#?}"
    );
    std::fs::write(home.root().join("scenario"), "mcp-login").unwrap();
    let since = seq(&home);
    let login_id = login(&home);
    let events = login_events(&home, since);
    let types: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(
        types,
        [
            "connector.login.open_url",
            "connector.login.waiting",
            "connector.login.done"
        ],
        "{events:#?}"
    );
    assert!(
        events.iter().all(|e| e["data"]["login_id"] == login_id),
        "{events:#?}"
    );
    assert!(
        events[0]["data"]["url"]
            .as_str()
            .unwrap()
            .starts_with("https://"),
        "{events:#?}"
    );
    assert_eq!(events[0]["data"]["id"], SLACK, "{events:#?}");
    assert_valid("connector-login-open-url.json", &events[0]["data"]);
    assert_valid("connector-login-waiting.json", &events[1]["data"]);
    assert_valid("connector-login-done.json", &events[2]["data"]);
    assert_eq!(events[2]["data"]["status"], "connected", "{events:#?}");
    // `claude mcp login <name>`, by Claude Code's absolute path.
    let log = fixture_log(&home);
    let argv = log["starts"].as_array().unwrap().last().unwrap()["argv"].clone();
    assert!(
        std::path::Path::new(argv[0].as_str().unwrap()).is_absolute(),
        "{argv}"
    );
    assert_eq!(
        argv.as_array().unwrap()[1..],
        [json!("mcp"), json!("login"), json!("--"), json!(SLACK)],
        "{argv}"
    );
    // The connector is connected now, and its PTY is gone.
    let list = home.rlxd(&["call", "connectors.list"]).data();
    assert_eq!(list["connectors"][0]["status"], "connected", "{list:#}");
    eventually("the login's pty is gone", || {
        home.rlxd(&["call", "pty.list"]).data()["ptys"] == json!([])
    });
    assert_no_query_in_log(&home);
}

/// A login with fake-claude `scenario`: its `connector.login.*` events.
fn failed_login(scenario: &str) -> (TestHome, Vec<Value>) {
    let home = home_with(scenario, &[]);
    let since = seq(&home);
    login(&home);
    let events = login_events(&home, since);
    (home, events)
}

#[test]
fn a_login_whose_url_is_not_https_ends_not_https_and_passes_no_url_on() {
    let (home, events) = failed_login("mcp-login-http");
    assert_eq!(events.len(), 1, "{events:#?}");
    assert_eq!(events[0]["type"], "connector.login.error", "{events:#?}");
    assert_eq!(events[0]["data"]["code"], "not_https", "{events:#?}");
    assert_valid("connector-login-error.json", &events[0]["data"]);
    assert!(!events[0].to_string().contains("http://"), "{events:#?}");
    assert_no_query_in_log(&home);
    eventually("the login's pty is gone", || {
        home.rlxd(&["call", "pty.list"]).data()["ptys"] == json!([])
    });
}

#[test]
fn a_login_that_prints_no_url_ends_no_url() {
    let (home, events) = failed_login("mcp-login-silent");
    let types: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["connector.login.error"], "{events:#?}");
    assert_eq!(events[0]["data"]["code"], "no_url", "{events:#?}");
    assert_no_query_in_log(&home);
}

#[test]
fn a_login_that_fails_after_its_url_ends_failed() {
    let (home, events) = failed_login("mcp-login-fails");
    let types: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(
        types,
        [
            "connector.login.open_url",
            "connector.login.waiting",
            "connector.login.error"
        ],
        "{events:#?}"
    );
    assert_eq!(events[2]["data"]["code"], "failed", "{events:#?}");
    assert_no_query_in_log(&home);
}

#[test]
fn every_role_lists_and_remote_may_not_log_in() {
    let home = home_with("mcp-login", &[]);
    for role in ["app", "cli", "tui", "remote"] {
        let mut args = vec!["call"];
        if role == "app" {
            args.push("--as-app");
        } else {
            args.extend(["--role", role]);
        }
        let mut list = args.clone();
        list.push("connectors.list");
        let listed = home.rlxd(&list);
        assert_eq!(listed.code(), Some(0), "{role}: {listed:?}");
    }
    let refused = home.rlxd(&[
        "call",
        "--role",
        "remote",
        "connectors.login",
        &json!({"id": SLACK}).to_string(),
    ]);
    assert_eq!(refused.code(), Some(1), "{refused:?}");
    assert_eq!(refused.error_code(), "role_not_allowed", "{refused:?}");
    let invalid = home.rlxd(&["call", "connectors.login", r#"{"id":""}"#]);
    assert_eq!(invalid.error_code(), "invalid_params", "{invalid:?}");
    // An id `claude mcp login` would read as an option is refused, and starts nothing.
    let option = home.rlxd(&["call", "connectors.login", r#"{"id":"--help"}"#]);
    assert_eq!(option.error_code(), "invalid_params", "{option:?}");
    assert!(!home.root().join("fake-claude.log.json").exists());
    // Remote lists, but may not spend a model turn on a refresh.
    let turn = home.rlxd(&[
        "call",
        "--role",
        "remote",
        "connectors.list",
        r#"{"refresh":true,"allow_turn":true}"#,
    ]);
    assert_eq!(turn.error_code(), "role_not_allowed", "{turn:?}");
    assert!(!home.root().join("fake-claude.log.json").exists());
}

// --- 7.7: the CLI -------------------------------------------------------------------------

/// Whether Python's side must run: rlxd.yml sets `RLXD_INTEROP_REQUIRED`, as for the
/// registry's interop tests. A checkout with no uv passes without it.
fn interop_required() -> bool {
    ["RLXD_INTEROP_REQUIRED", "RLXD_BIN"]
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|v| !v.is_empty()))
}

#[test]
fn the_cli_prints_rlxds_data_bare() {
    if !interop_required() {
        eprintln!("skipped: set RLXD_INTEROP_REQUIRED=1 to run Python's side (rlxd.yml does)");
        return;
    }
    let home = home_with("init-connectors", &[]);
    let (_, events) = gated_start(&home, &mounting(), 60).wait();
    assert!(
        events.iter().any(|e| e["type"] == "run.done"),
        "{events:#?}"
    );
    let data = home.rlxd(&["call", "connectors.list"]).data();
    let repo = common::workspace().parent().unwrap().to_path_buf();
    let output = std::process::Command::new("uv")
        .args(["run", "--quiet", "rlx", "--json", "connectors", "list"])
        .current_dir(&repo)
        .env("AISQUARE_HOME", home.root())
        .env_remove("RLX_HOME")
        .env("RLX_RLXD_BIN", env!("CARGO_BIN_EXE_rlxd"))
        .output()
        .unwrap_or_else(|e| panic!("uv must run here (RLXD_INTEROP_REQUIRED is set): {e}"));
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let printed: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(printed, data);
}

// --- 4.3: the app's environment ---------------------------------------------------------

/// A home whose rlxd has no `RLXD_CLAUDE_BIN`, so it starts the `claude` the app's `PATH`
/// resolves to (4.3), and `<home>/bin/claude`: fake-claude, playing what `<home>/scenario`
/// names at each start. rlxd's own `CLAUDE_CONFIG_DIR` is the home's, never the app's.
fn home_with_the_apps_claude(first: &str) -> (TestHome, PathBuf) {
    let home = TestHome::new().with_own_user();
    let file = home.root().join("scenario");
    std::fs::write(&file, first).unwrap();
    let home = home.with_env("FAKE_CLAUDE_SCENARIO_FILE", file.to_str().unwrap());
    let mut serve = home.command(&["serve", "--detach", "--gate-dev-mode"]);
    serve.env_remove("RLXD_CLAUDE_BIN").stdin(Stdio::null());
    let started = common::run(serve, None);
    assert_eq!(started.code(), Some(0), "{started:?}");
    let bin = home.root().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let fake = Path::new(env!("CARGO_BIN_EXE_fake-claude"));
    let claude = bin.join(if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    });
    if std::fs::hard_link(fake, &claude).is_err() {
        std::fs::copy(fake, &claude).unwrap();
    }
    (home, claude)
}

/// `env.set change` from the app, which must succeed.
fn app_sets(home: &TestHome, change: &Value) {
    let run = home.rlxd(&["call", "--as-app", "env.set", &change.to_string()]);
    assert_eq!(run.code(), Some(0), "{run:?}");
}

/// `connectors.list {refresh: true, allow_turn: true}`: the one-turn probe.
fn refresh(home: &TestHome) -> common::Run {
    home.rlxd(&[
        "call",
        "connectors.list",
        r#"{"refresh":true,"allow_turn":true}"#,
    ])
}

#[test]
fn the_refresh_probe_and_a_login_start_the_apps_claude_with_its_environment() {
    let (home, claude) = home_with_the_apps_claude("init-connectors");
    let config = home.root().join("cc-app");
    std::fs::create_dir_all(&config).unwrap();
    let mut dirs = vec![claude.parent().unwrap().to_path_buf()];
    dirs.extend(home.path_without_real_claude());
    let path = std::env::join_paths(dirs).unwrap();
    let (path, config) = (path.to_str().unwrap(), config.to_str().unwrap());
    app_sets(&home, &json!({"PATH": path, "CLAUDE_CONFIG_DIR": config}));
    // The last start in fake-claude's log: the app's claude, by the path resolved when
    // the app set PATH, with the app's PATH, raw, and `config` its CLAUDE_CONFIG_DIR.
    let last_start = |what: &str, config: Option<&str>| {
        let log = fixture_log(&home);
        let start = log["starts"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(
            start["argv"][0],
            claude.display().to_string(),
            "{what}: {start:#}"
        );
        assert_eq!(
            start["env"]
                .get("CLAUDE_CONFIG_DIR")
                .and_then(Value::as_str),
            config,
            "{what}: {start:#}"
        );
        assert_eq!(start["env"]["PATH"], path, "{what}: {start:#}");
        start
    };

    let probed = refresh(&home);
    assert_eq!(probed.code(), Some(0), "{probed:?}");
    assert_eq!(probed.data()["model_turns"], 1, "{probed:?}");
    // The probe is the default account's (#660): the plain account's home, neither of the
    // two variables, though the app set CLAUDE_CONFIG_DIR.
    let start = last_start("the refresh probe", None);
    assert_eq!(start["env"].get("CLAUDE_CODE_TMPDIR"), None, "{start:#}");

    std::fs::write(home.root().join("scenario"), "mcp-login").unwrap();
    let since = seq(&home);
    login(&home);
    let events = login_events(&home, since);
    assert_eq!(
        events.last().unwrap()["type"],
        "connector.login.done",
        "{events:#?}"
    );
    // A login runs where a run does: on the app's CLAUDE_CONFIG_DIR.
    let start = last_start("the login", Some(config));
    assert_eq!(start["argv"][1], "mcp", "{start:#}");

    // With no claude on the app's PATH, neither starts, and each says where rlxd looked.
    let empty = home.root().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    app_sets(&home, &json!({"PATH": empty.to_str().unwrap()}));
    let starts = fixture_log(&home)["starts"].as_array().unwrap().len();
    let refused = refresh(&home);
    assert_eq!(refused.code(), Some(1), "{refused:?}");
    assert_eq!(refused.error_code(), "claude_not_found", "{refused:?}");
    let message = refused.json()["error"]["message"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(message.contains("on the app's PATH"), "{message}");
    let since = seq(&home);
    login(&home);
    let events = login_events(&home, since);
    assert_eq!(events.len(), 1, "{events:#?}");
    assert_eq!(events[0]["type"], "connector.login.error", "{events:#?}");
    assert_eq!(events[0]["data"]["code"], "claude_not_found", "{events:#?}");
    let message = events[0]["data"]["message"].as_str().unwrap();
    assert!(message.contains("on the app's PATH"), "{events:#?}");
    assert_valid("connector-login-error.json", &events[0]["data"]);
    assert_eq!(
        fixture_log(&home)["starts"].as_array().unwrap().len(),
        starts,
        "nothing started"
    );
}
