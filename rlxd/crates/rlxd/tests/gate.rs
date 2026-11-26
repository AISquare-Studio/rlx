//! The app gate's credential, pairing and dev mode (handoff 0.10.3 rev 6: 5.1 and 7.2,
//! with the hub's per-device gate of 2026-11-25; task #10's acceptance), each against a
//! fresh home, on every OS rlxd.yml runs.
//!
//! The test client, `rlxd call`, is unsigned, so it can be the gate only in gate dev
//! mode. The peer identity check itself, per OS, is in the gate_peer_* tests.

mod common;

use std::process::{Child, Stdio};

use common::{TestHome, assert_valid, eventually};
use rlxd_client::proto::{HelloParams, Role};
use rlxd_client::{Connection, Home};
use serde_json::{Value, json};

/// One app-role connection, held open.
struct App {
    connection: Connection,
}

impl App {
    /// Says `hello {role: "app"}` with `credential`: the connection, or the refusal.
    fn connect(home: &TestHome, credential: &str) -> Result<Self, Value> {
        let mut connection = Connection::open(&Home::at(home.root())).unwrap();
        let hello = HelloParams::new(home.token(), "gate-test", "0", Role::App)
            .unwrap()
            .with_gate_credential(credential)
            .unwrap();
        let reply = connection.hello(&hello).unwrap();
        let envelope: Value = serde_json::from_str(&reply.line).unwrap();
        if reply.is_ok() {
            Ok(Self { connection })
        } else {
            Err(envelope)
        }
    }

    /// One request on this connection, and its reply envelope.
    fn call(&mut self, method: &str, params: Value) -> Value {
        let Value::Object(params) = params else {
            panic!("params must be an object")
        };
        let reply = self.connection.request(method, params).unwrap();
        serde_json::from_str(&reply.line).unwrap()
    }

    /// Whether rlxd dropped this connection from the gate: a last `not_gate` envelope,
    /// then the connection closes.
    fn was_dropped(&mut self) -> bool {
        let Ok(Some(line)) = self.connection.read_line() else {
            return false;
        };
        let envelope: Value = serde_json::from_str(&line).unwrap();
        envelope["error"]["code"] == "not_gate" && matches!(self.connection.read_line(), Ok(None))
    }
}

