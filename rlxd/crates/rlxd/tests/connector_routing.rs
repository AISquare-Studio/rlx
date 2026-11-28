//! A run's account, picked by its connectors (task #15), through the real socket, with
//! fake-claude standing in for Claude Code on every OS.
//!
//! Two accounts, as the brain's `accounts` would list them (`RLXD_TEST_ACCOUNTS`): slot 1,
//! the plain one, and slot 2, a managed one with homes of its own. Each account's
//! connectors are its config dir's `fake-connectors.json`, which a no-turn read
//! (`mcp-accounts`) takes into the map; a run then plays `init-connectors` (Slack
//! connected at init). Usage is `RLXD_TEST_USAGE`, a file holding what the brain's `usage`
//! would answer. Identities are seeded into rlxd's own `connectors.json` before it starts:
//! the read keeps them while the login marks stay. Fixture names use letters outside
//! a–f, so no grep of a log for one can match a run's id.

mod common;

use std::path::{Path, PathBuf};
use std::process::Child;

use common::runs::{fixture_log, home_with_scenario_file_in, run_id, seq};
use common::{TestHome, assert_valid, eventually, real_rlx};
use serde_json::{Value, json};

const SLACK: &str = "claude.ai Slack";

const MARK_1: &str = "mark-of-slot-1";
const MARK_2: &str = "mark-of-slot-2";

/// When slot 2's limit resets, in the usage fixture.
const RESETS: &str = "2026-11-29T17:00:00Z";

/// `mcp_status` rows: Slack in `status`.
fn slack(status: &str) -> Value {
    json!([{"name": SLACK, "status": status, "source": "claudeai",
        "tools": [{"name": "slack_search_public", "annotations": {"readOnly": true}}]}])
}

/// `mcp_status` rows with no Slack: Gmail alone, connected. (A list of no server at all
/// never settles, so its read fails: that account is `unread`.)
fn no_slack() -> Value {
    json!([{"name": "claude.ai Gmail", "status": "connected", "source": "claudeai",
        "tools": [{"name": "gmail_search", "annotations": {"readOnly": true}}]}])
}

/// The two accounts' homes in a test home.
struct Accounts {
    plain: PathBuf,
    managed: PathBuf,
    managed_tmp: PathBuf,
    /// Slot 3's homes, for the tests that list a third account.
    third: PathBuf,
    third_tmp: PathBuf,
    usage: PathBuf,
    listed: PathBuf,
}

impl Accounts {
    fn of(root: &Path) -> Self {
        Self {
            plain: root.join("user-home").join(".claude"),
            managed: root.join("account-2"),
            managed_tmp: root.join("account-2-tmp"),
            third: root.join("account-3"),
            third_tmp: root.join("account-3-tmp"),
            usage: root.join("usage.json"),
            listed: root.join("accounts.json"),
        }
    }

    /// Writes `rows` as `slot`'s connectors.
    fn set(&self, slot: u32, rows: &Value) {
        let dir = match slot {
            1 => &self.plain,
            2 => &self.managed,
            _ => &self.third,
        };
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("fake-connectors.json"), rows.to_string()).unwrap();
    }

    /// The brain's list: slot 1, and slot 2 when `two`.
    fn list(&self, two: bool) {
        self.list_slots(if two { &[1, 2] } else { &[1] });
    }

    /// The brain's list: each of `slots` (of 1, 2 and 3).
    fn list_slots(&self, slots: &[u32]) {
        let listed: Vec<Value> = slots
            .iter()
            .map(|slot| match slot {
                1 => json!({"slot": 1, "label": "main", "login": MARK_1}),
                2 => json!({"slot": 2, "label": "work",
                    "config_dir": self.managed, "tmp_dir": self.managed_tmp, "login": MARK_2}),
                _ => json!({"slot": 3, "label": "side",
                    "config_dir": self.third, "tmp_dir": self.third_tmp,
                    "login": "mark-of-slot-3"}),
            })
            .collect();
        std::fs::write(&self.listed, Value::Array(listed).to_string()).unwrap();
    }

    /// The brain's usage: `rows`, each `{slot, used_percent?, limited, resets_at?}`.
    fn usage(&self, rows: &Value) {
        std::fs::write(&self.usage, json!({"accounts": rows}).to_string()).unwrap();
    }
}

/// A home whose rlxd routes runs, its two accounts holding `one` and `two` (Slack's
/// status on each, or `None` for no Slack: [`no_slack`]), their identities on Slack `who`
/// when given (seeded), and no usage. Its map is read once.
fn routing_home(
    one: Option<&str>,
    two: Option<&str>,
    who: Option<(Option<&str>, Option<&str>)>,
) -> (TestHome, Accounts) {
    routing_home_with(one, two, who, false)
}

/// [`routing_home`]; with `own_config`, rlxd itself runs under a `CLAUDE_CONFIG_DIR` of its
/// own, `<home>/claude-config`, which is no account's.
fn routing_home_with(
    one: Option<&str>,
    two: Option<&str>,
    who: Option<(Option<&str>, Option<&str>)>,
    own_config: bool,
) -> (TestHome, Accounts) {
    let root = TestHome::new();
    let accounts = Accounts::of(root.root());
    accounts.set(1, &one.map_or_else(no_slack, slack));
    accounts.set(2, &two.map_or_else(no_slack, slack));
    accounts.list(true);
    accounts.usage(&json!([]));
    if let Some((first, second)) = who {
        seed(root.root(), &accounts, first, second);
    }
    let listed = accounts.listed.to_str().unwrap().to_owned();
    let usage = accounts.usage.to_str().unwrap().to_owned();
    let mut envs = vec![
        ("RLXD_TEST_ACCOUNTS", listed.as_str()),
        ("RLXD_TEST_USAGE", usage.as_str()),
        ("RLXD_TEST_CONNECTOR_MAP_MS", "off"),
        ("RLXD_TEST_CONNECTORS_NO_TURN", "1"),
        // Route 1: a try whose init shows Slack connected sends its prompt at once.
        ("RLXD_TEST_CONNECTOR_ROUTE", "1"),
    ];
    if !own_config {
        // rlxd, and so its default runs, on the plain account.
        envs.push(("CLAUDE_CONFIG_DIR", ""));
    }
    let home = home_with_scenario_file_in(root, "mcp-accounts", &envs);
    read_map(&home);
    std::fs::write(home.root().join("scenario"), "init-connectors").unwrap();
    (home, accounts)
}

/// rlxd's own `connectors.json`, before it starts: both accounts hold Slack connected,
/// slot 1 as `first` and slot 2 as `second` (`None`: not known).
fn seed(root: &Path, accounts: &Accounts, first: Option<&str>, second: Option<&str>) {
    let connector = |who: Option<&str>| {
        json!({"id": SLACK, "source": "claudeai", "status": "connected", "tools": [],
            "identity": who.map(|workspace| json!({"workspace": workspace}))})
    };
    let saved = json!({"listed": true, "accounts": {
        "1": {"label": "main", "login": MARK_1, "as_of": "2026-11-28T10:00:00Z",
              "connectors": [connector(first)]},
        "2": {"label": "work", "config_dir": accounts.managed, "tmp_dir": accounts.managed_tmp,
              "login": MARK_2, "as_of": "2026-11-28T10:00:00Z",
              "connectors": [connector(second)]}
    }});
    std::fs::create_dir_all(root.join("rlxd")).unwrap();
    std::fs::write(root.join("rlxd").join("connectors.json"), saved.to_string()).unwrap();
}

