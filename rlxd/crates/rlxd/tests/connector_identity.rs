//! The connector map's identity checks (task #14, step 2; acceptance 5): who each account
//! is on a connector, read with one gated `connectors.call` of the connector's read tool,
//! only on an explicit refresh with an app connected to decide it. fake-claude's
//! `mcp-accounts` plays each account's Claude Code; the identity table's test entry is
//! `claude.ai Fixture`'s `whoami`, listed beside a write tool, `post`, which is never called.

mod common;

use std::path::PathBuf;
use std::process::Child;
use std::time::{Duration, Instant};

use common::runs::{fixture_log, home_with_scenario_file_in, seq};
use common::{TestHome, eventually};
use serde_json::{Value, json};

const FIXTURE: &str = "claude.ai Fixture";
const WHOAMI: &str = "mcp__claude_ai_Fixture__whoami";
const POST: &str = "mcp__claude_ai_Fixture__post";

/// An account's `mcp_status` rows: the fixture connector, its `whoami` answering
/// `workspace` and `user` (read-only when `read_only`), and `post`, a write.
fn fixture(workspace: &str, user: &str, read_only: bool) -> Value {
    let said = json!({"workspace": workspace, "user": user}).to_string();
    let whoami_hints = if read_only {
        json!({"readOnly": true})
    } else {
        json!({})
    };
    json!([{"name": FIXTURE, "status": "connected", "source": "claudeai", "tools": [
        {"name": "whoami", "annotations": whoami_hints,
         "result": [{"type": "text", "text": said}]},
        {"name": "post", "annotations": {}},
    ]}])
}

/// A home whose rlxd reads with no turn and makes connectors.call, two accounts listed:
/// slot 1 (the plain one, `<HOME>/.claude`) and slot 2 (a managed one), with `slot_1` and
/// `slot_2` as their connectors. `envs` are set too.
fn identity_home(slot_1: &Value, slot_2: &Value, envs: &[(&str, &str)]) -> (TestHome, PathBuf) {
    let root = TestHome::new();
    let plain = root.root().join("user-home").join(".claude");
    let managed = root.root().join("account-2");
    for (dir, rows) in [(&plain, slot_1), (&managed, slot_2)] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("fake-connectors.json"), rows.to_string()).unwrap();
    }
    let listed = root.root().join("accounts.json");
    let accounts = json!([{"slot": 1, "login": "mark-1"},
        {"slot": 2, "config_dir": managed, "tmp_dir": root.root().join("account-2-tmp"),
         "login": "mark-2"}]);
    std::fs::write(&listed, accounts.to_string()).unwrap();
    let listed = listed.to_str().unwrap().to_owned();
    let mut all = vec![
        ("RLXD_TEST_ACCOUNTS", listed.as_str()),
        ("RLXD_TEST_CONNECTOR_MAP_MS", "off"),
        ("RLXD_TEST_CONNECTORS_NO_TURN", "1"),
        ("RLXD_TEST_CONNECTORS_CALL", "1"),
        ("CLAUDE_CONFIG_DIR", ""),
    ];
    for (name, value) in envs {
        all.retain(|(each, _)| each != name);
        all.push((name, value));
    }
    let home = home_with_scenario_file_in(root, "mcp-accounts", &all);
    eventually("the pairing call is gone", || home.app_clients() == 0);
    (home, managed)
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
        eventually("the app is connected", || home.app_clients() > 0);
        Self { capture }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = self.capture.kill();
        let _ = self.capture.wait();
    }
}