/// `rlxd call --as-app status`, the first of which pairs in dev mode: the credential it
/// kept.
fn pair_test_client(home: &TestHome) -> String {
    let run = home.rlxd(&["call", "--as-app", "status"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    home.test_credential()
        .expect("the first --as-app call kept its credential")
}

/// `rlxd call gate.pair params` started in the background: a new device's pairing,
/// which waits for an approval.
fn pair_in_background(home: &TestHome, params: &str) -> Child {
    home.command(&["call", "gate.pair", params])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// What a background `rlxd call` printed and how it exited.
fn finish(child: Child) -> (Option<i32>, Value) {
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let envelope = serde_json::from_str(text.trim())
        .unwrap_or_else(|e| panic!("not one envelope ({e}): {text}"));
    (out.status.code(), envelope)
}

/// Waits until a pairing request for `device` waits.
fn until_requested(home: &TestHome, device: &str) {
    eventually(&format!("a pairing request for {device}"), || {
        let run = home.rlxd(&["call", "gate.pair_requests"]);
        run.code() == Some(0)
            && run.data()["requests"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["device"] == device)
    });
}

fn devices(home: &TestHome) -> Vec<Value> {
    let run = home.rlxd(&["gate", "devices"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    run.data()["devices"].as_array().unwrap().clone()
}

fn count(log: &str, said: &str) -> usize {
    log.lines().filter(|line| line.contains(said)).count()
}

#[test]
fn only_the_app_can_take_the_gate_role() {
    // 5.1 Accept, "Only the app can take the gate role", in dev mode, after a pairing:
    // the credential is required whatever the mode.
    let home = TestHome::new();
    home.start_dev();
    pair_test_client(&home);
    for args in [
        &["call", "--role", "app", "status"][..],
        &["call", "--role", "app", "--client", "rlx-tui", "status"],
        &[
            "call",
            "--role",
            "app",
            "--gate-credential",
            "wrong",
            "status",
        ],
        &[
            "call",
            "--role",
            "app",
            "--gate-credential",
            "wrong",
            "run.decide",
            r#"{"run_id":"r","call_id":"c","decision":"allow","reason":"x"}"#,
        ],
    ] {
        let run = home.rlxd(args);
        assert_eq!(run.code(), Some(1), "{args:?}: {run:?}");
        assert_eq!(run.error_code(), "not_gate", "{args:?}");
        assert_valid("envelope.json", &run.json());
    }
    assert_eq!(home.app_clients(), 0);
}

#[test]
fn as_app_without_a_credential_is_not_gate_and_with_one_it_is_counted() {
    // Task acceptance 2.
    let home = TestHome::new();
    home.start_dev();
    let run = home.rlxd(&["call", "--role", "app", "status"]);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_eq!(run.error_code(), "not_gate");
    let run = home.rlxd(&["call", "--as-app", "status"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_eq!(
        run.data()["clients"]["app"],
        1,
        "the asking app counts itself"
    );
    let credential = home.test_credential().unwrap();
    let _app = App::connect(&home, &credential).unwrap();
    assert_eq!(home.app_clients(), 1);
}

#[test]
fn outside_dev_mode_an_unsigned_client_is_refused_even_with_a_dev_mode_credential() {
    // 5.1 Accept, "Dev mode off".
    let home = TestHome::new();
    home.start_dev();
    let credential = pair_test_client(&home);
    home.stop();
    home.start();
    assert_eq!(
        home.rlxd(&["status", "--json"]).data()["gate"],
        "signed app only"
    );
    let before = std::fs::read(home.gate_json()).unwrap();
    let pair_again = format!(r#"{{"gate_credential":"{credential}"}}"#);
    for args in [
        &["call", "--as-app", "status"][..],
        &["call", "gate.pair"],
        &["call", "gate.pair", &pair_again],
        &["call", "gate.pair", r#"{"device":"another"}"#],
    ] {
        let run = home.rlxd(args);
        assert_eq!(run.code(), Some(1), "{args:?}: {run:?}");
        assert_eq!(run.error_code(), "not_gate", "{args:?}");
    }
    assert_eq!(std::fs::read(home.gate_json()).unwrap(), before);
    assert_eq!(home.app_clients(), 0);
    let log = home.log();
    assert!(
        log.contains("hello refused: role app"),
        "the refusal is logged:\n{log}"
    );
}

#[test]
fn in_dev_mode_status_says_so_and_the_log_records_each_gate_hello() {
    // 5.1 Accept, "Dev mode on".
    let home = TestHome::new();
    home.start_dev();
    let run = home.rlxd(&["status", "--json"]);
    assert_eq!(run.data()["gate"], "dev mode (unsigned app accepted)");
    assert_valid("status-reply.json", &run.data());
    let human = home.rlxd(&["status"]);
    assert!(
        human
            .stdout
            .contains("gate: dev mode (unsigned app accepted)"),
        "{human:?}"
    );
    pair_test_client(&home);
    let run = home.rlxd(&["call", "--as-app", "status"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let run = home.rlxd(&[
        "call",
        "--role",
        "app",
        "--gate-credential",
        "wrong",
        "status",
    ]);
    assert_eq!(run.error_code(), "not_gate");
    let log = home.log();
    let hellos = count(&log, "hello: rlxd-cli");
    let app_hellos = 2;
    assert!(
        count(&log, "gate dev mode") > app_hellos,
        "one line at start, plus one per gate hello ({hellos} hellos):\n{log}"
    );
    assert_eq!(count(&log, "gate dev mode is ON"), 1, "{log}");
    assert_eq!(
        count(&log, "gate dev mode: hello role app"),
        app_hellos,
        "{log}"
    );
}

#[test]
fn gate_dev_mode_in_the_config_is_dev_mode_and_nothing_else_there_moves_the_gate() {
    let home = TestHome::new();
    let rlxd = home.root().join("rlxd");
    std::fs::create_dir_all(&rlxd).unwrap();
    std::fs::write(
        rlxd.join("config.toml"),
        "[gate]\ndev_mode = true\nrequirement = 'identifier \"anything\"'\n",
    )
    .unwrap();
    home.start();
    assert_eq!(
        home.status()["gate"],
        "dev mode (unsigned app accepted)",
        "the config's gate.dev_mode"
    );
    assert!(
        home.log().contains("ignoring gate.requirement"),
        "{}",
        home.log()
    );
    home.stop();
    std::fs::write(rlxd.join("config.toml"), "[gate]\ndev_mode = 3\n").unwrap();
    let run = home.rlxd(&["serve", "--detach"]);
    assert_eq!(
        run.code(),
        Some(1),
        "a broken config fails the start: {run:?}"
    );
    assert_eq!(run.error_code(), "start_failed");
}

#[test]
fn pairing_is_trust_on_first_use_in_dev_mode_too() {
    // 5.1 Accept, "Pairing is trust on first use, in dev mode too", and task acceptance
    // 3 (i)-(iii).
    let home = TestHome::new();
    home.start_dev();
    let c1 = pair_test_client(&home);
    let g1 = std::fs::read(home.gate_json()).unwrap();
    let text = String::from_utf8_lossy(&g1).into_owned();
    assert!(!text.contains(&c1), "gate.json keeps only the hash");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(home.gate_json())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
    let mut held = App::connect(&home, &c1).unwrap();
    assert_eq!(home.app_clients(), 1);

    // No second first pairing.
    for args in [
        &["call", "gate.pair"][..],
        &["call", "gate.pair", r#"{"gate_credential":"wrong"}"#],
    ] {
        let run = home.rlxd(args);
        assert_eq!(run.code(), Some(1), "{args:?}: {run:?}");
        assert_eq!(run.error_code(), "not_gate", "{args:?}");
    }
    assert_eq!(std::fs::read(home.gate_json()).unwrap(), g1);
    assert_eq!(home.app_clients(), 1);
    assert_eq!(home.rlxd(&["call", "--as-app", "status"]).code(), Some(0));

    // A replaced credential's connections drop.
    let run = home.rlxd(&[
        "call",
        "gate.pair",
        &format!(r#"{{"gate_credential":"{c1}"}}"#),
    ]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let c2 = run.data()["gate_credential"].as_str().unwrap().to_owned();
    assert_ne!(c2, c1);
    assert_eq!(run.data()["device"], "default");
    assert_eq!(run.data()["route"], "local");
    assert_eq!(home.app_clients(), 0);
    assert!(held.was_dropped(), "the held connection was dropped");
    let old = home.rlxd(&["call", "--role", "app", "--gate-credential", &c1, "status"]);
    assert_eq!(old.error_code(), "not_gate");
    let new = home.rlxd(&["call", "--role", "app", "--gate-credential", &c2, "status"]);
    assert_eq!(new.code(), Some(0), "{new:?}");

    // Unpair.
    std::fs::write(home.root().join("test-gate-credential"), &c2).unwrap();
    let mut held = App::connect(&home, &c2).unwrap();
    assert_eq!(home.app_clients(), 1);
    let run = home.rlxd(&["gate", "unpair"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_eq!(run.data()["unpaired"], json!(["default"]));
    assert!(!home.gate_json().exists());
    assert_eq!(home.app_clients(), 0);
    assert!(held.was_dropped());
    assert_eq!(count(&home.log(), "gate unpaired"), 1, "{}", home.log());
    let run = home.rlxd(&["call", "--as-app", "status"]);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_eq!(run.error_code(), "not_gate");
    assert!(
        !home.gate_json().exists(),
        "the test client did not pair again"
    );
    let run = home.rlxd(&["call", "gate.pair"]);
    assert_eq!(run.code(), Some(0), "a first pairing again: {run:?}");
    assert!(!run.data()["gate_credential"].as_str().unwrap().is_empty());
}

#[test]
fn an_app_re_pairing_itself_gets_its_reply_and_then_its_connection_drops() {
    // 5.1: "the others before the reply, and the caller's own right after it".
    let home = TestHome::new();
    home.start_dev();
    let c1 = pair_test_client(&home);
    let mut other = App::connect(&home, &c1).unwrap();
    let mut caller = App::connect(&home, &c1).unwrap();
    let reply = caller.call("gate.pair", json!({"gate_credential": c1}));
    assert_eq!(reply["ok"], true, "{reply}");
    assert!(other.was_dropped());
    assert!(
        caller.was_dropped(),
        "the caller's own goes after its reply"
    );
    assert_eq!(home.app_clients(), 0);
    let c2 = reply["data"]["gate_credential"].as_str().unwrap();
    assert!(App::connect(&home, c2).is_ok());
}

#[test]
fn two_devices_pair_and_unpairing_one_leaves_the_other_working() {
    // Task acceptance 3: per device.
    let home = TestHome::new();
    home.start_dev();
    let run = home.rlxd(&["call", "gate.pair", r#"{"device":"laptop"}"#]);
    assert_eq!(run.code(), Some(0), "the first device: {run:?}");
    let laptop = run.data()["gate_credential"].as_str().unwrap().to_owned();

    // A second local device waits for an approval; the user on the host approves it.
    let waiting = pair_in_background(&home, r#"{"device":"desktop"}"#);
    until_requested(&home, "desktop");
    let run = home.rlxd(&["gate", "pair", "--device", "desktop"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_eq!(run.data()["approved"], true);
    let (code, envelope) = finish(waiting);
    assert_eq!(code, Some(0), "{envelope}");
    let desktop = envelope["data"]["gate_credential"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(desktop, laptop);

    let listed = devices(&home);
    let names: Vec<&str> = listed
        .iter()
        .map(|d| d["device"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["laptop", "desktop"]);
    assert!(listed.iter().all(|d| d["route"] == "local"));
    assert!(
        listed.iter().all(|d| d["last_seen"].is_null()),
        "{listed:?}"
    );

    let mut on_laptop = App::connect(&home, &laptop).unwrap();
    let mut on_desktop = App::connect(&home, &desktop).unwrap();
    assert_eq!(home.app_clients(), 2);
    let listed = devices(&home);
    assert!(
        listed
            .iter()
            .all(|d| d["last_seen"].is_string() && d["connections"] == 1),
        "{listed:?}"
    );

    // Re-pairing one device drops only that device's connections.
    let reply = on_laptop.call("gate.pair", json!({"gate_credential": laptop}));
    assert_eq!(reply["ok"], true, "{reply}");
    assert!(on_laptop.was_dropped());
    let status = on_desktop.call("status", json!({}));
    assert_eq!(status["data"]["clients"]["app"], 1, "{status}");
    let laptop = reply["data"]["gate_credential"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut on_laptop = App::connect(&home, &laptop).unwrap();

    // Unpairing one leaves the other.
    let run = home.rlxd(&["gate", "unpair", "desktop"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_eq!(run.data()["dropped"], 1);
    assert!(on_desktop.was_dropped());
    assert_eq!(on_laptop.call("status", json!({}))["ok"], true);
    assert_eq!(home.app_clients(), 1);
    let names: Vec<String> = devices(&home)
        .iter()
        .map(|d| d["device"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["laptop"]);
    assert!(App::connect(&home, &desktop).is_err());
    let run = home.rlxd(&["gate", "unpair", "desktop"]);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_eq!(run.error_code(), "unknown_device");
}

#[test]
fn a_paired_app_approves_or_the_user_denies_a_new_device() {
    let home = TestHome::new();
    home.start_dev();
    let first = pair_test_client(&home);
    let mut app = App::connect(&home, &first).unwrap();

    let waiting = pair_in_background(&home, r#"{"device":"phone"}"#);
    until_requested(&home, "phone");
    let requests = app.call("gate.pair_requests", json!({}));
    let request = &requests["data"]["requests"][0];
    assert_eq!(request["device"], "phone", "{requests}");
    assert_eq!(request["route"], "local");
    let id = request["request_id"].as_str().unwrap().to_owned();
    let answer = app.call(
        "gate.pair_answer",
        json!({"request_id": id, "approve": true}),
    );
    assert_eq!(answer["ok"], true, "{answer}");
    let (code, envelope) = finish(waiting);
    assert_eq!(code, Some(0), "{envelope}");
    assert_eq!(envelope["data"]["device"], "phone");
    assert!(home.log().contains("approved by device \"default\""));

    let waiting = pair_in_background(&home, r#"{"device":"tablet"}"#);
    until_requested(&home, "tablet");
    let run = home.rlxd(&["gate", "pair", "--device", "tablet", "--deny"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let (code, envelope) = finish(waiting);
    assert_eq!(code, Some(1), "{envelope}");
    assert_eq!(envelope["error"]["code"], "not_gate");
    assert_eq!(devices(&home).len(), 2, "default and phone");

    let run = home.rlxd(&["gate", "pair"]);
    assert_eq!(run.code(), Some(1), "nothing waits: {run:?}");
    assert_eq!(run.error_code(), "no_pair_request");
    let run = home.rlxd(&[
        "call",
        "--role",
        "remote",
        "gate.pair_answer",
        r#"{"approve":true}"#,
    ]);
    assert_eq!(run.error_code(), "not_gate", "a remote client: {run:?}");
    let run = home.rlxd(&["call", "--role", "remote", "gate.unpair"]);
    assert_eq!(run.error_code(), "not_gate", "a remote client: {run:?}");
    let unpair = app.call("gate.unpair", json!({}));
    assert_eq!(
        unpair["error"]["code"], "not_gate",
        "the app never unpairs: {unpair}"
    );
}

#[test]
fn a_connection_to_the_socket_never_reaches_the_ssh_route() {
    // Task acceptance 3: "a socket-only connection cannot reach gate.pair's ssh route".
    let home = TestHome::new();
    home.start_dev();
    for params in [
        r#"{"device":"remote-box","route":"ssh"}"#,
        r#"{"route":"ssh"}"#,
    ] {
        let run = home.rlxd(&["call", "gate.pair", params]);
        assert_eq!(run.code(), Some(1), "{params}: {run:?}");
        assert_eq!(run.error_code(), "not_gate", "{params}");
    }
    assert!(!home.gate_json().exists(), "nothing was paired");
    pair_test_client(&home);
    let run = home.rlxd(&[
        "call",
        "gate.pair",
        r#"{"device":"remote-box","route":"ssh"}"#,
    ]);
    assert_eq!(run.error_code(), "not_gate");
    assert!(devices(&home).iter().all(|d| d["route"] == "local"));
}

#[test]
fn with_rlxd_stopped_devices_are_listed_and_unpaired_from_gate_json() {
    let home = TestHome::new();
    home.start_dev();
    pair_test_client(&home);
    home.stop();
    let listed = devices(&home);
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0]["connections"], 0);
    assert!(
        listed[0]["last_seen"].is_string(),
        "the stop wrote when it was last seen: {listed:?}"
    );
    let run = home.rlxd(&["gate", "unpair", "nope"]);
    assert_eq!(run.error_code(), "unknown_device");
    let run = home.rlxd(&["gate", "unpair", "default"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert!(!home.gate_json().exists());
    assert_eq!(count(&home.log(), "gate unpaired"), 1, "{}", home.log());
}

#[test]
fn a_broken_gate_json_fails_the_start_until_the_user_unpairs() {
    // rlxd never reads a broken gate.json as "nothing paired", which would let anyone
    // pair first.
    let home = TestHome::new();
    home.start();
    home.stop();
    std::fs::write(home.gate_json(), "{broken").unwrap();
    let run = home.rlxd(&["serve", "--detach"]);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_eq!(run.error_code(), "start_failed");
    let run = home.rlxd(&["gate", "unpair"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert!(!home.gate_json().exists());
    home.start();
}

#[test]
fn a_dev_mode_start_keeps_its_flag_in_the_registry_for_a_restart() {
    let home = TestHome::new();
    home.start_dev();
    let dir = Home::at(home.root()).daemons();
    let record = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|x| x == "json"))
        .unwrap();
    let record: Value = serde_json::from_str(&std::fs::read_to_string(record).unwrap()).unwrap();
    let argv: Vec<&str> = record["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert_eq!(argv[1..], ["serve", "--detach", "--gate-dev-mode"]);
}