/// A refresh of every account's connectors, which must succeed: rlxd learns the accounts
/// and reads each one's map with no turn.
fn read_map(home: &TestHome) -> Value {
    let run = home.rlxd(&["call", "connectors.list", r#"{"refresh": true}"#]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    run.data()
}

/// A task run named by `connectors` (and `account` when pinned), mounting Slack.
fn task(connectors: &[&str], account: Option<u32>) -> Value {
    let mut params = json!({"kind": "task", "prompt": "digest",
        "mount": {"kind": "connectors", "connectors": [SLACK]}, "connectors": connectors});
    if let Some(slot) = account {
        params["account"] = json!(slot);
    }
    params
}

/// `run.start` of `params` as `id`.
fn start(home: &TestHome, params: &Value, id: &str) -> common::Run {
    let mut params = params.clone();
    params["run_id"] = json!(id);
    home.rlxd(&["call", "run.start", &params.to_string()])
}

/// `run.start`'s refusal: its code and detail, the detail valid against `schema`.
fn refused(home: &TestHome, params: &Value, schema: &str) -> (String, Value) {
    let id = run_id();
    let run = start(home, params, &id);
    assert_eq!(run.code(), Some(1), "{run:?}");
    let error = run.json()["error"].clone();
    assert_valid(schema, &error["detail"]);
    assert_valid("envelope.json", &run.json());
    assert!(no_run(home, &id), "a run was made: {run:?}");
    (
        error["code"].as_str().unwrap().to_owned(),
        error["detail"].clone(),
    )
}

/// Whether rlxd knows no run `id`, and no Claude Code was started for one.
fn no_run(home: &TestHome, id: &str) -> bool {
    let runs = home.rlxd(&["call", "run.list"]).data();
    let listed = runs["runs"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["run_id"] == id);
    !listed && starts_of(home, id).is_empty()
}

/// The fixture log's starts of run `id`: those whose command line names it.
fn starts_of(home: &TestHome, id: &str) -> Vec<Value> {
    if !home.root().join("fake-claude.log.json").exists() {
        return Vec::new();
    }
    fixture_log(home)["starts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|start| {
            start["argv"]
                .as_array()
                .is_some_and(|argv| argv.iter().any(|arg| arg == id))
        })
        .cloned()
        .collect()
}

/// Waits for run `id` to end, in `state`.
fn ended(home: &TestHome, id: &str, state: &str) {
    eventually(&format!("run {id} ends {state}"), || {
        let run = home.rlxd(&["call", "run.get", &json!({"run_id": id}).to_string()]);
        run.code() == Some(0) && run.data()["state"] == state
    });
}

/// The `run.started` events of run `id` from `since`, each valid.
fn started(home: &TestHome, since: u64, id: &str) -> Vec<Value> {
    let events = home
        .rlxd(&["events", "--topics", "run", "--since", &since.to_string()])
        .lines();
    let found: Vec<Value> = events
        .into_iter()
        .filter(|e| e["type"] == "run.started" && e["data"]["run_id"] == id)
        .collect();
    for event in &found {
        assert_valid("run-event.json", event);
    }
    found
}

/// Asserts `start` ran on slot 2's own homes.
fn on_slot_2(start: &Value, accounts: &Accounts) {
    let env = &start["env"];
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

/// Asserts `start` ran on the plain account: neither of the two homes set.
fn on_slot_1(start: &Value) {
    let env = &start["env"];
    assert!(
        env.get("CLAUDE_CONFIG_DIR")
            .is_none_or(|dir| dir.as_str() == Some("")),
        "{start:#}"
    );
    assert_eq!(env.get("CLAUDE_CODE_TMPDIR"), None, "{start:#}");
}

/// Slack's status on `slot`, as `connectors.list` gives it.
fn slack_on(home: &TestHome, slot: u32) -> Value {
    let data = home.rlxd(&["call", "connectors.list"]).data();
    data["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["account"] == slot)
        .and_then(|row| row["connectors"].as_array())
        .and_then(|rows| rows.iter().find(|c| c["id"] == SLACK))
        .map_or(Value::Null, |c| c["status"].clone())
}

// --- acceptance 1 --------------------------------------------------------------------

#[test]
fn a_run_by_its_connectors_starts_on_the_one_account_holding_them_connected() {
    let (home, accounts) = routing_home(Some("needs-auth"), Some("connected"), None);
    let since = seq(&home);
    let id = run_id();
    let run = start(&home, &task(&[SLACK], None), &id);
    assert_eq!(run.code(), Some(0), "{run:?}");
    ended(&home, &id, "done");
    let events = started(&home, since, &id);
    assert_eq!(events.len(), 1, "{events:#?}");
    assert_eq!(
        events[0]["data"]["account"],
        json!({"slot": 2, "by": "connectors"})
    );
    // On slot 2's own homes, its init filed there; slot 1's map as it was.
    let runs = starts_of(&home, &id);
    assert_eq!(runs.len(), 1, "{runs:#?}");
    on_slot_2(&runs[0], &accounts);
    assert_eq!(slack_on(&home, 2), "connected");
    assert_eq!(slack_on(&home, 1), "needs-auth");
    // run.get and run.list say it too, valid.
    let got = home
        .rlxd(&["call", "run.get", &json!({"run_id": id}).to_string()])
        .data();
    assert_valid("run-get-reply.json", &got);
    assert_eq!(got["account"], json!({"slot": 2, "by": "connectors"}));
    let listed = home.rlxd(&["call", "run.list"]).data();
    assert_valid("run-list-reply.json", &listed);
    let summary = listed["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["run_id"] == id)
        .unwrap();
    assert_eq!(summary["account"], json!({"slot": 2, "by": "connectors"}));
    // rlxd's log names the slot and why, never a home.
    let log = home.log();
    assert!(
        log.contains(&format!("run {id}: on account 2 (connectors)")),
        "{log}"
    );
    assert!(!log.contains(accounts.managed.to_str().unwrap()), "{log}");
}

#[test]
fn a_run_with_neither_pin_nor_connectors_keeps_the_apps_environment() {
    let (home, _accounts) = routing_home(Some("connected"), Some("connected"), None);
    let since = seq(&home);
    let id = run_id();
    let run = start(
        &home,
        &json!({"kind": "task", "prompt": "digest", "mount": {"kind": "no_tools"}}),
        &id,
    );
    assert_eq!(run.code(), Some(0), "{run:?}");
    ended(&home, &id, "done");
    let events = started(&home, since, &id);
    assert_eq!(
        events[0]["data"]["account"],
        json!({"slot": 1, "by": "default"})
    );
    on_slot_1(&starts_of(&home, &id)[0]);
}

// --- acceptance 2 --------------------------------------------------------------------

#[test]
fn a_limited_sole_holder_is_refused_with_its_reset_and_no_run_starts_anywhere() {
    let (home, accounts) = routing_home(Some("needs-auth"), Some("connected"), None);
    accounts.usage(&json!([
        {"slot": 1, "used_percent": 5.0, "limited": false},
        {"slot": 2, "used_percent": 100.0, "limited": true, "resets_at": RESETS},
    ]));
    let before = fixture_log(&home)["starts"].as_array().unwrap().len();
    let (code, detail) = refused(
        &home,
        &task(&[SLACK], None),
        "connector-account-limited.json",
    );
    assert_eq!(code, "connector_account_limited");
    assert_eq!(
        detail,
        json!({"connector": SLACK, "account": 2, "resets_at": RESETS})
    );
    // No Claude Code started at all: not on slot 1 either.
    assert_eq!(
        fixture_log(&home)["starts"].as_array().unwrap().len(),
        before
    );
    let listed = home.rlxd(&["call", "run.list"]).data();
    assert_eq!(listed["runs"], json!([]), "{listed:#}");
}

#[test]
fn a_pin_to_a_limited_account_is_never_refused_as_limited() {
    let (home, accounts) = routing_home(Some("needs-auth"), Some("connected"), None);
    accounts.usage(&json!([{"slot": 2, "limited": true, "resets_at": RESETS}]));
    let since = seq(&home);
    let id = run_id();
    let run = start(&home, &task(&[SLACK], Some(2)), &id);
    assert_eq!(run.code(), Some(0), "{run:?}");
    ended(&home, &id, "done");
    assert_eq!(
        started(&home, since, &id)[0]["data"]["account"],
        json!({"slot": 2, "by": "pinned"})
    );
}

#[test]
fn of_two_holders_known_the_same_the_one_with_more_room_and_no_limit_takes_the_run() {
    let (home, accounts) = routing_home(
        Some("connected"),
        Some("connected"),
        Some((Some("Gizmo"), Some("Gizmo"))),
    );
    accounts.usage(&json!([
        {"slot": 1, "used_percent": 80.0, "limited": false},
        {"slot": 2, "used_percent": 10.0, "limited": false},
    ]));
    let since = seq(&home);
    let id = run_id();
    assert_eq!(start(&home, &task(&[SLACK], None), &id).code(), Some(0));
    ended(&home, &id, "done");
    assert_eq!(
        started(&home, since, &id)[0]["data"]["account"],
        json!({"slot": 2, "by": "connectors"})
    );
    on_slot_2(&starts_of(&home, &id)[0], &accounts);
    // Slot 2 limited now: slot 1 takes the next, though it has less room.
    accounts.usage(&json!([
        {"slot": 1, "used_percent": 80.0, "limited": false},
        {"slot": 2, "used_percent": 0.0, "limited": true, "resets_at": RESETS},
    ]));
    let since = seq(&home);
    let next = run_id();
    assert_eq!(start(&home, &task(&[SLACK], None), &next).code(), Some(0));
    ended(&home, &next, "done");
    assert_eq!(
        started(&home, since, &next)[0]["data"]["account"],
        json!({"slot": 1, "by": "connectors"})
    );
    on_slot_1(&starts_of(&home, &next)[0]);
}

#[test]
fn with_no_usage_the_lowest_slot_of_holders_known_the_same_takes_the_run() {
    let (home, _accounts) = routing_home(
        Some("connected"),
        Some("connected"),
        Some((Some("Gizmo"), Some("Gizmo"))),
    );
    let since = seq(&home);
    let id = run_id();
    assert_eq!(start(&home, &task(&[SLACK], None), &id).code(), Some(0));
    ended(&home, &id, "done");
    assert_eq!(
        started(&home, since, &id)[0]["data"]["account"],
        json!({"slot": 1, "by": "connectors"})
    );
}

// --- acceptance 3 --------------------------------------------------------------------

#[test]
fn two_identities_are_ambiguous_until_the_run_pins_one() {
    let (home, accounts) = routing_home(
        Some("connected"),
        Some("connected"),
        Some((Some("Gizmo"), Some("Quorum"))),
    );
    let (code, detail) = refused(&home, &task(&[SLACK], None), "ambiguous-account.json");
    assert_eq!(code, "ambiguous_account");
    assert_eq!(
        detail,
        json!({"connector": SLACK, "candidates": [
            {"account": 1, "identity": {"workspace": "Gizmo"}},
            {"account": 2, "identity": {"workspace": "Quorum"}},
        ]})
    );
    let since = seq(&home);
    let id = run_id();
    let run = start(&home, &task(&[SLACK], Some(2)), &id);
    assert_eq!(run.code(), Some(0), "{run:?}");
    ended(&home, &id, "done");
    assert_eq!(
        started(&home, since, &id)[0]["data"]["account"],
        json!({"slot": 2, "by": "pinned"})
    );
    on_slot_2(&starts_of(&home, &id)[0], &accounts);
    // The identities are on the wire alone: never in rlxd's log.
    let log = home.log();
    assert!(!log.contains("Gizmo") && !log.contains("Quorum"), "{log}");
}

#[test]
fn two_holders_whose_identities_are_not_known_are_ambiguous_with_nulls() {
    // D2, the strict rule: an identity rlxd hasn't read can't be shown to be the same.
    let (home, _accounts) = routing_home(
        Some("connected"),
        Some("connected"),
        Some((Some("Gizmo"), None)),
    );
    let (code, detail) = refused(&home, &task(&[SLACK], None), "ambiguous-account.json");
    assert_eq!(code, "ambiguous_account");
    assert_eq!(
        detail["candidates"],
        json!([
            {"account": 1, "identity": {"workspace": "Gizmo"}},
            {"account": 2, "identity": null},
        ])
    );
    let (home, _accounts) = routing_home(Some("connected"), Some("connected"), None);
    let (code, detail) = refused(&home, &task(&[SLACK], None), "ambiguous-account.json");
    assert_eq!(code, "ambiguous_account");
    assert_eq!(
        detail["candidates"],
        json!([{"account": 1, "identity": null}, {"account": 2, "identity": null}])
    );
}

// --- acceptance 4 --------------------------------------------------------------------

#[test]
fn a_pin_lacking_the_connector_is_connector_unavailable() {
    let (home, _accounts) = routing_home(Some("connected"), Some("needs-auth"), None);
    let (code, detail) = refused(
        &home,
        &task(&[SLACK], Some(2)),
        "connector-unavailable.json",
    );
    assert_eq!(code, "connector_unavailable");
    assert_eq!(
        detail,
        json!({"connector": SLACK, "accounts": [
            {"account": 1, "status": "connected"},
            {"account": 2, "status": "needs-auth"},
        ]})
    );
    // A slot rlx doesn't know is no_such_account; a pin is never ignored.
    let id = run_id();
    let run = start(&home, &task(&[SLACK], Some(7)), &id);
    assert_eq!(run.error_code(), "no_such_account", "{run:?}");
    assert!(no_run(&home, &id));
}

#[test]
fn no_account_holding_the_connector_is_connector_unavailable_with_each_status() {
    let (home, _accounts) = routing_home(Some("needs-auth"), None, None);
    let (code, detail) = refused(&home, &task(&[SLACK], None), "connector-unavailable.json");
    assert_eq!(code, "connector_unavailable");
    assert_eq!(
        detail["accounts"],
        json!([
            {"account": 1, "status": "needs-auth"},
            {"account": 2, "status": "absent"},
        ])
    );
}

#[test]
fn connectors_with_mount_no_tools_are_refused_as_a_mismatch() {
    let (home, _accounts) = routing_home(Some("connected"), None, None);
    let id = run_id();
    let run = start(
        &home,
        &json!({"kind": "task", "prompt": "digest", "mount": {"kind": "no_tools"},
            "connectors": [SLACK]}),
        &id,
    );
    assert_eq!(run.error_code(), "kind_mount_mismatch", "{run:?}");
}

#[test]
fn a_routed_runs_connectors_are_checked_again_at_its_init() {
    // The map says Slack is connected on slot 2; the run's own init finds it signed out
    // there: the gate (5.5) ends the run, on that account, and files what it found.
    let (home, accounts) = routing_home(Some("needs-auth"), Some("connected"), None);
    std::fs::write(home.root().join("scenario"), "connectors-needs-auth").unwrap();
    let id = run_id();
    assert_eq!(start(&home, &task(&[SLACK], None), &id).code(), Some(0));
    ended(&home, &id, "failed");
    let got = home
        .rlxd(&["call", "run.get", &json!({"run_id": id}).to_string()])
        .data();
    assert_eq!(got["account"], json!({"slot": 2, "by": "connectors"}));
    assert_eq!(got["last_event"]["data"]["reason"], "connectors", "{got:#}");
    eventually("the init is filed on slot 2", || {
        slack_on(&home, 2) == "needs-auth"
    });
    // Its needs-you for the sign-in is out. Slack signed in on slot 1, the app's and the
    // runs' own home, where it did not run, clears nothing; on slot 2, its own account, it
    // clears it.
    std::fs::write(home.root().join("scenario"), "mcp-accounts").unwrap();
    let cleared = |since: u64| {
        home.rlxd(&[
            "events",
            "--topics",
            "needs_you",
            "--since",
            &since.to_string(),
        ])
        .lines()
        .iter()
        .any(|e| e["type"] == "needs_you.cleared" && e["data"]["id"] == format!("run:{id}:1"))
    };
    let since = seq(&home);
    accounts.set(1, &slack("connected"));
    let run = home.rlxd(&[
        "call",
        "connectors.list",
        r#"{"refresh": true, "account": 1}"#,
    ]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_eq!(slack_on(&home, 1), "connected");
    assert!(
        !cleared(since),
        "slot 1's sign-in cleared a run that ran on slot 2"
    );
    let since = seq(&home);
    accounts.set(2, &slack("connected"));
    read_map(&home);
    eventually("the run's needs-you is cleared", || cleared(since));
}

/// Run `id`'s transcript under `home`: `projects/<the space's folder>/<id>.jsonl`, each
/// that is there (a file or a link).
fn transcripts(home: &Path, id: &str) -> Vec<PathBuf> {
    std::fs::read_dir(home.join("projects"))
        .into_iter()
        .flatten()
        .filter_map(|entry| Some(entry.ok()?.path().join(format!("{id}.jsonl"))))
        .filter(|path| path.symlink_metadata().is_ok())
        .collect()
}

/// A run routed to slot 2 by Slack, done: its id and its transcript there.
fn routed_and_done(home: &TestHome, accounts: &Accounts) -> (String, PathBuf) {
    let id = run_id();
    assert_eq!(start(home, &task(&[SLACK], None), &id).code(), Some(0));
    ended(home, &id, "done");
    let found = transcripts(&accounts.managed, &id);
    assert_eq!(found.len(), 1, "no transcript on slot 2: {found:?}");
    (id, found[0].clone())
}

fn erase(home: &TestHome) -> common::Run {
    home.rlxd(&["call", "space.erase", r#"{"name": "default"}"#])
}

#[test]
fn a_run_routed_to_the_plain_account_is_cleared_by_a_sign_in_there() {
    // #4306 G1, the owner's main route: Slack connected on slot 1 alone. The run is routed
    // there, its init finds Slack signed out, and its needs-you for the sign-in is out.
    // Slack signed in on slot 1 again clears it. (Its recorded home is ~/.claude, which is
    // no account's CLAUDE_CONFIG_DIR once rlx has listed the accounts: the sign-in goes by
    // the run's slot, never by that home.)
    let (home, accounts) = routing_home(Some("connected"), None, None);
    std::fs::write(home.root().join("scenario"), "connectors-needs-auth").unwrap();
    let since = seq(&home);
    let id = run_id();
    assert_eq!(start(&home, &task(&[SLACK], None), &id).code(), Some(0));
    ended(&home, &id, "failed");
    let got = home
        .rlxd(&["call", "run.get", &json!({"run_id": id}).to_string()])
        .data();
    assert_eq!(
        got["account"],
        json!({"slot": 1, "by": "connectors"}),
        "{got:#}"
    );
    assert_eq!(recorded_home(&home, &id), accounts.plain.to_str().unwrap());
    let needs_you = |kind: &str, since: u64| {
        home.rlxd(&[
            "events",
            "--topics",
            "needs_you",
            "--since",
            &since.to_string(),
        ])
        .lines()
        .iter()
        .any(|e| e["type"] == kind && e["data"]["id"] == format!("run:{id}:1"))
    };
    assert!(needs_you("needs_you.raised", since), "no needs-you raised");
    eventually("the init is filed on slot 1", || {
        slack_on(&home, 1) == "needs-auth"
    });
    let since = seq(&home);
    std::fs::write(home.root().join("scenario"), "mcp-accounts").unwrap();
    accounts.set(1, &slack("connected"));
    read_map(&home);
    assert_eq!(slack_on(&home, 1), "connected");
    eventually("slot 1's sign-in clears the run's needs-you", || {
        needs_you("needs_you.cleared", since)
    });
}

#[test]
fn erasing_a_space_takes_a_routed_runs_transcript_from_its_own_accounts_home_alone() {
    let (home, accounts) = routing_home(Some("needs-auth"), Some("connected"), None);
    let (id, ours) = routed_and_done(&home, &accounts);
    // A default run of the space too, on rlxd's own home (slot 1's here), so that home is
    // gone into: its transcript goes, and nothing of the routed run's is looked for there.
    let plain = run_id();
    let default = json!({"kind": "task", "prompt": "digest", "mount": {"kind": "no_tools"}});
    assert_eq!(start(&home, &default, &plain).code(), Some(0));
    ended(&home, &plain, "done");
    let plain_transcript = transcripts(&accounts.plain, &plain);
    assert_eq!(plain_transcript.len(), 1, "{plain_transcript:?}");
    // The same name as the routed run's under slot 1's home: not that run's, which ran on
    // slot 2.
    let folder = ours.parent().unwrap().file_name().unwrap();
    let theirs = accounts.plain.join("projects").join(folder);
    std::fs::create_dir_all(&theirs).unwrap();
    let theirs = theirs.join(format!("{id}.jsonl"));
    std::fs::write(&theirs, "{}\n").unwrap();
    let erased = erase(&home);
    assert_eq!(erased.code(), Some(0), "{erased:?}");
    assert!(!ours.exists(), "slot 2's transcript is left");
    assert!(
        !plain_transcript[0].exists(),
        "the default run's transcript is left"
    );
    assert!(theirs.exists(), "slot 1's file of the same name went");
    assert_eq!(
        home.rlxd(&["call", "run.get", &json!({"run_id": id}).to_string()])
            .error_code(),
        "unknown_run"
    );
}

#[test]
fn erasing_a_space_whose_runs_home_is_gone_removes_the_rest() {
    let (home, accounts) = routing_home(Some("needs-auth"), Some("connected"), None);
    let (id, _) = routed_and_done(&home, &accounts);
    std::fs::remove_dir_all(&accounts.managed).unwrap();
    let erased = erase(&home);
    assert_eq!(erased.code(), Some(0), "{erased:?}");
    assert_eq!(
        home.rlxd(&["call", "run.get", &json!({"run_id": id}).to_string()])
            .error_code(),
        "unknown_run"
    );
}

#[cfg(unix)]
#[test]
fn erasing_a_space_follows_no_link_planted_in_a_runs_home() {
    // The space's project folder in slot 2's home, swapped for a link out of it: refused,
    // nothing removed, and what the link names left.
    let (home, accounts) = routing_home(Some("needs-auth"), Some("connected"), None);
    let (id, ours) = routed_and_done(&home, &accounts);
    let project = ours.parent().unwrap().to_path_buf();
    let outside = home.root().join("outside-project");
    std::fs::rename(&project, &outside).unwrap();
    std::os::unix::fs::symlink(&outside, &project).unwrap();
    let refused = erase(&home);
    assert_eq!(refused.error_code(), "link_refused", "{refused:?}");
    assert!(outside.join(format!("{id}.jsonl")).exists());
    assert_eq!(
        home.rlxd(&["call", "run.get", &json!({"run_id": id}).to_string()])
            .data()["state"],
        "done"
    );
    // The transcript itself a link to a file elsewhere: the link goes, its target stays.
    std::fs::remove_file(&project).unwrap();
    std::fs::rename(&outside, &project).unwrap();
    let elsewhere = home.root().join("elsewhere.jsonl");
    std::fs::write(&elsewhere, "keep\n").unwrap();
    std::fs::remove_file(&ours).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &ours).unwrap();
    let erased = erase(&home);
    assert_eq!(erased.code(), Some(0), "{erased:?}");
    assert!(ours.symlink_metadata().is_err(), "the link is left");
    assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), "keep\n");
}

#[cfg(unix)]
#[test]
fn a_link_in_an_account_no_run_of_the_space_ran_on_never_stops_its_erase() {
    // Slot 1's home holds a projects/ that is a link, but no run of the space ran there:
    // it is never gone into.
    let (home, accounts) = routing_home(Some("needs-auth"), Some("connected"), None);
    let (_, ours) = routed_and_done(&home, &accounts);
    let outside = home.root().join("outside-projects");
    std::fs::create_dir_all(outside.join("anything")).unwrap();
    std::fs::write(outside.join("anything").join("keep"), "x").unwrap();
    std::os::unix::fs::symlink(&outside, accounts.plain.join("projects")).unwrap();
    let erased = erase(&home);
    assert_eq!(erased.code(), Some(0), "{erased:?}");
    assert!(!ours.exists());
    assert!(outside.join("anything").join("keep").exists());
}

/// What `run.json` records of run `id`: its `home`.
fn recorded_home(home: &TestHome, id: &str) -> Value {
    let record = home
        .root()
        .join("rlxd")
        .join("runs")
        .join(id)
        .join("run.json");
    let record: Value = serde_json::from_str(&std::fs::read_to_string(record).unwrap()).unwrap();
    record["home"].clone()
}

#[test]
fn a_run_pinned_to_the_plain_account_records_and_is_erased_from_its_default_home() {
    // rlxd itself runs under a CLAUDE_CONFIG_DIR of its own, no account's. A run pinned to
    // the plain account has that variable taken away: Claude Code keeps its transcript in
    // ~/.claude, which is the home its run.json records, and where it is erased.
    let (home, accounts) = routing_home_with(Some("connected"), Some("connected"), None, true);
    let id = run_id();
    let pinned = json!({"kind": "task", "prompt": "digest", "mount": {"kind": "no_tools"},
        "account": 1});
    assert_eq!(start(&home, &pinned, &id).code(), Some(0));
    ended(&home, &id, "done");
    on_slot_1(&starts_of(&home, &id)[0]);
    assert_eq!(recorded_home(&home, &id), accounts.plain.to_str().unwrap());
    let ours = transcripts(&accounts.plain, &id);
    assert_eq!(ours.len(), 1, "{ours:?}");
    // The same name under rlxd's own home: not this run's.
    let own = home.root().join("claude-config").join("projects");
    let theirs = own.join(ours[0].parent().unwrap().file_name().unwrap());
    std::fs::create_dir_all(&theirs).unwrap();
    let theirs = theirs.join(format!("{id}.jsonl"));
    std::fs::write(&theirs, "{}\n").unwrap();
    assert_eq!(erase(&home).code(), Some(0));
    assert!(!ours[0].exists(), "the plain account's transcript is left");
    assert!(theirs.exists(), "rlxd's own home's file went");
}

#[test]
fn a_routed_attempt_takes_none_of_the_apps_homes_and_records_its_accounts() {
    let (home, accounts) = routing_home(Some("needs-auth"), Some("connected"), None);
    let app_home = home.root().join("app-claude");
    let app_secure = home.root().join("app-secure");
    let change =
        json!({"CLAUDE_CONFIG_DIR": app_home, "CLAUDE_SECURESTORAGE_CONFIG_DIR": app_secure});
    let set = home.rlxd(&["call", "--as-app", "env.set", &change.to_string()]);
    assert_eq!(set.code(), Some(0), "{set:?}");
    let (id, _) = routed_and_done(&home, &accounts);
    let started = &starts_of(&home, &id)[0];
    on_slot_2(started, &accounts);
    assert_eq!(
        started["env"].get("CLAUDE_SECURESTORAGE_CONFIG_DIR"),
        None,
        "{started:#}"
    );
    assert_eq!(
        recorded_home(&home, &id),
        accounts.managed.to_str().unwrap()
    );
}

#[test]
fn a_queued_run_keeps_the_account_it_was_routed_to() {
    // Two holders of one workspace; slot 2 has the room. A refresh run holds the pool
    // (refreshes run one at a time), so the routed refresh queues, its route decided.
    let (home, accounts) = routing_home(
        Some("connected"),
        Some("connected"),
        Some((Some("Gizmo"), Some("Gizmo"))),
    );
    accounts.usage(&json!([
        {"slot": 1, "used_percent": 80.0, "limited": false},
        {"slot": 2, "used_percent": 10.0, "limited": false},
    ]));
    std::fs::write(home.root().join("scenario"), "slow").unwrap();
    let holder = run_id();
    let refresh =
        json!({"kind": "widget_refresh", "prompt": "hold", "mount": {"kind": "no_tools"}});
    assert_eq!(start(&home, &refresh, &holder).code(), Some(0));
    eventually("the holder runs", || {
        let got = home.rlxd(&["call", "run.get", &json!({"run_id": holder}).to_string()]);
        got.data()["state"] == "running"
    });
    let since = seq(&home);
    let id = run_id();
    let routed = json!({"kind": "widget_refresh", "prompt": "digest",
        "mount": {"kind": "connectors", "connectors": [SLACK]}, "connectors": [SLACK]});
    assert_eq!(start(&home, &routed, &id).code(), Some(0));
    let queued = home
        .rlxd(&["call", "run.get", &json!({"run_id": id}).to_string()])
        .data();
    assert_eq!(queued["state"], "queued", "{queued:#}");
    assert_eq!(queued["account"], json!({"slot": 2, "by": "connectors"}));
    // Slot 2 limited now: a route picked again would go to slot 1. None is.
    accounts.usage(&json!([
        {"slot": 1, "used_percent": 0.0, "limited": false},
        {"slot": 2, "limited": true, "resets_at": RESETS},
    ]));
    std::fs::write(home.root().join("scenario"), "init-connectors").unwrap();
    let cancel = home.rlxd(&["call", "run.cancel", &json!({"run_id": holder}).to_string()]);
    assert_eq!(cancel.code(), Some(0), "{cancel:?}");
    ended(&home, &id, "done");
    assert_eq!(
        started(&home, since, &id)[0]["data"]["account"],
        json!({"slot": 2, "by": "connectors"})
    );
    on_slot_2(&starts_of(&home, &id)[0], &accounts);
}

#[test]
fn the_real_brains_usage_routes_and_refuses_a_limited_sole_holder() {
    let Some(rlx) = real_rlx() else { return };
    let root = TestHome::new().with_rlx(rlx);
    // Slot 2, a managed slot as rlx makes one, signed in, Slack connected there alone.
    let slot = root.root().join("claude-accounts").join("2");
    std::fs::create_dir_all(&slot).unwrap();
    std::fs::write(
        slot.join(".aisquare-account.json"),
        r#"{"slot": 2, "created_at": "2026-11-28T00:00:00+00:00"}"#,
    )
    .unwrap();
    std::fs::write(
        slot.join(".claude.json"),
        json!({"oauthAccount": {"emailAddress": "fixture@example.com",
            "accountUuid": "0e9d6c1a-uuid-of-the-fixture"}})
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        slot.join("fake-connectors.json"),
        slack("connected").to_string(),
    )
    .unwrap();
    let plain = root.root().join("user-home").join(".claude");
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::write(
        plain.join("fake-connectors.json"),
        slack("needs-auth").to_string(),
    )
    .unwrap();
    let home = home_with_scenario_file_in(
        root,
        "mcp-accounts",
        &[
            ("RLXD_TEST_CONNECTORS_NO_TURN", "1"),
            ("RLXD_TEST_CONNECTOR_MAP_MS", "off"),
            ("RLXD_TEST_CONNECTOR_ROUTE", "1"),
        ],
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    while home
        .rlxd(&["call", "connectors.list", r#"{"account":2}"#])
        .code()
        != Some(0)
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the brain never listed slot 2"
        );
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    read_map(&home);
    std::fs::write(home.root().join("scenario"), "init-connectors").unwrap();
    // The brain's usage says slot 2 has room: the run goes there.
    let id = run_id();
    assert_eq!(start(&home, &task(&[SLACK], None), &id).code(), Some(0));
    ended(&home, &id, "done");
    assert!(!home.log().contains("route: no usage"), "{}", home.log());
    // A weekly limit rlx remembers for slot 2 (as a refused turn's hook would): refused.
    // In rlx's own store for this home (AISQUARE_HOME), as the brain reads it; uv as
    // real_rlx finds rlx, nothing of the agent the tests run under passed on.
    let mut python = std::process::Command::new("uv");
    for (name, _) in std::env::vars_os() {
        let name = name.to_string_lossy().into_owned();
        if name.starts_with("AISQUARE_") || name.starts_with("CLAUDE") || name == "RLX_HOME" {
            python.env_remove(name);
        }
    }
    let seeded = python
        .args([
            "run",
            "--quiet",
            "python",
            "-c",
            "from datetime import UTC, datetime, timedelta\n\
             from aisquare.services import claude_accounts as s\n\
             until = (datetime.now(UTC) + timedelta(hours=5)).replace(microsecond=0)\n\
             s.remember_limit(2, 'week', until)\n\
             print(until.isoformat().replace('+00:00', 'Z'), end='')",
        ])
        .env("AISQUARE_HOME", home.root())
        .env("HOME", home.root().join("user-home"))
        .env("USERPROFILE", home.root().join("user-home"))
        .current_dir(common::workspace().parent().unwrap())
        .output()
        .unwrap();
    assert!(seeded.status.success(), "{seeded:?}");
    let until = String::from_utf8(seeded.stdout).unwrap();
    let (code, detail) = refused(
        &home,
        &task(&[SLACK], None),
        "connector-account-limited.json",
    );
    assert_eq!(code, "connector_account_limited");
    assert_eq!(detail["account"], 2);
    assert_eq!(detail["resets_at"], until.as_str());
}

/// #4291's setup: slot 1 (the plain account) and slot 3 (B) without Slack, slot 2 (A)
/// with it connected; a run routed by Slack in the space `sx`, its session kept, done.
/// Its id, and its transcript in A's home.
fn three_accounts_and_a_routed_run_in_sx() -> (TestHome, Accounts, String, PathBuf) {
    let root = TestHome::new();
    let accounts = Accounts::of(root.root());
    accounts.set(1, &slack("needs-auth"));
    accounts.set(2, &slack("connected"));
    accounts.set(3, &no_slack());
    accounts.list_slots(&[1, 2, 3]);
    accounts.usage(&json!([]));
    let listed = accounts.listed.to_str().unwrap().to_owned();
    let usage = accounts.usage.to_str().unwrap().to_owned();
    let home = home_with_scenario_file_in(
        root,
        "mcp-accounts",
        &[
            ("RLXD_TEST_ACCOUNTS", listed.as_str()),
            ("RLXD_TEST_USAGE", usage.as_str()),
            ("RLXD_TEST_CONNECTOR_MAP_MS", "off"),
            ("RLXD_TEST_CONNECTORS_NO_TURN", "1"),
            ("RLXD_TEST_CONNECTOR_ROUTE", "1"),
            ("CLAUDE_CONFIG_DIR", ""),
        ],
    );
    read_map(&home);
    std::fs::write(home.root().join("scenario"), "init-connectors").unwrap();
    let id = run_id();
    let mut params = task(&[SLACK], None);
    params["space"] = json!("sx");
    params["persist_session"] = json!(true);
    assert_eq!(start(&home, &params, &id).code(), Some(0));
    ended(&home, &id, "done");
    assert_eq!(
        recorded_home(&home, &id),
        accounts.managed.to_str().unwrap()
    );
    let found = transcripts(&accounts.managed, &id);
    assert_eq!(found.len(), 1, "no transcript in A's home: {found:?}");
    (home, accounts, id, found[0].clone())
}

/// The same name as `ours` (A's transcript), planted under `home`'s own projects/.
fn plant_beside(home: &Path, ours: &Path) -> PathBuf {
    let folder = home
        .join("projects")
        .join(ours.parent().unwrap().file_name().unwrap());
    std::fs::create_dir_all(&folder).unwrap();
    let planted = folder.join(ours.file_name().unwrap());
    std::fs::write(&planted, "not this run's\n").unwrap();
    planted
}

fn erase_sx(home: &TestHome) -> common::Run {
    home.rlxd(&["call", "space.erase", r#"{"name": "sx"}"#])
}

#[test]
fn erasing_sx_removes_the_routed_runs_transcript_in_a_and_no_same_named_file_in_b_or_the_plain_home()
 {
    // #4291 (a).
    let (home, accounts, id, ours) = three_accounts_and_a_routed_run_in_sx();
    let in_b = plant_beside(&accounts.third, &ours);
    let in_plain = plant_beside(&accounts.plain, &ours);
    let erased = erase_sx(&home);
    assert_eq!(erased.code(), Some(0), "{erased:?}");
    assert!(!ours.exists(), "A's transcript is left");
    assert!(in_b.exists(), "B's file of the same name went");
    assert!(
        in_plain.exists(),
        "the plain home's file of the same name went"
    );
    let removed = erased.data()["removed"].to_string();
    assert!(!removed.contains("account-3"), "{removed}");
    assert!(!removed.contains("user-home"), "{removed}");
    assert!(removed.contains(&format!("{id}.jsonl")), "{removed}");
}

#[cfg(unix)]
#[test]
fn a_symlinked_projects_in_b_never_blocks_the_erase_of_as_run() {
    // #4291 (b).
    let (home, accounts, _id, ours) = three_accounts_and_a_routed_run_in_sx();
    let outside = home.root().join("outside-b-projects");
    let victim = outside.join(ours.parent().unwrap().file_name().unwrap());
    std::fs::create_dir_all(&victim).unwrap();
    let victim = victim.join(ours.file_name().unwrap());
    std::fs::write(&victim, "keep\n").unwrap();
    std::os::unix::fs::symlink(&outside, accounts.third.join("projects")).unwrap();
    let erased = erase_sx(&home);
    assert_eq!(erased.code(), Some(0), "{erased:?}");
    assert!(!ours.exists(), "A's transcript is left");
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep\n");
}

#[test]
fn a_routed_runs_transcript_is_erased_from_its_recorded_home_after_its_account_is_dropped() {
    // #4291 (c): A is no longer rlx's, its home still on disk. The run's own record names
    // that home, so the erase finds the transcript there.
    let (home, accounts, id, ours) = three_accounts_and_a_routed_run_in_sx();
    std::fs::write(home.root().join("scenario"), "mcp-accounts").unwrap();
    accounts.list_slots(&[1, 3]);
    read_map(&home);
    let listed = home.rlxd(&["call", "connectors.list"]).data();
    assert!(
        listed["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["account"] != 2),
        "{listed:#}"
    );
    assert!(accounts.managed.exists());
    let erased = erase_sx(&home);
    assert_eq!(erased.code(), Some(0), "{erased:?}");
    assert!(!ours.exists(), "A's transcript is left");
    assert_eq!(
        home.rlxd(&["call", "run.get", &json!({"run_id": id}).to_string()])
            .error_code(),
        "unknown_run"
    );
}

#[test]
fn a_queued_routed_run_whose_account_is_dropped_fails_and_moves_nowhere() {
    // #4291 F3: routed to slot 2 and queued; slot 2 then no longer rlx's. Its attempt runs
    // there or nowhere: it fails, and no Claude Code starts, on the app's environment or
    // any other.
    let (home, accounts) = routing_home(Some("needs-auth"), Some("connected"), None);
    std::fs::write(home.root().join("scenario"), "slow").unwrap();
    let holder = run_id();
    let refresh =
        json!({"kind": "widget_refresh", "prompt": "hold", "mount": {"kind": "no_tools"}});
    assert_eq!(start(&home, &refresh, &holder).code(), Some(0));
    eventually("the holder runs", || {
        let got = home.rlxd(&["call", "run.get", &json!({"run_id": holder}).to_string()]);
        got.data()["state"] == "running"
    });
    let id = run_id();
    let routed = json!({"kind": "widget_refresh", "prompt": "digest",
        "mount": {"kind": "connectors", "connectors": [SLACK]}, "connectors": [SLACK]});
    assert_eq!(start(&home, &routed, &id).code(), Some(0));
    let queued = home
        .rlxd(&["call", "run.get", &json!({"run_id": id}).to_string()])
        .data();
    assert_eq!(queued["state"], "queued", "{queued:#}");
    assert_eq!(queued["account"], json!({"slot": 2, "by": "connectors"}));
    std::fs::write(home.root().join("scenario"), "mcp-accounts").unwrap();
    accounts.list(false);
    read_map(&home);
    std::fs::write(home.root().join("scenario"), "init-connectors").unwrap();
    let cancel = home.rlxd(&["call", "run.cancel", &json!({"run_id": holder}).to_string()]);
    assert_eq!(cancel.code(), Some(0), "{cancel:?}");
    ended(&home, &id, "failed");
    let got = home
        .rlxd(&["call", "run.get", &json!({"run_id": id}).to_string()])
        .data();
    let message = got["last_event"]["data"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(message.contains("rlx no longer knows account 2"), "{got:#}");
    assert!(
        starts_of(&home, &id).is_empty(),
        "a Claude Code started for it"
    );
}

// --- the route is kept ---------------------------------------------------------------

#[test]
fn a_resumed_run_keeps_its_account_and_none_is_picked_again() {
    let (home, accounts) = routing_home(Some("connected"), Some("connected"), None);
    std::fs::write(home.root().join("scenario"), "asks").unwrap();
    let id = run_id();
    let pinned = json!({"kind": "task", "prompt": "which?", "mount": {"kind": "no_tools"},
        "account": 2, "needs_you_when":
            {"pointer": "/status", "equals": "needs_you", "question_pointer": "/question"}});
    assert_eq!(start(&home, &pinned, &id).code(), Some(0));
    ended(&home, &id, "needs_you");
    let since = seq(&home);
    let resumed = home.rlxd(&[
        "call",
        "run.resume",
        &json!({"run_id": id, "reply": "#dev"}).to_string(),
    ]);
    assert_eq!(resumed.code(), Some(0), "{resumed:?}");
    ended(&home, &id, "done");
    let events = started(&home, since, &id);
    assert_eq!(
        events[0]["data"]["account"],
        json!({"slot": 2, "by": "pinned"})
    );
    let runs = starts_of(&home, &id);
    assert_eq!(runs.len(), 2, "{runs:#?}");
    for start in &runs {
        on_slot_2(start, &accounts);
    }
    // Slot 2 gone from rlx's list: the run resumes there or nowhere.
    std::fs::write(home.root().join("scenario"), "asks").unwrap();
    let again = run_id();
    assert_eq!(start(&home, &pinned, &again).code(), Some(0));
    ended(&home, &again, "needs_you");
    std::fs::write(home.root().join("scenario"), "mcp-accounts").unwrap();
    accounts.list(false);
    read_map(&home);
    let refused = home.rlxd(&[
        "call",
        "run.resume",
        &json!({"run_id": again, "reply": "#dev"}).to_string(),
    ]);
    assert_eq!(refused.error_code(), "no_such_account", "{refused:?}");
    assert_eq!(starts_of(&home, &again).len(), 1);
}

// --- acceptance 5 --------------------------------------------------------------------

/// An app gate client, connected until dropped: jobs fire only with one (2.3).
struct App<'a> {
    home: &'a TestHome,
    child: Child,
}

impl Drop for App<'_> {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        eventually("the app is gone", || self.home.app_clients() == 0);
    }
}

fn app(home: &TestHome) -> App<'_> {
    let since = seq(home).to_string();
    let child = home.spawn(&[
        "events", "--as-app", "--topics", "daemon", "--since", &since, "--follow",
    ]);
    eventually("the app is connected", || home.app_clients() == 1);
    App { home, child }
}

/// A run job routed by Slack, every hour.
fn routed_job() -> Value {
    json!({"job_id": "digest", "type": "run", "schedule": {"every_minutes": 60},
        "run": {"kind": "task", "prompt": "digest",
                "mount": {"kind": "connectors", "connectors": [SLACK]},
                "connectors": [SLACK]},
        "catch_up": "skip", "enabled": true})
}

/// The `job` topic's events from `since`, each valid.
fn job_events(home: &TestHome, since: u64) -> Vec<Value> {
    let events = home
        .rlxd(&["events", "--topics", "job", "--since", &since.to_string()])
        .lines();
    for event in &events {
        assert_valid("job-event.json", event);
    }
    events
}

#[test]
fn a_routed_jobs_fire_goes_to_its_account_and_a_limited_sole_holder_is_skipped() {
    let (home, accounts) = routing_home(Some("needs-auth"), Some("connected"), None);
    let put = home.rlxd(&["call", "job.put", &routed_job().to_string()]);
    assert_eq!(put.code(), Some(0), "{put:?}");
    let _app = app(&home);
    let since = seq(&home);
    let fired = home.rlxd(&["call", "job.run_now", r#"{"job_id": "digest"}"#]);
    assert_eq!(fired.code(), Some(0), "{fired:?}");
    let id = fired.data()["run_id"].as_str().unwrap().to_owned();
    ended(&home, &id, "done");
    assert_eq!(
        started(&home, since, &id)[0]["data"]["account"],
        json!({"slot": 2, "by": "connectors"})
    );
    on_slot_2(&starts_of(&home, &id)[0], &accounts);
    assert!(
        job_events(&home, since)
            .iter()
            .any(|e| e["type"] == "job.fired" && e["data"]["run_id"] == id.as_str())
    );
    // Slot 2 limited: the fire is skipped with the limit's reason, and no run starts.
    accounts.usage(&json!([{"slot": 2, "limited": true, "resets_at": RESETS}]));
    let since = seq(&home);
    let starts = fixture_log(&home)["starts"].as_array().unwrap().len();
    let skipped = home.rlxd(&["call", "job.run_now", r#"{"job_id": "digest"}"#]);
    assert_eq!(
        skipped.error_code(),
        "connector_account_limited",
        "{skipped:?}"
    );
    let events = job_events(&home, since);
    let skip: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "job.skipped")
        .collect();
    assert_eq!(skip.len(), 1, "{events:#?}");
    assert_eq!(
        skip[0]["data"],
        json!({"job_id": "digest", "scheduled_for": skip[0]["data"]["scheduled_for"],
            "reason": "connector_account_limited", "connector": SLACK, "account": 2,
            "resets_at": RESETS})
    );
    assert!(
        events.iter().all(|e| e["type"] != "job.fired"),
        "{events:#?}"
    );
    assert_eq!(
        fixture_log(&home)["starts"].as_array().unwrap().len(),
        starts
    );
    let jobs = home.rlxd(&["call", "job.list"]).data();
    assert_eq!(jobs["jobs"][0]["skipped_count"], 1, "{jobs:#}");
}

#[test]
fn a_routed_jobs_due_fire_reads_the_usage_first() {
    // On the test clock: the fire comes due as the clock moves, and its route has the
    // brain's usage from before the pass, so a limited sole holder is skipped.
    let root = TestHome::new().with_env("RLXD_TEST_CLOCK", "2027-01-01T00:00:00Z");
    let accounts = Accounts::of(root.root());
    accounts.set(1, &slack("needs-auth"));
    accounts.set(2, &slack("connected"));
    accounts.list(true);
    accounts.usage(&json!([{"slot": 2, "limited": true, "resets_at": RESETS}]));
    let listed = accounts.listed.to_str().unwrap().to_owned();
    let usage = accounts.usage.to_str().unwrap().to_owned();
    let home = home_with_scenario_file_in(
        root,
        "mcp-accounts",
        &[
            ("RLXD_TEST_ACCOUNTS", listed.as_str()),
            ("RLXD_TEST_USAGE", usage.as_str()),
            ("RLXD_TEST_CONNECTOR_MAP_MS", "off"),
            ("RLXD_TEST_CONNECTORS_NO_TURN", "1"),
            ("RLXD_TEST_CONNECTOR_ROUTE", "1"),
            ("CLAUDE_CONFIG_DIR", ""),
        ],
    );
    read_map(&home);
    std::fs::write(home.root().join("scenario"), "init-connectors").unwrap();
    let put = home.rlxd(&["call", "job.put", &routed_job().to_string()]);
    assert_eq!(put.code(), Some(0), "{put:?}");
    let _app = app(&home);
    let since = seq(&home);
    let moved = home.rlxd(&["call", "debug.clock_advance", r#"{"seconds": 3600}"#]);
    assert_eq!(moved.code(), Some(0), "{moved:?}");
    let events = job_events(&home, since);
    let skip: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "job.skipped")
        .collect();
    assert_eq!(skip.len(), 1, "{events:#?}");
    assert_eq!(skip[0]["data"]["reason"], "connector_account_limited");
    assert_eq!(skip[0]["data"]["account"], 2);
    // With room again, the next due fire goes to slot 2.
    accounts.usage(&json!([]));
    let since = seq(&home);
    let moved = home.rlxd(&["call", "debug.clock_advance", r#"{"seconds": 3600}"#]);
    assert_eq!(moved.code(), Some(0), "{moved:?}");
    let events = job_events(&home, since);
    let fired: Vec<&Value> = events.iter().filter(|e| e["type"] == "job.fired").collect();
    assert_eq!(fired.len(), 1, "{events:#?}");
    let id = fired[0]["data"]["run_id"].as_str().unwrap().to_owned();
    ended(&home, &id, "done");
    on_slot_2(&starts_of(&home, &id)[0], &accounts);
}
