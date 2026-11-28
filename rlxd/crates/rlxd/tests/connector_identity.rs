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
                .map(move |call| (call["tool"].as_str().unwrap().to_owned(), dir.clone()))
        })
        .collect()
}

#[test]
fn with_an_app_each_account_gains_its_identity_from_one_read_call_on_its_own_home() {
    let (home, managed) = identity_home(
        &fixture("Fixture Works", "ada", true),
        &fixture("Other Works", "grace", true),
        &[],
    );
    let _app = App::open(&home);
    let (data, asked) = refresh_deciding(&home, "allow");
    assert_eq!(
        fixture_row(&data, 1)["identity"],
        json!({"workspace": "Fixture Works", "user": "ada"}),
        "{data:#}"
    );
    assert_eq!(
        fixture_row(&data, 2)["identity"],
        json!({"workspace": "Other Works", "user": "grace"}),
        "{data:#}"
    );
    // One call per account, of the read tool, each decided by the app.
    assert_eq!(asked.len(), 2, "{asked:#?}");
    assert!(asked.iter().all(|a| a["tool"] == WHOAMI), "{asked:#?}");
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
    // The values are on the wire, never in rlxd's log.
    for said in ["Fixture Works", "Other Works", "ada", "grace"] {
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
        &fixture("Fixture Works", "ada", true),
        &fixture("Other Works", "grace", true),
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
        &fixture("Fixture Works", "ada", true),
        &fixture("Other Works", "grace", true),
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
        &fixture("Fixture Works", "ada", true),
        &fixture("Other Works", "grace", false),
        &[],
    );
    let _app = App::open(&home);
    let (data, asked) = refresh_deciding(&home, "allow");
    assert_eq!(asked.len(), 1, "slot 1's alone: {asked:#?}");
    assert_eq!(fixture_row(&data, 1)["identity"]["user"], "ada", "{data:#}");
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
        &fixture("Fixture Works", "ada", true),
        &fixture("Other Works", "grace", true),
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
        &fixture("Fixture Works", "ada", true),
        &fixture("Other Works", "grace", true),
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
        &fixture("Fixture Works", "ada", true),
        &fixture("Other Works", "grace", true),
        &[],
    );
    let app = App::open(&home);
    let (data, _) = refresh_deciding(&home, "allow");
    assert_eq!(
        fixture_row(&data, 2)["identity"]["user"],
        "grace",
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
    assert_eq!(fixture_row(&data, 1)["identity"]["user"], "ada", "{data:#}");
}