/// `connectors.list {"refresh":true}`, the app answering each call it is asked with
/// `decision` meanwhile: the list, and the calls the app was asked.
fn refresh_deciding(home: &TestHome, decision: &str) -> (Value, Vec<Value>) {
    let since = seq(home);
    let mut refresh = home.spawn(&["call", "connectors.list", r#"{"refresh":true}"#]);
    let mut asked: Vec<Value> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let events = home
            .rlxd(&["events", "--topics", "run", "--since", &since.to_string()])
            .lines();
        for event in events.iter().filter(|e| e["type"] == "run.tool_call") {
            // A call is its run's and its call id's: each Claude Code numbers its own.
            if asked.iter().any(|a| {
                a["run_id"] == event["data"]["run_id"] && a["call_id"] == event["data"]["call_id"]
            }) {
                continue;
            }
            let params = json!({"run_id": event["data"]["run_id"],
                "call_id": event["data"]["call_id"], "decision": decision,
                "reason": "the test said so"});
            let run = home.rlxd(&["call", "--as-app", "run.decide", &params.to_string()]);
            assert_eq!(run.code(), Some(0), "{run:?}");
            asked.push(event["data"].clone());
        }
        if refresh.try_wait().unwrap().is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the refresh never ended: {:#}\n{}",
            Value::Array(starts(home)),
            home.log()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let out = refresh.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    (list(home), asked)
}

/// `connectors.list`, which must succeed: its data.
fn list(home: &TestHome) -> Value {
    let run = home.rlxd(&["call", "connectors.list"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    run.data()
}

/// `slot`'s fixture connector in `data`.
fn fixture_row(data: &Value, slot: u32) -> Value {
    let row = data["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["account"] == slot)
        .unwrap_or_else(|| panic!("no account {slot}: {data:#}"));
    row["connectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == FIXTURE)
        .unwrap_or_else(|| panic!("no {FIXTURE} on {slot}: {data:#}"))
        .clone()
}

/// The fixture log's starts.
fn starts(home: &TestHome) -> Vec<Value> {
    if !home.root().join("fake-claude.log.json").exists() {
        return Vec::new();
    }
    fixture_log(home)["starts"].as_array().unwrap().clone()
}

/// Every `mcp_call` a start was sent, with that start's `CLAUDE_CONFIG_DIR`.
fn mcp_calls(home: &TestHome) -> Vec<(String, Option<String>)> {
    mcp_calls_with_arguments(home)
        .into_iter()
        .map(|(tool, dir, _)| (tool, dir))
        .collect()
}

/// [`mcp_calls`], each with the arguments it was sent.
fn mcp_calls_with_arguments(home: &TestHome) -> Vec<(String, Option<String>, Value)> {
    starts(home)
        .iter()
        .flat_map(|start| {
            let dir = start["env"]["CLAUDE_CONFIG_DIR"]
                .as_str()
                .map(str::to_owned);
            start["mcp_calls"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(move |call| {
                    let tool = call["tool"].as_str().unwrap().to_owned();
                    (tool, dir.clone(), call["arguments"].clone())
                })
        })
        .collect()
}

/// How many identity runs `run.list` holds: the runs started by `rlxd-identity`.
fn identity_runs(home: &TestHome) -> usize {
    let runs = home.rlxd(&["call", "run.list"]).data();
    runs["runs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|run| run["client"] == "rlxd-identity")
        .count()
}

/// Every file under `dir`, as text.
fn texts_under(dir: &std::path::Path) -> String {
    let mut all = String::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return all;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            all.push_str(&texts_under(&path));
        } else if let Ok(bytes) = std::fs::read(&path) {
            all.push_str(&String::from_utf8_lossy(&bytes));
        }
    }
    all
}

#[test]
fn with_an_app_each_account_gains_its_identity_from_one_read_call_on_its_own_home() {
    let (home, managed) = identity_home(
        &fixture("Tidewater Works", "Zoltan", true),
        &fixture("Kestrel Works", "Quinn", true),
        &[],
    );
    let _app = App::open(&home);
    let (data, asked) = refresh_deciding(&home, "allow");
    assert_eq!(
        fixture_row(&data, 1)["identity"],
        json!({"workspace": "Tidewater Works", "user": "Zoltan"}),
        "{data:#}"
    );
    assert_eq!(
        fixture_row(&data, 2)["identity"],
        json!({"workspace": "Kestrel Works", "user": "Quinn"}),
        "{data:#}"
    );
    // One call per account, of the read tool, each decided by the app.
    assert_eq!(asked.len(), 2, "{asked:#?}");
    assert!(asked.iter().all(|a| a["tool"] == WHOAMI), "{asked:#?}");
    // Each sent with the table's fixed input, nothing of the user's.
    for (_, _, arguments) in mcp_calls_with_arguments(&home) {
        assert_eq!(arguments, json!({}), "the fixed input");
    }
    let calls = mcp_calls(&home);
    assert_eq!(calls.len(), 2, "{calls:#?}");
    assert!(
        calls.iter().all(|(tool, _)| tool == WHOAMI),
        "never {POST}: {calls:#?}"
    );
    let homes: Vec<Option<&str>> = calls.iter().map(|(_, dir)| dir.as_deref()).collect();
    assert!(
        homes.contains(&None),
        "slot 1's on the plain home: {homes:?}"
    );
    assert!(
        homes.contains(&Some(managed.to_str().unwrap())),
        "slot 2's on its own: {homes:?}"
    );
    // Each read is a gated watch run like any connectors.call, started by rlxd-identity,
    // so the app can tell it is one.
    let runs = home.rlxd(&["call", "run.list"]).data();
    let identity_runs: Vec<&Value> = runs["runs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|run| run["client"] == "rlxd-identity")
        .collect();
    assert_eq!(identity_runs.len(), 2, "{runs:#}");
    assert!(
        identity_runs
            .iter()
            .all(|run| run["kind"] == "watch" && run["state"] == "done"),
        "{runs:#}"
    );
    // The values are in connectors.list's reply and the cache alone: never in an event
    // (the identity runs' run.tool_result carries no content), a run's folder, or
    // rlxd's log.
    let events = home.rlxd(&["events", "--since", "0"]).stdout.clone();
    let results: Vec<Value> = home
        .rlxd(&["events", "--topics", "run", "--since", "0"])
        .lines()
        .into_iter()
        .filter(|e| e["type"] == "run.tool_result")
        .collect();
    assert_eq!(results.len(), 2, "{results:#?}");
    assert!(
        results.iter().all(|e| e["data"]["content"].is_null()),
        "{results:#?}"
    );
    let runs = texts_under(&home.root().join("rlxd").join("runs"));
    for said in ["Tidewater Works", "Kestrel Works", "Zoltan", "Quinn"] {
        assert!(!events.contains(said), "{said} in an event");
        assert!(!runs.contains(said), "{said} in a run's folder");
        assert!(!home.log().contains(said), "{said} in rlxd's log");
    }
    // A later refresh keeps them, and reads them again: one more call each.
    let (again, asked) = refresh_deciding(&home, "allow");
    assert_eq!(asked.len(), 2, "{asked:#?}");
    assert_eq!(
        fixture_row(&again, 1)["identity"],
        fixture_row(&data, 1)["identity"]
    );
}

#[test]
fn with_no_app_the_identity_stays_null_and_nothing_is_called() {
    let (home, _) = identity_home(
        &fixture("Tidewater Works", "Zoltan", true),
        &fixture("Kestrel Works", "Quinn", true),
        &[],
    );
    let (data, asked) = refresh_deciding(&home, "allow");
    assert!(asked.is_empty(), "{asked:#?}");
    for slot in [1, 2] {
        assert_eq!(
            fixture_row(&data, slot)["identity"],
            Value::Null,
            "{data:#}"
        );
    }
    assert!(mcp_calls(&home).is_empty());
}

#[test]
fn a_denied_read_leaves_the_identity_null() {
    let (home, _) = identity_home(
        &fixture("Tidewater Works", "Zoltan", true),
        &fixture("Kestrel Works", "Quinn", true),
        &[],
    );
    let _app = App::open(&home);
    let (data, asked) = refresh_deciding(&home, "deny");
    assert_eq!(asked.len(), 2, "{asked:#?}");
    for slot in [1, 2] {
        assert_eq!(
            fixture_row(&data, slot)["identity"],
            Value::Null,
            "{data:#}"
        );
    }
    let ran: Vec<Value> = starts(&home)
        .iter()
        .flat_map(|s| s["tool_runs_of"].as_array().cloned().unwrap_or_default())
        .collect();
    assert!(ran.is_empty(), "no tool ran: {ran:#?}");
}

#[test]
fn a_tool_that_accounts_listing_does_not_hold_read_only_is_never_called() {
    let (home, _) = identity_home(
        &fixture("Tidewater Works", "Zoltan", true),
        &fixture("Kestrel Works", "Quinn", false),
        &[],
    );
    let _app = App::open(&home);
    let (data, asked) = refresh_deciding(&home, "allow");
    assert_eq!(asked.len(), 1, "slot 1's alone: {asked:#?}");
    assert_eq!(
        fixture_row(&data, 1)["identity"]["user"],
        "Zoltan",
        "{data:#}"
    );
    assert_eq!(fixture_row(&data, 2)["identity"], Value::Null, "{data:#}");
    assert_eq!(mcp_calls(&home).len(), 1);
    // Not even started for slot 2: one identity run, slot 1's (the gate's own rules would
    // deny slot 2's too, but no Claude Code is started for a call that can't be made).
    let runs = home.rlxd(&["call", "run.list"]).data();
    let identity_runs = runs["runs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|run| run["client"] == "rlxd-identity")
        .count();
    assert_eq!(identity_runs, 1, "{runs:#}");
}

#[test]
fn without_connectors_call_no_identity_is_read() {
    let (home, _) = identity_home(
        &fixture("Tidewater Works", "Zoltan", true),
        &fixture("Kestrel Works", "Quinn", true),
        &[("RLXD_TEST_CONNECTORS_CALL", "0")],
    );
    let _app = App::open(&home);
    let (data, asked) = refresh_deciding(&home, "allow");
    assert!(asked.is_empty(), "{asked:#?}");
    assert_eq!(fixture_row(&data, 1)["identity"], Value::Null, "{data:#}");
    assert!(mcp_calls(&home).is_empty());
}

#[test]
fn the_background_never_reads_an_identity() {
    let (home, _) = identity_home(
        &fixture("Tidewater Works", "Zoltan", true),
        &fixture("Kestrel Works", "Quinn", true),
        &[("RLXD_TEST_CONNECTOR_MAP_MS", "200,1000")],
    );
    let _app = App::open(&home);
    // Ticks go by, each account read more than once, and no call is made.
    eventually("the background reads both accounts again", || {
        starts(&home).len() >= 4
    });
    assert!(mcp_calls(&home).is_empty(), "{:#?}", mcp_calls(&home));
    let data = list(&home);
    assert_eq!(fixture_row(&data, 1)["identity"], Value::Null, "{data:#}");
}

#[test]
fn a_new_sign_in_forgets_the_identity_read_before() {
    let (home, _) = identity_home(
        &fixture("Tidewater Works", "Zoltan", true),
        &fixture("Kestrel Works", "Quinn", true),
        &[],
    );
    let app = App::open(&home);
    let (data, _) = refresh_deciding(&home, "allow");
    assert_eq!(
        fixture_row(&data, 2)["identity"]["user"],
        "Quinn",
        "{data:#}"
    );
    drop(app);
    eventually("the app is gone", || home.app_clients() == 0);
    // Slot 2 signed in as someone else: read again, with no app to ask who.
    let listed = home.root().join("accounts.json");
    let mut accounts: Value =
        serde_json::from_str(&std::fs::read_to_string(&listed).unwrap()).unwrap();
    accounts[1]["login"] = json!("another-mark");
    std::fs::write(&listed, accounts.to_string()).unwrap();
    let (data, _) = refresh_deciding(&home, "allow");
    assert_eq!(fixture_row(&data, 2)["identity"], Value::Null, "{data:#}");
    assert_eq!(
        fixture_row(&data, 1)["identity"]["user"],
        "Zoltan",
        "{data:#}"
    );
}

#[test]
fn a_new_sign_in_forgets_the_identity_at_once_even_when_its_read_then_fails() {
    let (home, _) = identity_home(
        &fixture("Tidewater Works", "Zoltan", true),
        &fixture("Kestrel Works", "Quinn", true),
        &[],
    );
    let app = App::open(&home);
    let (data, _) = refresh_deciding(&home, "allow");
    assert_eq!(
        fixture_row(&data, 2)["identity"]["user"],
        "Quinn",
        "{data:#}"
    );
    drop(app);
    eventually("the app is gone", || home.app_clients() == 0);
    // Slot 2 signed in as someone else, and its next read fails.
    let listed = home.root().join("accounts.json");
    let mut accounts: Value =
        serde_json::from_str(&std::fs::read_to_string(&listed).unwrap()).unwrap();
    accounts[1]["login"] = json!("another-mark");
    std::fs::write(&listed, accounts.to_string()).unwrap();
    // As soon as rlxd hears the new mark (a list that has it ask the brain, no read).
    let unknown = home.rlxd(&["call", "connectors.list", r#"{"account":9}"#]);
    assert_eq!(unknown.error_code(), "no_such_account", "{unknown:?}");
    let data = list(&home);
    assert_eq!(
        fixture_row(&data, 2)["identity"],
        Value::Null,
        "forgotten at once: {data:#}"
    );
    std::fs::write(home.root().join("scenario"), "mcp-accounts-old").unwrap();
    let failed = home.rlxd(&["call", "connectors.list", r#"{"refresh":true}"#]);
    assert_eq!(failed.error_code(), "no_route", "{failed:?}");
    let data = list(&home);
    assert_eq!(
        fixture_row(&data, 2)["identity"],
        Value::Null,
        "forgotten at once: {data:#}"
    );
    // And a good read later, with no app to ask who, has none either.
    std::fs::write(home.root().join("scenario"), "mcp-accounts").unwrap();
    let (data, asked) = refresh_deciding(&home, "allow");
    assert!(asked.is_empty());
    assert_eq!(fixture_row(&data, 2)["identity"], Value::Null, "{data:#}");
    assert_eq!(
        fixture_row(&data, 1)["identity"]["user"],
        "Zoltan",
        "{data:#}"
    );
}

#[test]
fn a_remote_refresh_reads_the_statuses_but_never_an_identity() {
    let (home, _) = identity_home(
        &fixture("Tidewater Works", "Zoltan", true),
        &fixture("Kestrel Works", "Quinn", true),
        &[],
    );
    let _app = App::open(&home);
    let refreshed = home.rlxd(&[
        "call",
        "--role",
        "remote",
        "connectors.list",
        r#"{"refresh":true}"#,
    ]);
    assert_eq!(refreshed.code(), Some(0), "{refreshed:?}");
    let data = refreshed.data();
    assert_eq!(fixture_row(&data, 1)["status"], "connected", "{data:#}");
    assert_eq!(fixture_row(&data, 1)["identity"], Value::Null, "{data:#}");
    assert!(mcp_calls(&home).is_empty());
}

#[test]
fn a_connector_that_is_not_connected_there_gets_no_call() {
    let mut signed_out = fixture("Kestrel Works", "Quinn", true);
    signed_out[0]["status"] = json!("needs-auth");
    let (home, _) = identity_home(
        &fixture("Tidewater Works", "Zoltan", true),
        &signed_out,
        &[],
    );
    let _app = App::open(&home);
    let (data, asked) = refresh_deciding(&home, "allow");
    assert_eq!(asked.len(), 1, "slot 1's alone: {asked:#?}");
    assert_eq!(fixture_row(&data, 2)["identity"], Value::Null, "{data:#}");
    assert_eq!(mcp_calls(&home).len(), 1);
    assert_eq!(identity_runs(&home), 1, "none even started for slot 2");
}

#[test]
fn only_the_tables_tool_qualifies() {
    // Slot 2 lists a read-only tool of the fixture's, but not the table's whoami.
    let mut other_tool = fixture("Kestrel Works", "Quinn", true);
    other_tool[0]["tools"][0]["name"] = json!("whois");
    let (home, _) = identity_home(
        &fixture("Tidewater Works", "Zoltan", true),
        &other_tool,
        &[],
    );
    let _app = App::open(&home);
    let (data, asked) = refresh_deciding(&home, "allow");
    assert_eq!(asked.len(), 1, "slot 1's alone: {asked:#?}");
    assert_eq!(fixture_row(&data, 2)["identity"], Value::Null, "{data:#}");
    assert!(mcp_calls(&home).iter().all(|(tool, _)| tool == WHOAMI));
    assert_eq!(identity_runs(&home), 1, "none even started for slot 2");
}

/// The hold-then-sign-in sequence: slot 2's identity call held while slot 2 is signed in
/// as someone else and rlxd hears it (`hear`), then allowed: its answer is dropped.
fn held_across_a_new_sign_in(hear: fn(&TestHome)) {
    let (home, _) = identity_home(
        &fixture("Tidewater Works", "Zoltan", true),
        &fixture("Kestrel Works", "Quinn", true),
        &[],
    );
    let _app = App::open(&home);
    let since = seq(&home);
    let mut refresh = home.spawn(&["call", "connectors.list", r#"{"refresh":true}"#]);
    // The accounts are asked one after the other: slot 1's call, then slot 2's.
    let asked = |n: usize| {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let calls: Vec<Value> = home
                .rlxd(&["events", "--topics", "run", "--since", &since.to_string()])
                .lines()
                .into_iter()
                .filter(|e| e["type"] == "run.tool_call")
                .map(|e| e["data"].clone())
                .collect();
            if calls.len() >= n {
                return calls[n - 1].clone();
            }
            assert!(
                Instant::now() < deadline,
                "the app was never asked call {n}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    let decide = |call: &Value| {
        let params = json!({"run_id": call["run_id"], "call_id": call["call_id"],
            "decision": "allow", "reason": "the test said so"});
        let run = home.rlxd(&["call", "--as-app", "run.decide", &params.to_string()]);
        assert_eq!(run.code(), Some(0), "{run:?}");
    };
    decide(&asked(1));
    // Slot 2's call is held while slot 2 is signed in as someone else, and rlxd hears it.
    let held = asked(2);
    let listed = home.root().join("accounts.json");
    let mut accounts: Value =
        serde_json::from_str(&std::fs::read_to_string(&listed).unwrap()).unwrap();
    accounts[1]["login"] = json!("another-mark");
    std::fs::write(&listed, accounts.to_string()).unwrap();
    hear(&home);
    // Then the held call is allowed, and its answer, the old sign-in's, lands.
    decide(&held);
    let out = refresh.wait().unwrap();
    assert!(out.success());
    let data = list(&home);
    assert_eq!(
        fixture_row(&data, 2)["identity"],
        Value::Null,
        "dropped: {data:#}"
    );
    assert_eq!(
        fixture_row(&data, 1)["identity"]["user"],
        "Zoltan",
        "{data:#}"
    );
    assert!(
        home.log()
            .contains("identity on claude.ai Fixture is dropped")
    );
}

#[test]
fn an_identity_asked_under_one_sign_in_that_lands_after_a_read_of_another_is_dropped() {
    // A remote refresh: the new mark heard, and slot 2 read under it.
    held_across_a_new_sign_in(|home| {
        let remote = home.rlxd(&[
            "call",
            "--role",
            "remote",
            "connectors.list",
            r#"{"refresh":true}"#,
        ]);
        assert_eq!(remote.code(), Some(0), "{remote:?}");
    });
}

#[test]
fn an_identity_asked_under_one_sign_in_that_lands_once_another_is_heard_is_dropped() {
    // Only heard, no read yet: rlxd's account has the new mark, the entry the old.
    held_across_a_new_sign_in(|home| {
        let unknown = home.rlxd(&["call", "connectors.list", r#"{"account":9}"#]);
        assert_eq!(unknown.error_code(), "no_such_account", "{unknown:?}");
    });
}
