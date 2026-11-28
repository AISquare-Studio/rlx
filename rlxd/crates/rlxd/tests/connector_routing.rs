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
use common::{TestHome, assert_valid, eventually};
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
    usage: PathBuf,
    listed: PathBuf,
}

impl Accounts {
    fn of(root: &Path) -> Self {
        Self {
            plain: root.join("user-home").join(".claude"),
            managed: root.join("account-2"),
            managed_tmp: root.join("account-2-tmp"),
            usage: root.join("usage.json"),
            listed: root.join("accounts.json"),
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
    fn list(&self, two: bool) {
        let mut listed = vec![json!({"slot": 1, "label": "main", "login": MARK_1})];
        if two {
            listed.push(json!({"slot": 2, "label": "work",
                "config_dir": self.managed, "tmp_dir": self.managed_tmp, "login": MARK_2}));
        }
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
    let home = home_with_scenario_file_in(
        root,
        "mcp-accounts",
        &[
            ("RLXD_TEST_ACCOUNTS", listed.as_str()),
            ("RLXD_TEST_USAGE", usage.as_str()),
            ("RLXD_TEST_CONNECTOR_MAP_MS", "off"),
            ("RLXD_TEST_CONNECTORS_NO_TURN", "1"),
            // Route 1: a try whose init shows Slack connected sends its prompt at once.
            ("RLXD_TEST_CONNECTOR_ROUTE", "1"),
            ("CLAUDE_CONFIG_DIR", ""),
        ],
    );
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
    // Its needs-you for the sign-in is out, and Slack signed in again on slot 2, its own
    // account, clears it. (The runs' own home is slot 1's: that is not where it ran.)
    let since = seq(&home);
    std::fs::write(home.root().join("scenario"), "mcp-accounts").unwrap();
    accounts.set(2, &slack("connected"));
    read_map(&home);
    eventually("the run's needs-you is cleared", || {
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
    });
}

#[test]
fn erasing_a_space_takes_a_routed_runs_session_from_its_accounts_home() {
    let (home, accounts) = routing_home(Some("needs-auth"), Some("connected"), None);
    let id = run_id();
    assert_eq!(start(&home, &task(&[SLACK], None), &id).code(), Some(0));
    ended(&home, &id, "done");
    let sessions = |dir: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(dir.join("projects"))
            .into_iter()
            .flatten()
            .filter_map(|entry| Some(entry.ok()?.path().join(format!("{id}.jsonl"))))
            .filter(|path| path.is_file())
            .collect()
    };
    assert_eq!(sessions(&accounts.managed).len(), 1, "no session on slot 2");
    let erased = home.rlxd(&["call", "space.erase", r#"{"name": "default"}"#]);
    assert_eq!(erased.code(), Some(0), "{erased:?}");
    assert!(
        sessions(&accounts.managed).is_empty(),
        "the session is left"
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
