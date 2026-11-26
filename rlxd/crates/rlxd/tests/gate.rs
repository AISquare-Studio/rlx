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
use rlxd_client::{Connection, Home, Incoming};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

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
/// Waits until a pairing request for `device` waits, as `rlxd gate pair` lists it: the
/// request.
fn until_requested(home: &TestHome, device: &str) -> Value {
    let mut found = Value::Null;
    eventually(&format!("a pairing request for {device}"), || {
        let run = home.rlxd(&["gate", "pair"]);
        if run.code() != Some(0) {
            return false;
        }
        let requests = run.data()["requests"].as_array().unwrap().clone();
        match requests.into_iter().find(|r| r["device"] == device) {
            Some(request) => {
                found = request;
                true
            }
            None => false,
        }
    });
    found
}

/// The id of a waiting request.
fn id(request: &Value) -> String {
    request["request_id"].as_str().unwrap().to_owned()
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
    let request = until_requested(&home, "desktop");
    assert_eq!(request["route"], "local");
    assert!(
        request["peer"]["pid"].as_u64().is_some_and(|pid| pid > 0),
        "{request}"
    );
    assert!(
        request["peer"]["image"]
            .as_str()
            .is_some_and(|image| image.contains("rlxd")),
        "the asking process's binary: {request}"
    );
    assert_eq!(request["revoked"], false);
    let run = home.rlxd(&["gate", "pair", "--request-id", &id(&request)]);
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
    let phone = request["request_id"].as_str().unwrap().to_owned();
    let answer = app.call(
        "gate.pair_answer",
        json!({"request_id": phone, "approve": true}),
    );
    assert_eq!(answer["ok"], true, "{answer}");
    let (code, envelope) = finish(waiting);
    assert_eq!(code, Some(0), "{envelope}");
    assert_eq!(envelope["data"]["device"], "phone");
    assert!(home.log().contains("approved by device \"default\""));

    let waiting = pair_in_background(&home, r#"{"device":"tablet"}"#);
    let request = until_requested(&home, "tablet");
    let run = home.rlxd(&["gate", "pair", "--request-id", &id(&request), "--deny"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let (code, envelope) = finish(waiting);
    assert_eq!(code, Some(1), "{envelope}");
    assert_eq!(envelope["error"]["code"], "not_gate");
    assert_eq!(devices(&home).len(), 2, "default and phone");

    let run = home.rlxd(&["gate", "pair"]);
    assert_eq!(run.code(), Some(0), "nothing waits: {run:?}");
    assert_eq!(
        run.data()["requests"],
        json!([]),
        "it lists, and approves nothing"
    );
    let run = home.rlxd(&["gate", "pair", "--request-id", "no-such-request"]);
    assert_eq!(run.code(), Some(1), "{run:?}");
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

/// Writes a gate.json holding one device, `route`, whose credential is `credential`: as
/// any process of the user's could.
fn plant(home: &TestHome, route: &str, credential: &str) {
    let sha256: String = Sha256::digest(credential.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let gate = json!({"version": 1, "devices": [{
        "device": "planted", "route": route, "sha256": sha256,
        "created": "2026-11-26T00:00:00.000Z",
    }]});
    std::fs::create_dir_all(home.root().join("rlxd")).unwrap();
    std::fs::write(home.gate_json(), gate.to_string()).unwrap();
}

#[test]
fn a_planted_ssh_route_device_is_refused_until_ssh_pairing_lands() {
    // Review #1991, major 1: SSH pairing isn't built, so rlxd never minted an ssh-route
    // credential; one in gate.json is refused, outside dev mode and in it, and logged.
    for dev_mode in [false, true] {
        let home = TestHome::new();
        plant(&home, "ssh", "planted-credential");
        if dev_mode {
            home.start_dev();
        } else {
            home.start();
        }
        let hello = [
            "call",
            "--role",
            "app",
            "--gate-credential",
            "planted-credential",
            "status",
        ];
        let run = home.rlxd(&hello);
        assert_eq!(run.code(), Some(1), "dev mode {dev_mode}: {run:?}");
        assert_eq!(run.error_code(), "not_gate");
        let run = home.rlxd(&[
            "call",
            "gate.pair",
            r#"{"gate_credential":"planted-credential"}"#,
        ]);
        assert_eq!(run.error_code(), "not_gate", "nor re-paired: {run:?}");
        assert_eq!(home.app_clients(), 0);
        let log = home.log();
        // At load and at the hello; and at the re-pairing in dev mode, where no peer
        // check refuses this unsigned client first.
        assert_eq!(
            count(&log, "has route ssh, which this rlxd refuses"),
            if dev_mode { 3 } else { 2 },
            "{log}"
        );
        assert!(
            log.lines()
                .any(|l| l.contains("hello refused: role app") && l.contains("has route ssh")),
            "{log}"
        );
        assert_eq!(devices(&home)[0]["route"], "ssh", "the user still sees it");
        let run = home.rlxd(&["gate", "unpair", "planted"]);
        assert_eq!(run.code(), Some(0), "and can remove it: {run:?}");
    }
    // The control: the same entry with route local passes in dev mode, where only the
    // credential is checked.
    let home = TestHome::new();
    plant(&home, "local", "planted-credential");
    home.start_dev();
    let run = home.rlxd(&[
        "call",
        "--role",
        "app",
        "--gate-credential",
        "planted-credential",
        "status",
    ]);
    assert_eq!(run.code(), Some(0), "{run:?}");
}

#[test]
fn an_empty_gate_credential_is_not_gate() {
    // Review #1991, item 13.
    let home = TestHome::new();
    home.start_dev();
    pair_test_client(&home);
    let run = home.rlxd(&["call", "--role", "app", "--gate-credential", "", "status"]);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_eq!(run.error_code(), "not_gate");
    assert_eq!(home.app_clients(), 0);
}

#[test]
fn a_requester_that_leaves_withdraws_its_request_and_nothing_is_paired() {
    // Review #1991, item 7, and the tester's finding 1.
    let home = TestHome::new();
    home.start_dev();
    pair_test_client(&home);
    let mut waiting = pair_in_background(&home, r#"{"device":"orphan"}"#);
    let request = until_requested(&home, "orphan");
    waiting.kill().unwrap();
    waiting.wait().unwrap();
    eventually("the request is withdrawn", || {
        home.rlxd(&["gate", "pair"]).data()["requests"] == json!([])
    });
    let run = home.rlxd(&["gate", "pair", "--request-id", &id(&request)]);
    assert_eq!(run.error_code(), "no_pair_request", "{run:?}");
    assert_eq!(devices(&home).len(), 1, "no orphan device");
    assert!(
        home.log()
            .contains("a client left before its request was answered")
    );
}

#[test]
fn approving_a_revoked_devices_name_needs_force() {
    // Review #1991, item 6: no request can pass itself off as a device the user revoked.
    let home = TestHome::new();
    home.start_dev();
    pair_test_client(&home);
    let waiting = pair_in_background(&home, r#"{"device":"laptop"}"#);
    let request = until_requested(&home, "laptop");
    assert_eq!(
        home.rlxd(&["gate", "pair", "--request-id", &id(&request)])
            .code(),
        Some(0)
    );
    assert_eq!(finish(waiting).0, Some(0));
    assert_eq!(home.rlxd(&["gate", "unpair", "laptop"]).code(), Some(0));

    let waiting = pair_in_background(&home, r#"{"device":"laptop"}"#);
    let request = until_requested(&home, "laptop");
    assert_eq!(request["revoked"], true, "{request}");
    let run = home.rlxd(&["gate", "pair", "--request-id", &id(&request)]);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_eq!(run.error_code(), "revoked_device");
    let run = home.rlxd(&["gate", "pair", "--request-id", &id(&request), "--force"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let (code, envelope) = finish(waiting);
    assert_eq!(code, Some(0), "{envelope}");
    assert_eq!(envelope["data"]["device"], "laptop");
}

#[test]
fn a_kicked_connection_aborts_the_request_it_is_in() {
    // Review #1991, item 10: an app connection waiting on a new device's pairing is
    // unpaired meanwhile; its request ends with the kick, and is withdrawn.
    let home = TestHome::new();
    home.start_dev();
    let credential = pair_test_client(&home);
    let mut app = App::connect(&home, &credential).unwrap();
    let asking = std::thread::spawn(move || app.call("gate.pair", json!({"device": "newdev"})));
    until_requested(&home, "newdev");
    assert_eq!(home.rlxd(&["gate", "unpair", "default"]).code(), Some(0));
    let reply = asking.join().unwrap();
    assert_eq!(reply["error"]["code"], "not_gate", "{reply}");
    assert!(
        reply.get("id").is_none(),
        "the drop's notice, not a reply: {reply}"
    );
    eventually("the request is withdrawn", || {
        home.rlxd(&["gate", "pair"]).data()["requests"] == json!([])
    });
}

/// Sends `hello` and one request on a new connection, half-closes it (shuts the sending
/// side), and reads every reply until rlxd closes: the request's reply.
#[cfg(unix)]
fn ask_half_closed(home: &TestHome, method: &str, params: Value) -> Value {
    use std::io::{BufRead, BufReader, Write};
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;
    let mut stream = UnixStream::connect(home.root().join("run").join("rlxd.sock")).unwrap();
    let hello = json!({"id": 1, "method": "hello", "params": {
        "token": home.token(), "protocol_version": 1, "client": "half-close",
        "client_version": "0", "role": "cli", "capabilities": [],
    }});
    let request = json!({"id": 2, "method": method, "params": params});
    stream
        .write_all(format!("{hello}\n{request}\n").as_bytes())
        .unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    let lines: Vec<String> = BufReader::new(stream).lines().map(Result::unwrap).collect();
    assert_eq!(
        lines.len(),
        2,
        "{method}: hello's reply, then its own: {lines:?}"
    );
    serde_json::from_str(&lines[1]).unwrap()
}

#[cfg(unix)]
#[test]
fn a_client_that_half_closes_still_gets_its_last_reply() {
    // Review #2031, N2: every method class, each asked several times, as races showed.
    let home = TestHome::new();
    home.start_dev();
    for _ in 0..10 {
        assert_eq!(ask_half_closed(&home, "status", json!({}))["ok"], true);
        assert_eq!(
            ask_half_closed(&home, "gate.devices", json!({}))["ok"],
            true
        );
        assert_eq!(
            ask_half_closed(&home, "gate.pair_requests", json!({}))["ok"],
            true
        );
        let answer = json!({"request_id": "none", "approve": true});
        let reply = ask_half_closed(&home, "gate.pair_answer", answer);
        assert_eq!(reply["error"]["code"], "no_pair_request", "{reply}");
        let reply = ask_half_closed(&home, "no.such.method", json!({}));
        assert_eq!(reply["error"]["code"], "unknown_method", "{reply}");
    }
    // A pairing that mints, a refused one, and an unpair.
    let reply = ask_half_closed(&home, "gate.pair", json!({}));
    assert_eq!(reply["ok"], true, "{reply}");
    assert!(
        home.gate_json().exists(),
        "a delivered credential stays paired"
    );
    let reply = ask_half_closed(&home, "gate.pair", json!({}));
    assert_eq!(reply["error"]["code"], "not_gate", "{reply}");
    // A new device's pairing waits for the approval, half-closed, and gets its credential.
    let asking = {
        let root = home.root().to_path_buf();
        let token = home.token();
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader, Write};
            use std::net::Shutdown;
            use std::os::unix::net::UnixStream;
            let mut stream = UnixStream::connect(root.join("run").join("rlxd.sock")).unwrap();
            let hello = json!({"id": 1, "method": "hello", "params": {
                "token": token, "protocol_version": 1, "client": "half-close",
                "client_version": "0", "role": "cli", "capabilities": [],
            }});
            let request = json!({"id": 2, "method": "gate.pair", "params": {"device": "later"}});
            stream
                .write_all(format!("{hello}\n{request}\n").as_bytes())
                .unwrap();
            stream.shutdown(Shutdown::Write).unwrap();
            let lines: Vec<String> = BufReader::new(stream).lines().map(Result::unwrap).collect();
            serde_json::from_str::<Value>(lines.last().unwrap()).unwrap()
        })
    };
    let request = until_requested(&home, "later");
    let run = home.rlxd(&["gate", "pair", "--request-id", &id(&request)]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let reply = asking.join().unwrap();
    assert_eq!(reply["data"]["device"], "later", "{reply}");
    assert_eq!(devices(&home).len(), 2);
    let reply = ask_half_closed(&home, "gate.unpair", json!({"device": "later"}));
    assert_eq!(reply["ok"], true, "{reply}");
    let reply = ask_half_closed(&home, "stop", json!({}));
    assert_eq!(reply["ok"], true, "the stop too: {reply}");
}

/// How many bytes of replies a client must leave unread for rlxd's end of the socket to
/// stop being writable, while a reply that size is still written whole: half of what a
/// write may queue on Linux (writable up to a quarter), and on macOS what it may queue
/// less 1 KB (writable while 2 KB are free).
#[cfg(unix)]
fn unwritable_but_not_blocked() -> usize {
    let read = |program: &str, args: &[&str]| -> usize {
        let out = std::process::Command::new(program)
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
    };
    if cfg!(target_os = "macos") {
        read("sysctl", &["-n", "net.local.stream.sendspace"]) - 1024
    } else {
        read("cat", &["/proc/sys/net/core/wmem_default"]) / 2
    }
}

#[cfg(unix)]
#[test]
fn a_half_closed_client_with_unread_replies_keeps_its_waiting_request() {
    // Review #2068, r3 minor 1: replies the client has not read yet leave rlxd's end of
    // the socket unwritable. That is not a client gone, so its gate.pair still waits.
    use std::io::{BufRead, BufReader, Write};
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;
    let home = TestHome::new();
    home.start_dev();
    pair_test_client(&home);
    let mut stream = UnixStream::connect(home.root().join("run").join("rlxd.sock")).unwrap();
    let hello = json!({"id": 1, "method": "hello", "params": {
        "token": home.token(), "protocol_version": 1, "client": "unread",
        "client_version": "0", "role": "cli", "capabilities": [],
    }});
    // hello's reply and an unknown_method's, which says the method's name, are about
    // 400 bytes besides that name.
    let name = "x".repeat(unwritable_but_not_blocked() - 400);
    let filler = json!({"id": 2, "method": name});
    let pair = json!({"id": 3, "method": "gate.pair", "params": {"device": "unread"}});
    stream
        .write_all(format!("{hello}\n{filler}\n{pair}\n").as_bytes())
        .unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    let request = until_requested(&home, "unread");
    // A client gone is noticed within 250 ms: give it time to be wrongly noticed.
    std::thread::sleep(std::time::Duration::from_secs(1));
    let waiting = home.rlxd(&["gate", "pair"]).data()["requests"].clone();
    assert_eq!(waiting.as_array().map(Vec::len), Some(1), "{waiting}");
    let run = home.rlxd(&["gate", "pair", "--request-id", &id(&request)]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let lines: Vec<String> = BufReader::new(stream).lines().map(Result::unwrap).collect();
    assert_eq!(
        lines.len(),
        3,
        "hello's reply, the filler's and gate.pair's"
    );
    let filled: Value = serde_json::from_str(&lines[1]).unwrap();
    assert_eq!(filled["error"]["code"], "unknown_method");
    let paired: Value = serde_json::from_str(&lines[2]).unwrap();
    assert_eq!(paired["ok"], true, "{paired}");
    assert_eq!(paired["data"]["device"], "unread");
    assert_eq!(devices(&home).len(), 2);
}

#[cfg(unix)]
#[test]
fn a_client_gone_after_a_half_close_or_with_frames_queued_withdraws_its_request() {
    // Review #2031, N6, and item 7 kept: a client that only half-closed is watched until
    // it is gone, and so is one whose further frames are queued.
    use std::io::Write;
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;
    let home = TestHome::new();
    home.start_dev();
    pair_test_client(&home);
    let hello = json!({"id": 1, "method": "hello", "params": {
        "token": home.token(), "protocol_version": 1, "client": "gone",
        "client_version": "0", "role": "cli", "capabilities": [],
    }});
    for (device, half_close, extra) in [("halfgone", true, 0), ("queued", false, 3)] {
        let mut stream = UnixStream::connect(home.root().join("run").join("rlxd.sock")).unwrap();
        let mut frames = format!(
            "{hello}\n{}\n",
            json!({"id": 2, "method": "gate.pair", "params": {"device": device}})
        );
        for n in 0..extra {
            frames.push_str(&format!("{}\n", json!({"id": 3 + n, "method": "status"})));
        }
        stream.write_all(frames.as_bytes()).unwrap();
        if half_close {
            stream.shutdown(Shutdown::Write).unwrap();
        }
        until_requested(&home, device);
        drop(stream);
        eventually(&format!("{device}'s request is withdrawn"), || {
            home.rlxd(&["gate", "pair"]).data()["requests"] == json!([])
        });
    }
    assert_eq!(devices(&home).len(), 1, "nothing was paired");
}

#[test]
fn look_alike_names_cannot_dodge_the_revoked_name_rule() {
    // Review #2031, N3.
    let home = TestHome::new();
    home.start_dev();
    pair_test_client(&home);
    let waiting = pair_in_background(&home, r#"{"device":"laptop"}"#);
    let request = until_requested(&home, "laptop");
    assert_eq!(
        home.rlxd(&["gate", "pair", "--request-id", &id(&request)])
            .code(),
        Some(0)
    );
    assert_eq!(finish(waiting).0, Some(0));
    assert_eq!(home.rlxd(&["gate", "unpair", "laptop"]).code(), Some(0));

    // Refused outright: a zero-width space, a bidi control, Latin mixed with Cyrillic,
    // and punctuation outside the set.
    for name in [
        "lap\u{200B}top",
        "lap\u{202E}top",
        "l\u{0430}ptop",
        "laptop!",
    ] {
        let params = json!({"device": name}).to_string();
        let run = home.rlxd(&["call", "gate.pair", &params]);
        assert_eq!(run.code(), Some(1), "{name:?}: {run:?}");
        assert_eq!(run.error_code(), "invalid_params", "{name:?}");
    }
    // Let in, but as the revoked name: a case apart, an I for an l, a trailing space.
    for (name, kept) in [
        ("LAPTOP", "LAPTOP"),
        ("Iaptop", "Iaptop"),
        ("laptop ", "laptop"),
    ] {
        let params = json!({"device": name}).to_string();
        let waiting = pair_in_background(&home, &params);
        let request = until_requested(&home, kept);
        assert_eq!(request["revoked"], true, "{name:?}: {request}");
        let run = home.rlxd(&["gate", "pair", "--request-id", &id(&request)]);
        assert_eq!(run.error_code(), "revoked_device", "{name:?}: {run:?}");
        let run = home.rlxd(&["gate", "pair", "--request-id", &id(&request), "--deny"]);
        assert_eq!(run.code(), Some(0), "{run:?}");
        assert_eq!(finish(waiting).0, Some(1));
    }
}

impl App {
    /// Opens a following event stream, and returns once rlxd carries it: a request sent
    /// after the subscribe is answered.
    fn follow_events(&mut self) {
        let follow = json!({"follow": true}).as_object().unwrap().clone();
        self.connection.send("events.subscribe", follow).unwrap();
        let status = self.connection.send("status", Default::default()).unwrap();
        loop {
            match self.connection.read_incoming().unwrap() {
                Incoming::Reply(reply) if reply.envelope.id == Some(status) => {
                    assert!(reply.is_ok(), "{}", reply.line);
                    return;
                }
                Incoming::Reply(reply) => panic!("the stream ended: {}", reply.line),
                Incoming::Event(_) => {}
            }
        }
    }

    /// The next reply on a connection that carries a stream, past its events.
    fn next_reply(&mut self) -> Value {
        loop {
            match self.connection.read_incoming().unwrap() {
                Incoming::Reply(reply) => return serde_json::from_str(&reply.line).unwrap(),
                Incoming::Event(_) => {}
            }
        }
    }
}

#[test]
fn an_app_connection_carrying_an_event_stream_is_dropped_from_the_gate_too() {
    let home = TestHome::new();
    home.start_dev();
    let c1 = pair_test_client(&home);

    // Unpaired while it streams: the not_gate envelope, with no id, ends its stream.
    let mut streaming = App::connect(&home, &c1).unwrap();
    streaming.follow_events();
    let run = home.rlxd(&["gate", "unpair"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_eq!(run.data()["dropped"], 1);
    let last = streaming.next_reply();
    assert_eq!(last["error"]["code"], "not_gate", "{last}");
    assert!(last.get("id").is_none(), "{last}");
    assert!(matches!(streaming.connection.read_line(), Ok(None)));

    // Re-pairing itself while it streams: its reply, with the new credential, then the
    // not_gate envelope, and the connection closes.
    std::fs::remove_file(home.root().join("test-gate-credential")).unwrap();
    let c2 = pair_test_client(&home);
    let mut caller = App::connect(&home, &c2).unwrap();
    caller.follow_events();
    let pair = json!({"gate_credential": c2}).as_object().unwrap().clone();
    let id = caller.connection.send("gate.pair", pair).unwrap();
    let reply = caller.next_reply();
    assert_eq!(reply["id"], json!(id), "{reply}");
    assert_eq!(reply["ok"], true, "{reply}");
    let c3 = reply["data"]["gate_credential"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(c3, c2);
    let last = caller.next_reply();
    assert_eq!(last["error"]["code"], "not_gate", "{last}");
    assert!(matches!(caller.connection.read_line(), Ok(None)));
    assert!(App::connect(&home, &c2).is_err());
    assert!(App::connect(&home, &c3).is_ok());
}

#[test]
fn combining_letters_and_fillers_cannot_dodge_the_revoked_name_rule() {
    // Review #2068, r3 minor 2: "laptop" and a combining Latin letter compared apart from
    // the revoked "laptop", so it was approved without --force; Hangul fillers paired a
    // blank-looking name. Each is refused at once now, before any approval.
    let home = TestHome::new();
    home.start_dev();
    pair_test_client(&home);
    let waiting = pair_in_background(&home, r#"{"device":"laptop"}"#);
    let request = until_requested(&home, "laptop");
    let run = home.rlxd(&["gate", "pair", "--request-id", &id(&request)]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_eq!(finish(waiting).0, Some(0));
    assert_eq!(home.rlxd(&["gate", "unpair", "laptop"]).code(), Some(0));
    for name in [
        "laptop\u{0363}",
        "lapto\u{0369}p",
        "laptop\u{1DE7}",
        "\u{3164}",
        "\u{FFA0}",
        "\u{115F}\u{115F}",
    ] {
        let params = json!({"device": name}).to_string();
        let mut child = pair_in_background(&home, &params);
        // Refused at once; a name taken waits for an approval instead.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now() > deadline {
                child.kill().unwrap();
                let _ = child.wait();
                panic!("{name:?} was taken as a device name: it waits for an approval");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let (code, envelope) = finish(child);
        assert_eq!(code, Some(1), "{name:?}: {envelope}");
        assert_eq!(envelope["error"]["code"], "invalid_params", "{name:?}");
    }
    assert_eq!(home.rlxd(&["gate", "pair"]).data()["requests"], json!([]));
    assert_eq!(devices(&home).len(), 1, "only the test client");
}

/// A credential's SHA-256, as gate.json keeps it.
fn sha256_of(credential: &str) -> String {
    Sha256::digest(credential.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// macOS only: there a write waits whole once less than it is free in the send buffer,
/// so a reply can be left stuck exactly. Linux waits only once its own accounting of
/// the queued data reaches the buffer, which no reply size hits reliably; there the
/// unit test `server::tests::a_pairing_whose_reply_is_dropped_mid_send_is_undone`
/// covers the undo.
#[cfg(target_os = "macos")]
#[test]
fn a_re_pairing_reply_stuck_when_rlxd_stops_leaves_the_old_credential_paired() {
    // Review #2068, r3 minor 3: rlxd's stop aborted a connection still sending a
    // re-pairing's reply, so the new credential, which nobody received, stayed paired
    // and the old one was locked out.
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    let home = TestHome::new();
    home.start_dev();
    let c1 = pair_test_client(&home);
    let stream = UnixStream::connect(home.root().join("run").join("rlxd.sock")).unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let mut ask = |frame: Value| {
        writer.write_all(format!("{frame}\n").as_bytes()).unwrap();
    };
    let mut reply = String::new();
    ask(json!({"id": 1, "method": "hello", "params": {
        "token": home.token(), "protocol_version": 1, "client": "stuck",
        "client_version": "0", "role": "app", "capabilities": [],
        "gate_credential": c1,
    }}));
    reader.read_line(&mut reply).unwrap();
    assert!(reply.contains(r#""ok":true"#), "{reply}");
    // An unknown method's reply says the method's name: its length, besides the name.
    reply.clear();
    ask(json!({"id": 2, "method": "x"}));
    reader.read_line(&mut reply).unwrap();
    let besides_the_name = reply.len() - 1;
    // Every reply read, the send buffer is free. One unread reply fills it to 64 bytes
    // short, so the re-pairing's reply, about 165 bytes, waits.
    let sendspace: usize = String::from_utf8(
        std::process::Command::new("sysctl")
            .args(["-n", "net.local.stream.sendspace"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .parse()
    .unwrap();
    let name = "x".repeat(sendspace - 64 - besides_the_name);
    ask(json!({"id": 3, "method": name}));
    ask(json!({"id": 4, "method": "gate.pair", "params": {"gate_credential": c1}}));
    let old = sha256_of(&c1);
    eventually("the re-pairing is written", || {
        !std::fs::read_to_string(home.gate_json())
            .unwrap()
            .contains(&old)
    });
    home.stop();
    assert!(
        home.log().contains("never reached its client"),
        "{}",
        home.log()
    );
    assert!(
        std::fs::read_to_string(home.gate_json())
            .unwrap()
            .contains(&old)
    );
    home.start_dev();
    assert!(App::connect(&home, &c1).is_ok(), "the old credential works");
    drop(reader);
}
