//! The connector map's checks (task #14): each Claude Code account's connectors, read
//! with no model turn through the real socket, with fake-claude's `mcp-accounts` standing
//! in for Claude Code on every OS.
//!
//! The accounts come from `RLXD_TEST_ACCOUNTS`, a file holding what the brain's
//! `accounts` would list: slot 1, the default account, on rlxd's own environment, and
//! slot 2, a managed one with a config dir of its own. Each account's connectors are its
//! config dir's `fake-connectors.json` (slot 1's is `<HOME>/.claude/`, as the read takes
//! `CLAUDE_CONFIG_DIR` away for it). Test builds read with no turn only with
//! `RLXD_TEST_CONNECTORS_NO_TURN=1`, as a release build does once the owner's probe
//! confirms the route.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::runs::{fixture_log, home_with_scenario_file_in, run_id, seq, start};
use common::{TestHome, assert_valid, eventually};
use serde_json::{Value, json};

const SLACK: &str = "claude.ai Slack";

/// Slot 2's login mark, which never leaves rlxd's file.
const MARK: &str = "mark-of-slot-2";

/// `mcp_status` rows: Slack in `status`, with one read tool.
fn slack(status: &str) -> Value {
    json!([{"name": SLACK, "status": status, "source": "claudeai",
        "tools": [{"name": "slack_search_public", "annotations": {"readOnly": true}}]}])
}

/// The two accounts' homes in a test home.
struct Accounts {
    /// Slot 1's config dir: `<HOME>/.claude`.
    plain: PathBuf,
    /// Slot 2's config dir and tmp dir.
    managed: PathBuf,
    managed_tmp: PathBuf,
}

impl Accounts {
    fn of(root: &Path) -> Self {
        Self {
            plain: root.join("user-home").join(".claude"),
            managed: root.join("account-2"),
            managed_tmp: root.join("account-2-tmp"),
        }
    }

    /// Writes `rows` as `slot`'s connectors.
    fn set(&self, slot: u32, rows: &Value) {
        let dir = if slot == 1 {
            &self.plain
        } else {
            &self.managed
        };
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("fake-connectors.json"), rows.to_string()).unwrap();
    }

    /// The brain's list: slot 1, and slot 2 when `two`.
    fn listed(&self, two: bool) -> Value {
        let mut listed = vec![json!({"slot": 1, "label": "main"})];
        if two {
            listed.push(json!({"slot": 2, "label": "work",
                "config_dir": self.managed, "tmp_dir": self.managed_tmp, "login": MARK}));
        }
        Value::Array(listed)
    }
}

/// A home whose rlxd reads with no turn (unless `no_turn` is false), fake-claude playing
/// `mcp-accounts`, two accounts listed: slot 1 with Slack connected, slot 2 with Slack
/// signed out. `envs` are set too.
fn map_home(no_turn: bool, envs: &[(&str, &str)]) -> (TestHome, Accounts) {
    let root = TestHome::new();
    let accounts = Accounts::of(root.root());
    accounts.set(1, &slack("connected"));
    accounts.set(2, &slack("needs-auth"));
    let listed = root.root().join("accounts.json");
    std::fs::write(&listed, accounts.listed(true).to_string()).unwrap();
    let listed = listed.to_str().unwrap().to_owned();
    let mut all = vec![
        ("RLXD_TEST_ACCOUNTS", listed.as_str()),
        ("RLXD_TEST_CONNECTOR_MAP_MS", "off"),
    ];
    if no_turn {
        all.push(("RLXD_TEST_CONNECTORS_NO_TURN", "1"));
    }
    for (name, value) in envs {
        all.retain(|(each, _)| each != name);
        all.push((name, value));
    }
    let home = home_with_scenario_file_in(root, "mcp-accounts", &all);
    (home, accounts)
}

