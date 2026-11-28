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
use common::{TestHome, assert_valid, eventually, real_rlx};
use serde_json::{Value, json};

const SLACK: &str = "claude.ai Slack";

/// Slot 2's login mark, which never leaves rlxd's file.
const MARK: &str = "mark-of-slot-2";

/// Slot 1's: the default account is signed in too, as it normally is.
const MARK_1: &str = "mark-of-slot-1";

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
        let mut listed = vec![json!({"slot": 1, "label": "main", "login": MARK_1})];
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
    map_home_with(no_turn, envs, &slack("needs-auth"))
}

/// [`map_home`], slot 2's connectors being `rows` from its start.
fn map_home_with(no_turn: bool, envs: &[(&str, &str)], rows: &Value) -> (TestHome, Accounts) {
    map_home_in(TestHome::new(), no_turn, envs, rows)
}

/// [`map_home_with`] in `root`, a home not yet started (one with a brain of its own).
fn map_home_in(
    root: TestHome,
    no_turn: bool,
    envs: &[(&str, &str)],
    rows: &Value,
) -> (TestHome, Accounts) {
    let accounts = Accounts::of(root.root());
    accounts.set(1, &slack("connected"));
    accounts.set(2, rows);
    let listed = root.root().join("accounts.json");
    std::fs::write(&listed, accounts.listed(true).to_string()).unwrap();
    let listed = listed.to_str().unwrap().to_owned();
    // rlxd itself, and so its runs, on the plain account: no CLAUDE_CONFIG_DIR (its HOME
    // is the test's own).
    let mut all = vec![
        ("RLXD_TEST_ACCOUNTS", listed.as_str()),
        ("RLXD_TEST_CONNECTOR_MAP_MS", "off"),
        ("CLAUDE_CONFIG_DIR", ""),
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

/// rlxd's own file: each account's entry, by slot.
fn saved(home: &TestHome) -> Value {
    let text = std::fs::read_to_string(home.root().join("rlxd").join("connectors.json")).unwrap();
    serde_json::from_str::<Value>(&text).unwrap()["accounts"].clone()
}

/// Waits for the run `id` to end.
fn ended(home: &TestHome, id: &str) {
    eventually("the run ends", || {
        let runs = home.rlxd(&["call", "run.list"]).data();
        runs["runs"].as_array().unwrap().iter().any(|run| {
            run["run_id"] == id && ["done", "failed"].contains(&run["state"].as_str().unwrap_or(""))
        })
    });
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let file = home.root().join("rlxd").join("connectors.json");
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{}", file.display());
    }
    // The account's homes and its mark are rlxd's own: never on the wire, in an event, or
    // in rlxd's log.
    let wire = format!("{before}{after}{}", Value::Array(events));
    let log = home.log();
    for secret in [
        MARK.to_owned(),
        MARK_1.to_owned(),
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
    // Account 2 is as it was read, and slot 1 keeps the brain's mark for it.
    let data = list(&home, &json!({}));
    assert_eq!(row(&data, 2)["from"], "listing", "{data:#}");
    assert_eq!(row(&data, 1)["from"], "init", "{data:#}");
    assert_eq!(saved(&home)["1"]["login"], MARK_1);
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
    // Without one: the default account's alone, one turn, filed under the brain's own
    // entry for it, its mark kept.
    let data = list(&home, &json!({"refresh": true, "allow_turn": true}));
    assert_eq!(data["model_turns"], 1, "{data:#}");
    assert_eq!(row(&data, 1)["from"], "init", "{data:#}");
    assert_eq!(saved(&home)["1"]["login"], MARK_1);
}

#[test]
fn a_runs_init_files_under_the_brains_own_entry_and_an_empty_one_empties_nothing() {
    let (home, _accounts) = map_home(true, &[("RLXD_TEST_CONNECTOR_MAP_MS", "200,600000")]);
    eventually("both accounts are read unasked", || {
        let data = list(&home, &json!({}));
        data["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["from"] == "listing")
    });
    assert_eq!(saved(&home)["1"]["login"], MARK_1);
    let reads = starts(&home).len();
    let since = seq(&home);
    // A run whose init lists no server, as Claude Code's does with stdin at EOF.
    std::fs::write(home.root().join("scenario"), "hello").unwrap();
    let id = run_id();
    start(
        &home,
        &json!({"kind": "widget_refresh", "prompt": "digest",
            "mount": {"kind": "connectors", "connectors": [SLACK]}, "space": "default"}),
        &id,
    );
    ended(&home, &id);
    std::fs::write(home.root().join("scenario"), "mcp-accounts").unwrap();
    // Ticks go by: slot 1 is not "signed in again", so it is not read again.
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(starts(&home).len(), reads + 1, "the run's start only");
    let data = list(&home, &json!({}));
    assert_eq!(row(&data, 1)["connectors"][0]["id"], SLACK, "{data:#}");
    let removed: Vec<Value> = connector_events(&home, since)
        .into_iter()
        .filter(|e| e["type"] == "connector.removed")
        .collect();
    assert!(removed.is_empty(), "{removed:#?}");
    assert_eq!(saved(&home)["1"]["login"], MARK_1, "the mark is kept");
}

#[test]
fn a_failed_read_ages_as_one_that_succeeded() {
    // Slot 2's read fails at every try (signed out, it never settles).
    let (home, _accounts) = map_home_with(
        true,
        &[
            ("RLXD_TEST_CONNECTOR_MAP_MS", "200,600000"),
            ("RLXD_TEST_CONNECTORS_SETTLE_MS", "1200"),
        ],
        &slack("pending"),
    );
    let tried = || {
        let data = list(&home, &json!({}));
        row(&data, 1)["from"] == "listing" && row(&data, 2)["error"]["code"] == "settle_timeout"
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !tried() {
        assert!(
            std::time::Instant::now() < deadline,
            "never tried: {:#}\n{:#?}",
            list(&home, &json!({})),
            starts(&home)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let tried = starts(&home).len();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        starts(&home).len(),
        tried,
        "no read again before the day is out"
    );
}

#[test]
fn the_brains_accounts_reach_rlxd_with_their_homes_and_marks() {
    let Some(rlx) = real_rlx() else { return };
    let root = TestHome::new().with_rlx(rlx);
    // Slot 2, a managed slot as rlx makes one, signed in.
    let slot = root.root().join("claude-accounts").join("2");
    std::fs::create_dir_all(&slot).unwrap();
    std::fs::write(
        slot.join(".aisquare-account.json"),
        r#"{"slot": 2, "created_at": "2026-11-28T00:00:00+00:00"}"#,
    )
    .unwrap();
    let uuid = "0e9d6c1a-uuid-of-the-fixture";
    std::fs::write(
        slot.join(".claude.json"),
        json!({"oauthAccount": {"emailAddress": "fixture@example.com", "accountUuid": uuid}})
            .to_string(),
    )
    .unwrap();
    std::fs::write(
        slot.join("fake-connectors.json"),
        slack("needs-auth").to_string(),
    )
    .unwrap();
    let plain = root.root().join("user-home").join(".claude");
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::write(
        plain.join("fake-connectors.json"),
        slack("connected").to_string(),
    )
    .unwrap();
    let home = home_with_scenario_file_in(
        root,
        "mcp-accounts",
        &[
            ("RLXD_TEST_CONNECTORS_NO_TURN", "1"),
            ("RLXD_TEST_CONNECTOR_MAP_MS", "off"),
        ],
    );
    // The brain starts, then lists slot 2.
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    loop {
        let run = home.rlxd(&["call", "connectors.list", r#"{"account":2}"#]);
        if run.code() == Some(0) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the brain never listed slot 2: {run:?}"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
    let data = list(&home, &json!({"refresh": true}));
    assert_eq!(
        row(&data, 1)["connectors"][0]["status"],
        "connected",
        "{data:#}"
    );
    assert_eq!(
        row(&data, 2)["connectors"][0]["status"],
        "needs-auth",
        "{data:#}"
    );
    // Slot 2's read ran on its own two homes, as the brain named them.
    let starts = starts(&home);
    let managed = starts
        .iter()
        .find(|start| start["env"].get("CLAUDE_CONFIG_DIR").is_some())
        .unwrap_or_else(|| panic!("{starts:#?}"));
    let named = PathBuf::from(managed["env"]["CLAUDE_CONFIG_DIR"].as_str().unwrap());
    assert_eq!(
        std::fs::canonicalize(&named).unwrap(),
        std::fs::canonicalize(&slot).unwrap()
    );
    let tmp = managed["env"]["CLAUDE_CODE_TMPDIR"].as_str().unwrap();
    assert!(
        Path::new(tmp).ends_with(Path::new("cache").join("claude-accounts").join("2")),
        "{tmp}"
    );
    // Its mark is the SHA-256 of its account id, in rlxd's file and nowhere else.
    use sha2::{Digest, Sha256};
    let mark: String = Sha256::digest(uuid.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(saved(&home)["2"]["login"], mark.as_str());
    let wire = data.to_string();
    for secret in [mark.as_str(), uuid, "fixture@example.com"] {
        assert!(!wire.contains(secret), "{secret} in {wire}");
        assert!(!home.log().contains(secret), "{secret} in rlxd's log");
    }
}

#[test]
fn a_read_folder_holding_settings_rlxd_did_not_write_is_not_read() {
    let (home, _accounts) = map_home(true, &[]);
    let planted = home
        .root()
        .join("rlxd")
        .join("connectors-read")
        .join("2")
        .join(".claude");
    std::fs::create_dir_all(&planted).unwrap();
    std::fs::write(planted.join("settings.json"), r#"{"hooks": {}}"#).unwrap();
    let refused = home.rlxd(&["call", "connectors.list", r#"{"refresh":true,"account":2}"#]);
    assert_eq!(refused.error_code(), "untrusted_settings", "{refused:?}");
    assert_eq!(starts(&home).len(), 0, "no Claude Code started on it");
    let data = list(&home, &json!({}));
    assert_eq!(
        row(&data, 2)["error"]["code"],
        "untrusted_settings",
        "{data:#}"
    );
}

#[test]
fn under_a_managed_slots_home_runs_file_there_with_the_route_off() {
    // rlxd itself runs on slot 2's two variables, and reads nothing with no turn.
    let root = TestHome::new();
    let accounts = Accounts::of(root.root());
    std::fs::create_dir_all(&accounts.managed).unwrap();
    std::fs::create_dir_all(&accounts.managed_tmp).unwrap();
    let listed = root.root().join("accounts.json");
    std::fs::write(&listed, accounts.listed(true).to_string()).unwrap();
    let (listed, dir, tmp) = (
        listed.to_str().unwrap().to_owned(),
        accounts.managed.to_str().unwrap().to_owned(),
        accounts.managed_tmp.to_str().unwrap().to_owned(),
    );
    let home = home_with_scenario_file_in(
        root,
        "init-connectors",
        &[
            ("RLXD_TEST_ACCOUNTS", listed.as_str()),
            ("CLAUDE_CONFIG_DIR", dir.as_str()),
            ("CLAUDE_CODE_TMPDIR", tmp.as_str()),
        ],
    );
    let id = run_id();
    start(
        &home,
        &json!({"kind": "widget_refresh", "prompt": "digest",
            "mount": {"kind": "connectors", "connectors": [SLACK]}, "space": "default"}),
        &id,
    );
    ended(&home, &id);
    // With no connectors.list that asks the brain: rlxd asked at its start.
    eventually("the run's connectors are slot 2's", || {
        row(&list(&home, &json!({})), 2)["connectors"]
            .as_array()
            .is_some_and(|c| !c.is_empty())
    });
    let data = list(&home, &json!({}));
    assert_eq!(row(&data, 1)["connectors"], json!([]), "{data:#}");
    assert_eq!(
        data["connectors"],
        json!([]),
        "the top level is the plain account's"
    );
    assert_eq!(
        saved(&home)["2"]["login"],
        MARK,
        "filed under the brain's own entry"
    );
}

#[test]
fn under_a_home_that_is_no_account_a_runs_init_is_filed_nowhere() {
    let root = TestHome::new();
    let accounts = Accounts::of(root.root());
    let listed = root.root().join("accounts.json");
    std::fs::write(&listed, accounts.listed(true).to_string()).unwrap();
    let listed = listed.to_str().unwrap().to_owned();
    // home_in's own CLAUDE_CONFIG_DIR, a folder that is neither slot.
    let home = home_with_scenario_file_in(
        root,
        "init-connectors",
        &[("RLXD_TEST_ACCOUNTS", listed.as_str())],
    );
    let since = seq(&home);
    let id = run_id();
    start(
        &home,
        &json!({"kind": "widget_refresh", "prompt": "digest",
            "mount": {"kind": "connectors", "connectors": [SLACK]}, "space": "default"}),
        &id,
    );
    ended(&home, &id);
    std::thread::sleep(Duration::from_millis(500));
    let data = list(&home, &json!({}));
    for slot in [1, 2] {
        assert_eq!(row(&data, slot)["connectors"], json!([]), "{data:#}");
    }
    assert!(connector_events(&home, since).is_empty());
}

#[test]
fn after_a_restart_with_no_brain_a_managed_read_takes_both_homes_from_its_record() {
    let (mut home, accounts) = map_home(true, &[]);
    list(&home, &json!({"refresh": true}));
    home.stop();
    // The brain gone, and rlxd with a CLAUDE_CODE_TMPDIR of its own.
    let own_tmp = home.root().join("rlxd-own-tmp");
    std::fs::create_dir_all(&own_tmp).unwrap();
    let gone = home.root().join("no-accounts.json");
    home.set_env("RLXD_TEST_ACCOUNTS", gone.to_str().unwrap());
    home.set_env("CLAUDE_CODE_TMPDIR", own_tmp.to_str().unwrap());
    home.start_dev();
    let reads = starts(&home).len();
    list(&home, &json!({"refresh": true, "account": 2}));
    let starts = starts(&home);
    assert_eq!(starts.len(), reads + 1, "{starts:#?}");
    let env = &starts.last().unwrap()["env"];
    assert_eq!(env["CLAUDE_CONFIG_DIR"], accounts.managed.to_str().unwrap());
    assert_eq!(
        env["CLAUDE_CODE_TMPDIR"],
        accounts.managed_tmp.to_str().unwrap()
    );
}

#[test]
fn a_managed_account_whose_tmp_dir_is_not_known_is_not_read() {
    let root = TestHome::new();
    let accounts = Accounts::of(root.root());
    accounts.set(2, &slack("connected"));
    // A file from before tmp dirs were kept, and no brain to say slot 2's.
    std::fs::create_dir_all(root.root().join("rlxd")).unwrap();
    std::fs::write(
        root.root().join("rlxd").join("connectors.json"),
        json!({"accounts": {"2": {"config_dir": accounts.managed, "login": MARK,
            "as_of": null, "connectors": []}}})
        .to_string(),
    )
    .unwrap();
    let gone = root.root().join("no-accounts.json");
    let gone = gone.to_str().unwrap().to_owned();
    let home = home_with_scenario_file_in(
        root,
        "mcp-accounts",
        &[
            ("RLXD_TEST_ACCOUNTS", gone.as_str()),
            ("RLXD_TEST_CONNECTOR_MAP_MS", "off"),
            ("RLXD_TEST_CONNECTORS_NO_TURN", "1"),
            ("CLAUDE_CONFIG_DIR", ""),
        ],
    );
    let refused = home.rlxd(&["call", "connectors.list", r#"{"refresh":true,"account":2}"#]);
    assert_eq!(refused.error_code(), "account_unknown", "{refused:?}");
    assert_eq!(starts(&home).len(), 0);
    let data = list(&home, &json!({}));
    assert_eq!(
        row(&data, 2)["error"]["code"],
        "account_unknown",
        "{data:#}"
    );
}

#[test]
fn once_the_brain_has_listed_slot_1_alone_an_unknown_homes_init_never_flips_it() {
    let root = TestHome::new();
    let accounts = Accounts::of(root.root());
    accounts.set(1, &slack("connected"));
    // The brain lists the plain account alone, signed in.
    let listed = root.root().join("accounts.json");
    std::fs::write(&listed, json!([{"slot": 1, "login": MARK_1}]).to_string()).unwrap();
    let listed = listed.to_str().unwrap().to_owned();
    // rlxd under home_in's CLAUDE_CONFIG_DIR: a home that is no account's.
    let mut home = home_with_scenario_file_in(
        root,
        "mcp-accounts",
        &[
            ("RLXD_TEST_ACCOUNTS", listed.as_str()),
            ("RLXD_TEST_CONNECTOR_MAP_MS", "off"),
            ("RLXD_TEST_CONNECTORS_NO_TURN", "1"),
        ],
    );
    let read = list(&home, &json!({"refresh": true}));
    assert_eq!(
        row(&read, 1)["connectors"][0]["status"],
        "connected",
        "{read:#}"
    );
    let run_signed_out = |home: &TestHome| {
        std::fs::write(home.root().join("scenario"), "connectors-needs-auth").unwrap();
        let id = run_id();
        start(
            home,
            &json!({"kind": "widget_refresh", "prompt": "digest",
                "mount": {"kind": "connectors", "connectors": [SLACK]}, "space": "default"}),
            &id,
        );
        ended(home, &id);
        std::thread::sleep(Duration::from_millis(500));
        std::fs::write(home.root().join("scenario"), "mcp-accounts").unwrap();
    };
    let since = seq(&home);
    run_signed_out(&home);
    let data = list(&home, &json!({}));
    assert_eq!(
        row(&data, 1)["connectors"][0]["status"],
        "connected",
        "{data:#}"
    );
    assert!(connector_events(&home, since).is_empty());
    // And after a restart with the brain gone: its answer was kept, so still nowhere.
    home.stop();
    let gone = home.root().join("no-accounts.json");
    home.set_env("RLXD_TEST_ACCOUNTS", gone.to_str().unwrap());
    home.start_dev();
    let since = seq(&home);
    run_signed_out(&home);
    let data = list(&home, &json!({}));
    assert_eq!(
        row(&data, 1)["connectors"][0]["status"],
        "connected",
        "{data:#}"
    );
    assert!(connector_events(&home, since).is_empty());
    assert_eq!(saved(&home)["1"]["login"], MARK_1);
}

// --- 4.3: the app's environment ---------------------------------------------------------

/// rlxd's own `CLAUDE_SECURESTORAGE_CONFIG_DIR`, which no account's read may take.
const RLXD_SECURE: (&str, &str) = ("CLAUDE_SECURESTORAGE_CONFIG_DIR", "/rlxd/own-secure");

/// `env.set` from the app of a `CLAUDE_CONFIG_DIR` of its own, a
/// `CLAUDE_SECURESTORAGE_CONFIG_DIR` and a `PATH`: the values a run's Claude Code gets.
/// Their `(CLAUDE_CONFIG_DIR, PATH)`.
fn app_sets_its_own_home(home: &TestHome) -> (String, String) {
    let config = home.root().join("cc-app");
    std::fs::create_dir_all(&config).unwrap();
    let mut dirs = vec![home.root().join("app-bin")];
    dirs.extend(home.path_without_real_claude());
    let path = std::env::join_paths(dirs).unwrap();
    let (config, path) = (
        config.to_str().unwrap().to_owned(),
        path.to_str().unwrap().to_owned(),
    );
    let secure = home.root().join("app-secure").to_str().unwrap().to_owned();
    let change = json!({"CLAUDE_CONFIG_DIR": config, "CLAUDE_SECURESTORAGE_CONFIG_DIR": secure,
        "PATH": path});
    let set = home.rlxd(&["call", "--as-app", "env.set", &change.to_string()]);
    assert_eq!(set.code(), Some(0), "{set:?}");
    (config, path)
}

/// Each start's environment holds the app's `PATH`, and its account's homes alone: the
/// default account's neither `CLAUDE_CONFIG_DIR` nor `CLAUDE_CODE_TMPDIR`, though the app
/// set a `CLAUDE_CONFIG_DIR`; a managed one's its own two. And no account's holds a
/// `CLAUDE_SECURESTORAGE_CONFIG_DIR`, the app's or rlxd's: it picks whose Keychain login
/// Claude Code reads.
fn assert_each_on_its_account(start: &Value, slot: u32, accounts: &Accounts, path: &str) {
    let env = &start["env"];
    assert_eq!(env["PATH"], path, "the app's PATH: {start:#}");
    assert_eq!(
        env.get("CLAUDE_SECURESTORAGE_CONFIG_DIR"),
        None,
        "{start:#}"
    );
    if slot == 1 {
        assert_eq!(env.get("CLAUDE_CONFIG_DIR"), None, "{start:#}");
        assert_eq!(env.get("CLAUDE_CODE_TMPDIR"), None, "{start:#}");
    } else {
        assert_eq!(
            env["CLAUDE_CONFIG_DIR"],
            accounts.managed.to_str().unwrap(),
            "{start:#}"
        );
        assert_eq!(
            env["CLAUDE_CODE_TMPDIR"],
            accounts.managed_tmp.to_str().unwrap(),
            "{start:#}"
        );
    }
}

#[test]
fn with_the_apps_environment_each_read_keeps_to_its_accounts_homes() {
    let (home, accounts) = map_home(true, &[RLXD_SECURE]);
    let (config, path) = app_sets_its_own_home(&home);
    let data = list(&home, &json!({"refresh": true}));
    assert_eq!(data["model_turns"], 0, "{data:#}");
    assert_eq!(row(&data, 1)["connectors"][0]["status"], "connected");
    assert_eq!(row(&data, 2)["connectors"][0]["status"], "needs-auth");
    let starts = starts(&home);
    assert_eq!(starts.len(), 2, "{starts:#?}");
    assert_each_on_its_account(&starts[0], 1, &accounts, &path);
    assert_each_on_its_account(&starts[1], 2, &accounts, &path);
    for start in &starts {
        assert_ne!(
            start["env"]["CLAUDE_CONFIG_DIR"],
            config.as_str(),
            "{start:#}"
        );
    }
}

#[test]
fn with_the_apps_environment_each_one_turn_probe_keeps_to_its_accounts_homes() {
    let (home, accounts) = map_home(false, &[RLXD_SECURE]);
    let (_, path) = app_sets_its_own_home(&home);
    std::fs::write(home.root().join("scenario"), "init-connectors").unwrap();
    for slot in [1, 2] {
        let data = list(
            &home,
            &json!({"refresh": true, "allow_turn": true, "account": slot}),
        );
        assert_eq!(data["model_turns"], 1, "{data:#}");
        let starts = starts(&home);
        assert_eq!(starts.len(), slot as usize, "{starts:#?}");
        assert_each_on_its_account(starts.last().unwrap(), slot, &accounts, &path);
    }
}

#[test]
fn a_runs_init_is_filed_under_the_account_whose_home_the_app_set() {
    // rlxd itself on the plain account (no CLAUDE_CONFIG_DIR); the route off, so nothing
    // is read but what runs find.
    let (home, accounts) = map_home(false, &[]);
    let widget = json!({"kind": "widget_refresh", "prompt": "digest",
        "mount": {"kind": "connectors", "connectors": [SLACK]}, "space": "default"});
    let set = |change: Value| {
        let run = home.rlxd(&["call", "--as-app", "env.set", &change.to_string()]);
        assert_eq!(run.code(), Some(0), "{run:?}");
    };
    let status_of =
        |slot: u32| row(&list(&home, &json!({})), slot)["connectors"][0]["status"].clone();

    // The app's CLAUDE_CONFIG_DIR is slot 2's home: runs run there, and file there.
    set(json!({"CLAUDE_CONFIG_DIR": accounts.managed.to_str().unwrap()}));
    std::fs::write(home.root().join("scenario"), "init-connectors").unwrap();
    let id = run_id();
    start(&home, &widget, &id);
    ended(&home, &id);
    eventually("the run's connectors are slot 2's", || {
        status_of(2) == "connected"
    });
    let data = list(&home, &json!({}));
    assert_eq!(row(&data, 2)["from"], "init", "{data:#}");
    assert_eq!(row(&data, 1)["connectors"], json!([]), "{data:#}");
    assert_eq!(saved(&home)["2"]["login"], MARK, "the brain's own entry");
    let started = starts(&home);
    assert_eq!(
        started.last().unwrap()["env"]["CLAUDE_CONFIG_DIR"],
        accounts.managed.to_str().unwrap(),
        "{started:#?}"
    );

    // Taken back: runs are the plain account's again, and file there.
    set(json!({"CLAUDE_CONFIG_DIR": ""}));
    std::fs::write(home.root().join("scenario"), "connectors-needs-auth").unwrap();
    let id = run_id();
    start(&home, &widget, &id);
    ended(&home, &id);
    eventually("the run's connectors are slot 1's", || {
        status_of(1) == "needs-auth"
    });
    let data = list(&home, &json!({}));
    assert_eq!(row(&data, 1)["from"], "init", "{data:#}");
    assert_eq!(
        status_of(2),
        "connected",
        "slot 2 as the first run left it: {data:#}"
    );
    // rlxd's own, which map_home leaves empty: no home named.
    let started = starts(&home);
    let named = started.last().unwrap()["env"]["CLAUDE_CONFIG_DIR"].clone();
    assert!(named.is_null() || named == "", "{started:#?}");
}

/// A connectors widget run, which reads Slack.
fn widget() -> Value {
    json!({"kind": "widget_refresh", "prompt": "digest",
        "mount": {"kind": "connectors", "connectors": [SLACK]}, "space": "default"})
}

/// `env.set` from the app, which must succeed.
fn app_sets(home: &TestHome, change: &Value) {
    let run = home.rlxd(&["call", "--as-app", "env.set", &change.to_string()]);
    assert_eq!(run.code(), Some(0), "{run:?}");
}

/// Slack's status in `slot`'s connectors now, or null.
fn slack_status(home: &TestHome, slot: u32) -> Value {
    row(&list(home, &json!({})), slot)["connectors"][0]["status"].clone()
}

#[test]
fn a_runs_init_is_filed_by_the_home_its_attempt_ran_on_though_an_env_set_changed_it_since() {
    // #4167 F1: the app's home is slot 2's when the run starts, and the plain one's by
    // the time its init (10 s in) comes. It is slot 2's init.
    let (home, accounts) = map_home(false, &[]);
    app_sets(
        &home,
        &json!({"CLAUDE_CONFIG_DIR": accounts.managed.to_str().unwrap()}),
    );
    std::fs::write(home.root().join("scenario"), "connectors-slow-init").unwrap();
    let since = seq(&home);
    let id = run_id();
    start(&home, &widget(), &id);
    eventually("the run's Claude Code is started", || {
        starts(&home).len() == 1
    });
    assert_eq!(
        starts(&home)[0]["env"]["CLAUDE_CONFIG_DIR"],
        accounts.managed.to_str().unwrap()
    );
    app_sets(&home, &json!({"CLAUDE_CONFIG_DIR": ""}));
    // Its init comes 10 s in, past `ended`'s wait.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while slack_status(&home, 2) != "connected" {
        assert!(
            std::time::Instant::now() < deadline,
            "never: the init is slot 2's"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    ended(&home, &id);
    let data = list(&home, &json!({}));
    assert_eq!(row(&data, 2)["from"], "init", "{data:#}");
    assert_eq!(row(&data, 1)["connectors"], json!([]), "{data:#}");
    let events = connector_events(&home, since);
    assert!(
        events.iter().all(|e| e["data"]["account"] == 2),
        "nothing of slot 1's: {events:#?}"
    );
}

#[test]
fn an_init_held_for_the_brain_is_filed_by_its_own_home_and_a_later_runs_by_theirs() {
    // #4167 F2 and M9. rlxd learns slot 2 from RLXD_TEST_ACCOUNTS, with the app's home
    // slot 2's, and a run files slot 2's Slack connected.
    let brain = TestHome::new().with_rlx(common::fake_brain("print-env"));
    let (mut home, accounts) = map_home_in(brain, false, &[], &slack("needs-auth"));
    app_sets(
        &home,
        &json!({"CLAUDE_CONFIG_DIR": accounts.managed.to_str().unwrap()}),
    );
    std::fs::write(home.root().join("scenario"), "init-connectors").unwrap();
    let id = run_id();
    start(&home, &widget(), &id);
    ended(&home, &id);
    eventually("slot 2's Slack is connected", || {
        slack_status(&home, 2) == "connected"
    });

    // Restarted with the real brain path: its `accounts` is accepted and then held, so
    // rlx hasn't said which accounts there are. The app's home is still slot 2's.
    home.stop();
    home.remove_env("RLXD_TEST_ACCOUNTS");
    let (hold, busy) = (
        home.root().join("fake-brain-hold"),
        home.root().join("fake-brain-busy"),
    );
    std::fs::write(&hold, "").unwrap();
    let _ = std::fs::remove_file(&busy);
    home.start_dev();
    eventually("the brain holds the accounts ask", || busy.exists());

    // A run on slot 2's home: its init (Slack signed out) waits for the brain.
    std::fs::write(home.root().join("scenario"), "connectors-needs-auth").unwrap();
    let held = run_id();
    start(&home, &widget(), &held);
    ended(&home, &held);
    assert_eq!(slack_status(&home, 2), "connected", "held, not filed yet");

    // The app's home changes while the ask is pending (M9), and a run on the plain home
    // files under slot 1 at once (F2), never under slot 2 by a stale home.
    app_sets(&home, &json!({"CLAUDE_CONFIG_DIR": ""}));
    std::fs::write(home.root().join("scenario"), "init-connectors").unwrap();
    let plain = run_id();
    start(&home, &widget(), &plain);
    ended(&home, &plain);
    eventually("the plain run's init is slot 1's", || {
        slack_status(&home, 1) == "connected"
    });
    assert_eq!(
        slack_status(&home, 2),
        "connected",
        "{:#}",
        list(&home, &json!({}))
    );

    // The brain answers with no accounts list: they stay as they were, and the held init
    // is filed by the home it ran on, slot 2's, though the app's is the plain one now.
    std::fs::remove_file(&hold).unwrap();
    eventually("the held init is slot 2's", || {
        slack_status(&home, 2) == "needs-auth"
    });
    assert_eq!(
        slack_status(&home, 1),
        "connected",
        "{:#}",
        list(&home, &json!({}))
    );
}

/// `connectors.login {id: Slack}`, started: its `login_id`.
fn login(home: &TestHome) -> String {
    let run = home.rlxd(&[
        "call",
        "connectors.login",
        &json!({"id": SLACK}).to_string(),
    ]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    run.data()["login_id"].as_str().unwrap().to_owned()
}

/// The `connector` events after `since`, until the first login's end.
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
}

/// The `needs_you` events after `since` of `type`.
fn needs_you_events(home: &TestHome, since: u64, r#type: &str) -> Vec<Value> {
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
    .filter(|e| e["type"] == r#type)
    .collect()
}

/// A home whose two accounts have read Slack `slot_1` and `slot_2`, with no turn, and
/// `envs` set too.
fn read_home(slot_1: &str, slot_2: &str, envs: &[(&str, &str)]) -> (TestHome, Accounts) {
    let (home, accounts) = map_home_with(true, envs, &slack(slot_2));
    accounts.set(1, &slack(slot_1));
    list(&home, &json!({"refresh": true}));
    assert_eq!(slack_status(&home, 1), slot_1);
    assert_eq!(slack_status(&home, 2), slot_2);
    (home, accounts)
}

/// Waits until fake-claude's log holds `count` starts: the latest's.
fn started(home: &TestHome, count: usize) -> Value {
    eventually("the start is logged", || starts(home).len() >= count);
    starts(home)[count - 1].clone()
}

#[test]
fn a_logins_sign_in_is_filed_under_the_account_whose_home_it_ran_on() {
    // #4182 L: the login starts on slot 2's home, and the app's home is the plain one's
    // by the time it signs in. The sign-in is slot 2's; slot 1's Slack stays signed out.
    let (home, accounts) = read_home("needs-auth", "needs-auth", &[]);
    let reads = starts(&home).len();
    app_sets(
        &home,
        &json!({"CLAUDE_CONFIG_DIR": accounts.managed.to_str().unwrap()}),
    );
    std::fs::write(home.root().join("scenario"), "mcp-login-slow").unwrap();
    let since = seq(&home);
    login(&home);
    let start = started(&home, reads + 1);
    assert_eq!(start["argv"][1], "mcp", "{start:#}");
    assert_eq!(
        start["env"]["CLAUDE_CONFIG_DIR"],
        accounts.managed.to_str().unwrap()
    );
    app_sets(&home, &json!({"CLAUDE_CONFIG_DIR": ""}));
    let events = login_events(&home, since);
    assert_eq!(
        events.last().unwrap()["type"],
        "connector.login.done",
        "{events:#?}"
    );
    assert_eq!(slack_status(&home, 2), "connected");
    assert_eq!(slack_status(&home, 1), "needs-auth");
    // The change is published after the login's end: the replay holds it.
    let changed = || -> Vec<Value> {
        connector_events(&home, since)
            .into_iter()
            .filter(|e| e["type"] == "connector.changed")
            .collect()
    };
    eventually("the change is published", || !changed().is_empty());
    let changed = changed();
    assert_eq!(changed.len(), 1, "{changed:#?}");
    assert_eq!(changed[0]["data"]["account"], 2, "{changed:#?}");
}

#[test]
fn a_calls_verdict_is_filed_under_the_account_whose_home_it_ran_on() {
    // The call twin: the call starts on slot 2's home and finds Slack signed out 3 s in,
    // after the app's home went back to the plain one's. Slot 2's Slack is signed out.
    let (home, accounts) = read_home(
        "connected",
        "connected",
        &[("RLXD_TEST_CONNECTORS_CALL", "1")],
    );
    let reads = starts(&home).len();
    app_sets(
        &home,
        &json!({"CLAUDE_CONFIG_DIR": accounts.managed.to_str().unwrap()}),
    );
    std::fs::write(home.root().join("scenario"), "mcp-direct-needs-auth-slow").unwrap();
    let params = json!({"run_id": run_id(), "connector": SLACK,
        "tool": "mcp__claude_ai_Slack__slack_search_public", "input": {"query": "probe"}});
    let call = home.spawn(&["call", "connectors.call", &params.to_string()]);
    let start = started(&home, reads + 1);
    assert_eq!(
        start["env"]["CLAUDE_CONFIG_DIR"],
        accounts.managed.to_str().unwrap()
    );
    app_sets(&home, &json!({"CLAUDE_CONFIG_DIR": ""}));
    let run = common::finish(call);
    assert_eq!(run.error_code(), "call_failed", "{run:?}");
    assert_eq!(slack_status(&home, 2), "needs-auth");
    assert_eq!(slack_status(&home, 1), "connected");
}

#[test]
fn a_sign_in_clears_only_the_needs_you_of_runs_on_its_account() {
    // A run on the plain home and one on slot 2's both wait for Slack's sign-in. A login
    // on slot 2's home clears the second's needs-you alone.
    let (home, accounts) = read_home("needs-auth", "needs-auth", &[]);
    let needs = json!({"kind": "widget_refresh", "prompt": "digest",
        "mount": {"kind": "connectors", "connectors": [SLACK]},
        "needs_connectors": [SLACK], "space": "default"});
    std::fs::write(home.root().join("scenario"), "connectors-needs-auth").unwrap();
    let since = seq(&home);
    let plain = run_id();
    start(&home, &needs, &plain);
    ended(&home, &plain);
    app_sets(
        &home,
        &json!({"CLAUDE_CONFIG_DIR": accounts.managed.to_str().unwrap()}),
    );
    let managed = run_id();
    start(&home, &needs, &managed);
    ended(&home, &managed);
    let raised = needs_you_events(&home, since, "needs_you.raised");
    let id_of = |run: &str| {
        raised
            .iter()
            .find(|e| e["data"]["run_id"] == run && e["data"]["reason"] == "auth")
            .unwrap_or_else(|| panic!("{run}'s needs-you: {raised:#?}"))["data"]["id"]
            .clone()
    };
    let (plain_needs, managed_needs) = (id_of(&plain), id_of(&managed));

    std::fs::write(home.root().join("scenario"), "mcp-login").unwrap();
    let signed = seq(&home);
    login(&home);
    let events = login_events(&home, signed);
    assert_eq!(
        events.last().unwrap()["type"],
        "connector.login.done",
        "{events:#?}"
    );
    eventually("slot 2's run's needs-you is cleared", || {
        !needs_you_events(&home, signed, "needs_you.cleared").is_empty()
    });
    std::thread::sleep(Duration::from_millis(500));
    let cleared = needs_you_events(&home, signed, "needs_you.cleared");
    assert_eq!(cleared.len(), 1, "{cleared:#?}");
    assert_eq!(cleared[0]["data"]["id"], managed_needs, "{cleared:#?}");
    assert_ne!(cleared[0]["data"]["id"], plain_needs);
    // The plain run's is still out, in its record too.
    let record = home
        .root()
        .join("rlxd")
        .join("runs")
        .join(&plain)
        .join("run.json");
    let record: Value = serde_json::from_slice(&std::fs::read(record).unwrap()).unwrap();
    assert_eq!(record["auth"], json!([SLACK]), "{record:#}");
}

#[test]
fn a_retried_runs_init_is_filed_by_the_home_its_attempt_ran_on() {
    // #4182 N3, the gated-retry path: three tries on slot 2's home, and the app's home is
    // the plain one's from the first try on. The init is slot 2's.
    let (home, accounts) = map_home(false, &[]);
    app_sets(
        &home,
        &json!({"CLAUDE_CONFIG_DIR": accounts.managed.to_str().unwrap()}),
    );
    std::fs::write(home.root().join("scenario"), "connectors-never").unwrap();
    let needs = json!({"kind": "widget_refresh", "prompt": "digest",
        "mount": {"kind": "connectors", "connectors": [SLACK]},
        "needs_connectors": [SLACK], "space": "default"});
    let since = seq(&home);
    let id = run_id();
    start(&home, &needs, &id);
    let first = started(&home, 1);
    assert_eq!(
        first["env"]["CLAUDE_CONFIG_DIR"],
        accounts.managed.to_str().unwrap()
    );
    app_sets(&home, &json!({"CLAUDE_CONFIG_DIR": ""}));
    // Three tries, 2 s and 5 s apart, then the end.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while slack_status(&home, 2).is_null() {
        assert!(
            std::time::Instant::now() < deadline,
            "never: the init is filed"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    ended(&home, &id);
    let starts = starts(&home);
    assert_eq!(starts.len(), 3, "{starts:#?}");
    for start in &starts {
        assert_eq!(
            start["env"]["CLAUDE_CONFIG_DIR"],
            accounts.managed.to_str().unwrap()
        );
    }
    let data = list(&home, &json!({}));
    assert_eq!(row(&data, 2)["from"], "init", "{data:#}");
    assert_eq!(row(&data, 1)["connectors"], json!([]), "{data:#}");
    let events = connector_events(&home, since);
    assert!(
        events.iter().all(|e| e["data"]["account"] == 2),
        "{events:#?}"
    );
}

/// A widget run that needs Slack: it fails needing you to sign Slack in again when Slack
/// is signed out.
fn needs_slack() -> Value {
    json!({"kind": "widget_refresh", "prompt": "digest",
        "mount": {"kind": "connectors", "connectors": [SLACK]},
        "needs_connectors": [SLACK], "space": "default"})
}

/// Starts a run of `params` on `scenario` and waits for its end: its `needs_you {auth}`
/// id.
fn auth_needs_you(home: &TestHome, scenario: &str) -> Value {
    std::fs::write(home.root().join("scenario"), scenario).unwrap();
    let since = seq(home);
    let id = run_id();
    start(home, &needs_slack(), &id);
    ended(home, &id);
    let raised = needs_you_events(home, since, "needs_you.raised");
    raised
        .iter()
        .find(|e| e["data"]["run_id"] == id.as_str() && e["data"]["reason"] == "auth")
        .unwrap_or_else(|| panic!("{id}'s needs-you: {raised:#?}"))["data"]["id"]
        .clone()
}

/// A login of Slack on `scenario`, run to its end, as the app's values are now.
fn login_to_end(home: &TestHome) {
    std::fs::write(home.root().join("scenario"), "mcp-login").unwrap();
    let since = seq(home);
    login(home);
    let events = login_events(home, since);
    assert_eq!(
        events.last().unwrap()["type"],
        "connector.login.done",
        "{events:#?}"
    );
}

#[test]
fn a_runs_needs_you_waits_for_its_own_accounts_sign_in_across_a_restart() {
    // #4216 Q4/Q5: a run on slot 2's home needs Slack signed in again. Its home is kept
    // in its record: after a restart, a sign-in on the plain home leaves it out, and one
    // on slot 2's home clears it.
    let (home, accounts) = read_home("needs-auth", "needs-auth", &[]);
    app_sets(
        &home,
        &json!({"CLAUDE_CONFIG_DIR": accounts.managed.to_str().unwrap()}),
    );
    let needs = auth_needs_you(&home, "connectors-needs-auth");
    home.stop();
    home.start_dev();
    let since = seq(&home);
    app_sets(&home, &json!({"CLAUDE_CONFIG_DIR": ""}));
    login_to_end(&home);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(slack_status(&home, 1), "connected");
    assert!(
        needs_you_events(&home, since, "needs_you.cleared").is_empty(),
        "a plain sign-in clears none of slot 2's"
    );
    app_sets(
        &home,
        &json!({"CLAUDE_CONFIG_DIR": accounts.managed.to_str().unwrap()}),
    );
    login_to_end(&home);
    eventually("slot 2's sign-in clears it", || {
        !needs_you_events(&home, since, "needs_you.cleared").is_empty()
    });
    let cleared = needs_you_events(&home, since, "needs_you.cleared");
    assert_eq!(cleared.len(), 1, "{cleared:#?}");
    assert_eq!(cleared[0]["data"]["id"], needs, "{cleared:#?}");
}

#[test]
fn a_listing_read_clears_the_needs_you_of_runs_on_its_own_account() {
    // #4216 Q9: a no-turn read of slot 2 that finds Slack connected clears the needs-you
    // of a run on slot 2's home, and not that of one on the plain home.
    let (home, accounts) = read_home("needs-auth", "needs-auth", &[]);
    let plain_needs = auth_needs_you(&home, "connectors-needs-auth");
    app_sets(
        &home,
        &json!({"CLAUDE_CONFIG_DIR": accounts.managed.to_str().unwrap()}),
    );
    let managed_needs = auth_needs_you(&home, "connectors-needs-auth");
    std::fs::write(home.root().join("scenario"), "mcp-accounts").unwrap();
    accounts.set(2, &slack("connected"));
    let since = seq(&home);
    list(&home, &json!({"refresh": true, "account": 2}));
    assert_eq!(slack_status(&home, 2), "connected");
    eventually("slot 2's run's needs-you is cleared", || {
        !needs_you_events(&home, since, "needs_you.cleared").is_empty()
    });
    std::thread::sleep(Duration::from_millis(500));
    let cleared = needs_you_events(&home, since, "needs_you.cleared");
    assert_eq!(cleared.len(), 1, "{cleared:#?}");
    assert_eq!(cleared[0]["data"]["id"], managed_needs, "{cleared:#?}");
    assert_ne!(cleared[0]["data"]["id"], plain_needs);
}

#[test]
fn a_calls_read_only_hint_is_the_one_its_own_account_listed() {
    // #4216 Q8: slot 2's listing says Slack's search is not read-only, slot 1's that it
    // is. A call on slot 2's home is denied before its Claude Code starts; one on the
    // plain home starts.
    let (home, accounts) = read_home(
        "connected",
        "connected",
        &[("RLXD_TEST_CONNECTORS_CALL", "1")],
    );
    home.stop();
    let file = home.root().join("rlxd").join("connectors.json");
    let mut saved: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    let path = "/accounts/2/connectors/0/tools/0";
    assert_eq!(
        saved.pointer(path).map(|tool| tool["name"].clone()),
        Some(json!("mcp__claude_ai_Slack__slack_search_public")),
        "{saved:#}"
    );
    saved.pointer_mut(path).unwrap()["read_only_hint"] = Value::Bool(false);
    std::fs::write(&file, saved.to_string()).unwrap();
    home.start_dev();
    std::fs::write(home.root().join("scenario"), "mcp-direct").unwrap();
    let params = || {
        json!({"run_id": run_id(), "connector": SLACK,
            "tool": "mcp__claude_ai_Slack__slack_search_public", "input": {"query": "probe"}})
        .to_string()
    };
    app_sets(
        &home,
        &json!({"CLAUDE_CONFIG_DIR": accounts.managed.to_str().unwrap()}),
    );
    let before = starts(&home).len();
    let run = home.rlxd(&["call", "connectors.call", &params()]);
    assert_eq!(run.error_code(), "denied", "{run:?}");
    assert_eq!(run.json()["error"]["detail"]["by"], "rlxd", "{run:?}");
    assert_eq!(starts(&home).len(), before, "no Claude Code was started");
    // On the plain home, whose listing says read-only, the call starts (and, with no app,
    // is denied by the gate).
    app_sets(&home, &json!({"CLAUDE_CONFIG_DIR": ""}));
    let run = home.rlxd(&["call", "connectors.call", &params()]);
    assert_eq!(run.error_code(), "denied", "{run:?}");
    assert_eq!(
        run.json()["error"]["detail"]["by"],
        "unreachable",
        "{run:?}"
    );
    assert_eq!(starts(&home).len(), before + 1);
}