/// `connectors.list params`, which must succeed: its data.
fn list(home: &TestHome, params: &Value) -> Value {
    let run = home.rlxd(&["call", "connectors.list", &params.to_string()]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let data = run.data();
    assert_valid("connectors.json", &data);
    data
}

/// `slot`'s row in `data`.
fn row(data: &Value, slot: u32) -> Value {
    data["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["account"] == slot)
        .unwrap_or_else(|| panic!("no account {slot}: {data:#}"))
        .clone()
}

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

/// No user line reached `start`'s stdin: no model turn.
fn assert_no_user_line(start: &Value) {
    let lines = start["stdin"].as_array().unwrap();
    assert!(lines.iter().all(|line| line["type"] != "user"), "{start:#}");
}

/// The fixture log's starts.
fn starts(home: &TestHome) -> Vec<Value> {
    if !home.root().join("fake-claude.log.json").exists() {
        return Vec::new();
    }
    fixture_log(home)["starts"].as_array().unwrap().clone()
}

#[test]
fn a_refresh_lists_every_accounts_connectors_with_no_turn() {
    let (home, accounts) = map_home(true, &[]);
    let capabilities = home.status()["capabilities"].clone();
    assert!(
        capabilities
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "connectors.no_turn"),
        "{capabilities}"
    );
    let data = list(&home, &json!({"refresh": true}));
    assert_eq!(data["model_turns"], 0, "{data:#}");
    assert_eq!(data["from"], "listing", "{data:#}");
    let slots: Vec<&Value> = data["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| &row["account"])
        .collect();
    assert_eq!(slots, [1, 2], "{data:#}");
    let (one, two) = (row(&data, 1), row(&data, 2));
    assert_eq!(one["label"], "main", "{data:#}");
    assert_eq!(two["label"], "work", "{data:#}");
    for (row, status) in [(&one, "connected"), (&two, "needs-auth")] {
        let slack = &row["connectors"][0];
        assert_eq!(slack["id"], SLACK, "{data:#}");
        assert_eq!(slack["status"], status, "{data:#}");
        assert_eq!(slack["source"], "claude.ai", "{data:#}");
        assert!(slack["checked_at"].is_string(), "{data:#}");
        assert_eq!(slack["identity"], Value::Null, "{data:#}");
        assert_eq!(row["from"], "listing", "{data:#}");
        assert_eq!(row["error"], Value::Null, "{data:#}");
    }
    assert_eq!(
        one["connectors"][0]["tools"][0],
        json!({"name": "mcp__claude_ai_Slack__slack_search_public", "read_only_hint": true}),
        "{data:#}"
    );
    // The top level is the default account's, as before accounts.
    assert_eq!(data["connectors"], one["connectors"], "{data:#}");
    // One start a slot, each on its own home: the default account's with neither
    // variable, the managed one's with both. No user line, no call: no turn.
    let starts = starts(&home);
    assert_eq!(starts.len(), 2, "{starts:#?}");
    let plain = &starts[0]["env"];
    assert_eq!(plain.get("CLAUDE_CONFIG_DIR"), None, "{starts:#?}");
    assert_eq!(plain.get("CLAUDE_CODE_TMPDIR"), None, "{starts:#?}");
    let managed = &starts[1]["env"];
    assert_eq!(
        managed["CLAUDE_CONFIG_DIR"],
        accounts.managed.to_str().unwrap(),
        "{starts:#?}"
    );
    assert_eq!(
        managed["CLAUDE_CODE_TMPDIR"],
        accounts.managed_tmp.to_str().unwrap(),
        "{starts:#?}"
    );
    for start in &starts {
        assert_no_user_line(start);
        for request in start["control_requests"].as_array().unwrap() {
            let subtype = request["subtype"].as_str().unwrap();
            assert!(["initialize", "mcp_status"].contains(&subtype), "{start:#}");
        }
    }
}

#[test]
fn a_status_that_flips_is_told_with_its_account() {
    let (home, accounts) = map_home(true, &[]);
    list(&home, &json!({"refresh": true}));
    let since = seq(&home);
    accounts.set(2, &slack("connected"));
    let data = list(&home, &json!({"refresh": true, "account": 2}));
    assert_eq!(data["accounts"].as_array().unwrap().len(), 1, "{data:#}");
    assert_eq!(
        row(&data, 2)["connectors"][0]["status"],
        "connected",
        "{data:#}"
    );
    let events = connector_events(&home, since);
    assert_eq!(events.len(), 1, "{events:#?}");
    assert_eq!(events[0]["type"], "connector.changed", "{events:#?}");
    assert_eq!(
        events[0]["data"],
        json!({"account": 2, "id": SLACK, "status": "connected",
            "previous_status": "needs-auth"}),
        "{events:#?}"
    );
    assert_valid("connector-changed.json", &events[0]["data"]);
    // Only account 2 was read again.
    assert_eq!(starts(&home).len(), 3);
    // The same status again is no change.
    let since = seq(&home);
    list(&home, &json!({"refresh": true, "account": 2}));
    assert!(connector_events(&home, since).is_empty());
}

#[test]
fn the_map_survives_a_restart_and_keeps_its_homes_and_marks_to_itself() {
    let (home, accounts) = map_home(true, &[]);
    let since = seq(&home);
    let before = list(&home, &json!({"refresh": true}));
    let events = connector_events(&home, since);
    home.stop();
    home.start_dev();
    let after = list(&home, &json!({}));
    assert_eq!(after["accounts"], before["accounts"], "{after:#}");
    assert_eq!(after["model_turns"], 0);
    let file = home.root().join("rlxd").join("connectors.json");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{}", file.display());
    }
    // The account's homes and its mark are rlxd's own: never on the wire, in an event, or
    // in rlxd's log.
    let wire = format!("{before}{after}{}", Value::Array(events));
    let log = home.log();
    for secret in [
        MARK.to_owned(),
        accounts.managed.display().to_string(),
        accounts.managed_tmp.display().to_string(),
    ] {
        assert!(!wire.contains(&secret), "{secret} in {wire}");
        assert!(!log.contains(&secret), "{secret} in rlxd's log");
    }
}

#[test]
fn a_runs_init_that_finds_a_connector_signed_out_updates_the_map() {
    let (home, _accounts) = map_home(true, &[]);
    list(&home, &json!({"refresh": true}));
    let since = seq(&home);
    std::fs::write(home.root().join("scenario"), "connectors-needs-auth").unwrap();
    let id = run_id();
    start(
        &home,
        &json!({"kind": "widget_refresh", "prompt": "digest",
            "mount": {"kind": "connectors", "connectors": [SLACK]}, "space": "default"}),
        &id,
    );
    eventually("the run's init is taken", || {
        row(&list(&home, &json!({})), 1)["connectors"][0]["status"] == "needs-auth"
    });
    let events = connector_events(&home, since);
    let changed: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "connector.changed")
        .collect();
    assert_eq!(changed.len(), 1, "{events:#?}");
    assert_eq!(changed[0]["data"]["account"], 1, "{events:#?}");
    assert_eq!(changed[0]["data"]["status"], "needs-auth", "{events:#?}");
    // Account 2 is as it was read.
    let data = list(&home, &json!({}));
    assert_eq!(row(&data, 2)["from"], "listing", "{data:#}");
    assert_eq!(row(&data, 1)["from"], "init", "{data:#}");
}

#[test]
fn an_account_that_never_settles_keeps_what_was_read_and_says_so() {
    let (home, accounts) = map_home(true, &[("RLXD_TEST_CONNECTORS_SETTLE_MS", "1500")]);
    list(&home, &json!({"refresh": true}));
    let since = seq(&home);
    accounts.set(2, &slack("pending"));
    let refused = home.rlxd(&["call", "connectors.list", r#"{"refresh":true,"account":2}"#]);
    assert_eq!(refused.code(), Some(1), "{refused:?}");
    assert_eq!(refused.error_code(), "settle_timeout", "{refused:?}");
    let data = list(&home, &json!({}));
    let two = row(&data, 2);
    assert_eq!(two["connectors"][0]["status"], "needs-auth", "{data:#}");
    assert_eq!(two["error"]["code"], "settle_timeout", "{data:#}");
    assert!(connector_events(&home, since).is_empty());
    // A listing with no connector yet never settles either, and empties nothing.
    accounts.set(2, &json!([]));
    let refused = home.rlxd(&["call", "connectors.list", r#"{"refresh":true,"account":2}"#]);
    assert_eq!(refused.error_code(), "settle_timeout", "{refused:?}");
    assert_eq!(
        row(&list(&home, &json!({})), 2)["connectors"][0]["id"],
        SLACK
    );
    // A refresh of every account succeeds when one read did; the other keeps its error.
    let data = list(&home, &json!({"refresh": true}));
    assert_eq!(row(&data, 1)["error"], Value::Null, "{data:#}");
    assert_eq!(row(&data, 2)["error"]["code"], "settle_timeout", "{data:#}");
}

#[test]
fn a_refresh_of_an_account_being_read_joins_that_read() {
    let (home, accounts) = map_home(true, &[("RLXD_TEST_CONNECTORS_SETTLE_MS", "3000")]);
    accounts.set(2, &slack("pending"));
    let params = r#"{"refresh":true,"account":2}"#;
    let first = home.spawn(&["call", "connectors.list", params]);
    eventually("the first read starts", || starts(&home).len() == 1);
    let second = home.rlxd(&["call", "connectors.list", params]);
    let first = first.wait_with_output().unwrap();
    assert_eq!(second.error_code(), "settle_timeout", "{second:?}");
    assert_eq!(first.status.code(), Some(1));
    assert_eq!(starts(&home).len(), 1, "one read for both");
}

#[test]
fn a_claude_code_with_no_mcp_status_has_no_route_on_that_account() {
    let (home, _accounts) = map_home(true, &[]);
    std::fs::write(home.root().join("scenario"), "mcp-accounts-old").unwrap();
    let refused = home.rlxd(&["call", "connectors.list", r#"{"refresh":true}"#]);
    assert_eq!(refused.error_code(), "no_route", "{refused:?}");
    let data = list(&home, &json!({}));
    for slot in [1, 2] {
        assert_eq!(row(&data, slot)["error"]["code"], "no_route", "{data:#}");
    }
}

#[test]
fn an_unknown_account_is_refused() {
    let (home, _accounts) = map_home(true, &[]);
    for params in [r#"{"account":7}"#, r#"{"refresh":true,"account":7}"#] {
        let refused = home.rlxd(&["call", "connectors.list", params]);
        assert_eq!(refused.error_code(), "no_such_account", "{refused:?}");
    }
    let refused = home.rlxd(&["call", "connectors.list", r#"{"account":0}"#]);
    assert_eq!(refused.code(), Some(1), "{refused:?}");
    assert_eq!(starts(&home).len(), 0);
}

#[test]
fn the_background_reads_a_new_account_and_drops_one_gone_and_never_spends_a_turn() {
    let (home, accounts) = map_home(true, &[("RLXD_TEST_CONNECTOR_MAP_MS", "200,600000")]);
    // At its start: each account it had not read.
    eventually("both accounts are read unasked", || {
        let data = list(&home, &json!({}));
        data["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["from"] == "listing")
    });
    // An account gone from the brain's list: its connectors removed, with its slot.
    let since = seq(&home);
    let listed = home.root().join("accounts.json");
    std::fs::write(&listed, accounts.listed(false).to_string()).unwrap();
    eventually("account 2 is dropped", || {
        list(&home, &json!({}))["accounts"]
            .as_array()
            .unwrap()
            .len()
            == 1
    });
    let removed: Vec<Value> = connector_events(&home, since)
        .into_iter()
        .filter(|e| e["type"] == "connector.removed")
        .collect();
    assert_eq!(removed.len(), 1, "{removed:#?}");
    assert_eq!(removed[0]["data"], json!({"account": 2, "id": SLACK}));
    // Back, signed in as someone else: read again unasked.
    let reads = starts(&home).len();
    let mut back = accounts.listed(true);
    back[1]["login"] = json!("another-mark");
    accounts.set(2, &slack("connected"));
    std::fs::write(&listed, back.to_string()).unwrap();
    eventually("account 2 is read again", || {
        let data = list(&home, &json!({}));
        data["accounts"].as_array().unwrap().len() == 2
            && row(&data, 2)["connectors"][0]["status"] == "connected"
    });
    assert_eq!(starts(&home).len(), reads + 1);
    for start in &starts(&home) {
        assert_no_user_line(start);
    }
}

#[test]
fn the_background_reads_an_account_read_too_long_ago() {
    let (home, _accounts) = map_home(true, &[("RLXD_TEST_CONNECTOR_MAP_MS", "200,1000")]);
    eventually("both accounts are read unasked", || {
        starts(&home).len() >= 2
    });
    eventually("and read again once they are stale", || {
        starts(&home).len() >= 4
    });
}

#[test]
fn without_the_route_nothing_is_read_unasked_and_a_turn_reads_one_account() {
    let (home, accounts) = map_home(false, &[("RLXD_TEST_CONNECTOR_MAP_MS", "200,1000")]);
    let refused = home.rlxd(&["call", "connectors.list", r#"{"refresh":true}"#]);
    assert_eq!(refused.error_code(), "no_route", "{refused:?}");
    std::thread::sleep(Duration::from_millis(1000));
    assert_eq!(starts(&home).len(), 0, "the background spends no turn");
    // allow_turn with an account: that account's one-turn probe, on its own home.
    std::fs::write(home.root().join("scenario"), "init-connectors").unwrap();
    let data = list(
        &home,
        &json!({"refresh": true, "allow_turn": true, "account": 2}),
    );
    assert_eq!(data["model_turns"], 1, "{data:#}");
    assert_eq!(row(&data, 2)["from"], "init", "{data:#}");
    assert_eq!(row(&data, 2)["connectors"][0]["id"], SLACK, "{data:#}");
    let starts = starts(&home);
    assert_eq!(starts.len(), 1, "{starts:#?}");
    assert_eq!(
        starts[0]["env"]["CLAUDE_CONFIG_DIR"],
        accounts.managed.to_str().unwrap()
    );
    // Without one: the default account's alone, one turn.
    let data = list(&home, &json!({"refresh": true, "allow_turn": true}));
    assert_eq!(data["model_turns"], 1, "{data:#}");
    assert_eq!(row(&data, 1)["from"], "init", "{data:#}");
}
