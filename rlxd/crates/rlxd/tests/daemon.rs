//! The daemon's checks (handoff 0.10.3 rev 6: 1.3, 7.2 and 7.4; task #20's acceptance),
//! each against a fresh home, on every OS rlxd.yml runs.

mod common;

use common::{TestHome, assert_valid, eventually, fake_claude, kill_hard};
use rlxd_client::proto::{HelloParams, Role};
use rlxd_client::{Connection, Home};
use serde_json::{Value, json};

/// The version every crate of the workspace carries, which is rlx's.
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[test]
fn serve_detach_starts_rlxd_and_status_describes_it() {
    // Check 1.
    let home = TestHome::new();
    let started = home.start();
    let run = home.rlxd(&["status", "--json"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let envelope = run.json();
    assert_eq!(envelope["schema_version"], 1);
    assert_eq!(envelope["ok"], true);
    let data = &envelope["data"];
    assert!(data["pid"].as_u64().is_some_and(|pid| pid > 0), "{data}");
    assert_eq!(data["version"], VERSION);
    assert_eq!(
        data["pid"], started["pid"],
        "--detach printed the new rlxd's status"
    );
    assert_eq!(data["platform"], std::env::consts::OS);
    assert_eq!(data["protocol_version"], 1);
    assert!(data["capabilities"].is_array());
    assert_eq!(data["seq"], 1, "daemon.started, a new home's first event");
    assert_eq!(data["gate"], "signed app only");
    assert_eq!(data["paths"]["home"], home.root().display().to_string());
    assert_valid("status-reply.json", data);
}

#[test]
fn a_second_serve_exits_1_already_running() {
    // Check 2, in the foreground and detached.
    let home = TestHome::new();
    let first = home.start();
    for args in [&["serve"][..], &["serve", "--detach"]] {
        let run = home.rlxd(args);
        assert_eq!(run.code(), Some(1), "{args:?}: {run:?}");
        assert_eq!(run.error_code(), "already_running", "{args:?}");
        assert_valid("envelope.json", &run.json());
    }
    assert_eq!(
        home.status()["pid"],
        first["pid"],
        "the first one still serves"
    );
}

#[test]
fn role_app_is_refused_with_not_gate_and_never_counted() {
    // Check 3a, and 5.1's refusals: outside dev mode the unsigned test client can neither
    // pair nor present a credential, so every app hello is refused. tests/gate.rs has
    // the rest of the gate.
    let home = TestHome::new();
    home.start();
    for args in [
        &["call", "--as-app", "status"][..],
        &["call", "--role", "app", "status"],
        &["call", "--role", "app", "--client", "rlx-tui", "status"],
        &[
            "call",
            "--role",
            "app",
            "--gate-credential",
            "wrong",
            "status",
        ],
    ] {
        let run = home.rlxd(args);
        assert_eq!(run.code(), Some(1), "{args:?}: {run:?}");
        assert_eq!(run.error_code(), "not_gate", "{args:?}");
        assert_valid("envelope.json", &run.json());
    }
    std::fs::write(home.root().join("test-gate-credential"), "kept\n").unwrap();
    let run = home.rlxd(&["call", "--as-app", "status"]);
    assert_eq!(
        run.error_code(),
        "not_gate",
        "a kept credential changes nothing yet"
    );
    assert_eq!(home.status()["clients"]["app"], 0);
}

#[test]
fn a_wrong_token_or_no_hello_is_unauthorized() {
    // Check 3.
    let home = TestHome::new();
    home.start();
    let run = home.rlxd(&["call", "--token", "wrong", "status"]);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_eq!(run.error_code(), "unauthorized");
    let frame = "{\"id\":1,\"method\":\"status\"}\n";
    let run = home.rlxd_with_stdin(&["call", "--raw"], Some(frame));
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_eq!(run.error_code(), "unauthorized");
    assert_eq!(run.json()["id"], 1, "the refusal names the frame's id");
    for first in [
        "not json",
        "{\"id\":1}",
        "[1]",
        "{\"id\":2,\"method\":\"hello\",\"params\":{}}",
    ] {
        let run = home.rlxd_with_stdin(&["call", "--raw"], Some(&format!("{first}\n")));
        assert_eq!(run.code(), Some(1), "{first}: {run:?}");
        assert_eq!(run.error_code(), "unauthorized", "{first}");
        assert_valid("envelope.json", &run.json());
    }
}

#[test]
fn an_unknown_method_is_unknown_method_with_a_message() {
    // Check 4.
    let home = TestHome::new();
    home.start();
    let run = home.rlxd(&["call", "no.such.method"]);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_eq!(run.error_code(), "unknown_method");
    let message = run.json()["error"]["message"].as_str().unwrap().to_owned();
    assert!(!message.is_empty());
}

#[test]
fn every_reply_is_a_valid_envelope() {
    // Check 5, here with the schema crate; the ready note runs check-jsonschema too.
    let home = TestHome::new();
    home.start();
    let run = home.rlxd(&["call", "status"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let envelope = run.json();
    assert_valid("envelope.json", &envelope);
    assert_valid("status-reply.json", &envelope["data"]);
    assert!(
        envelope["id"].is_u64(),
        "a socket reply carries its request's id"
    );
    for args in [
        &["call", "no.such.method"][..],
        &["call", "--token", "t", "status"],
    ] {
        assert_valid("envelope.json", &home.rlxd(args).json());
    }
}

#[test]
fn the_socket_and_the_token_admit_only_this_user() {
    // Check 6.
    let home = TestHome::new();
    home.start();
    let rlxd = Home::at(home.root());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&rlxd.socket()), 0o600, "{}", rlxd.socket().display());
        assert_eq!(mode(&rlxd.token()), 0o600);
        assert_eq!(mode(&rlxd.run_dir()), 0o700);
        assert_eq!(mode(&rlxd.rlxd_dir()), 0o700);
        assert_eq!(home.status().get("pipe_acl"), None);
    }
    #[cfg(windows)]
    {
        let status = home.status();
        assert_eq!(status["pipe_acl"], "current_user");
        assert_eq!(status["platform"], "windows");
        assert_eq!(status["paths"]["socket"], rlxd.pipe_name());
    }
}

#[test]
fn stop_stops_it_and_then_it_is_not_running() {
    // Check 7.
    let home = TestHome::new();
    let pid = home.start()["pid"].clone();
    let run = home.rlxd(&["stop"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_eq!(run.data()["pid"], pid);
    let run = home.rlxd(&["status", "--json"]);
    assert_eq!(run.code(), Some(3), "{run:?}");
    assert_eq!(run.error_code(), "not_running");
    for args in [&["stop"][..], &["call", "status"], &["status"]] {
        let run = home.rlxd(args);
        assert_eq!(run.code(), Some(3), "{args:?}: {run:?}");
    }
    #[cfg(unix)]
    assert!(
        !Home::at(home.root()).socket().exists(),
        "the socket is removed"
    );
    // And it starts again.
    home.start();
}

#[test]
fn with_no_claude_code_status_says_it_is_not_installed() {
    // Check 9.
    let home = TestHome::new().with_claude(std::env::temp_dir().join("rlxd-no-such-claude"));
    home.start();
    let claude = &home.status()["claude"];
    assert_eq!(claude["installed"], false, "{claude}");
    assert_eq!(claude["signed_in"], false, "{claude}");
}

#[test]
fn a_signed_in_claude_code_names_its_account_and_plan() {
    // 7.4's fake-claude/auth-status check.
    let home = TestHome::new().with_claude(fake_claude("auth-status"));
    home.start();
    let claude = &home.status()["claude"];
    assert_eq!(claude["installed"], true, "{claude}");
    assert_eq!(claude["signed_in"], true, "{claude}");
    assert_eq!(claude["account"], "probe@example.com");
    assert_eq!(claude["plan"], "pro");
    assert_eq!(claude["auth_method"], "claude.ai");
    assert_eq!(claude["version"], "2.1.314");
}

#[test]
fn a_protocol_version_rlxd_does_not_serve_is_incompatible_protocol() {
    // 7.2: never a silent misparse, whatever the number.
    let home = TestHome::new();
    home.start();
    for version in [
        "99",
        "0",
        "2",
        "-1",
        "4294967296",
        "18446744073709551616",
        "1.5",
    ] {
        let run = home.rlxd(&["call", "--protocol-version", version, "status"]);
        assert_eq!(run.code(), Some(1), "{version}: {run:?}");
        let envelope = run.json();
        assert_eq!(
            envelope["error"]["code"], "incompatible_protocol",
            "{version}"
        );
        let supported = envelope["error"]["detail"]["supported"].as_array().unwrap();
        assert!(supported.contains(&json!(1)), "{version}");
        assert_valid("envelope.json", &envelope);
    }
    let run = home.rlxd(&["call", "--protocol-version", "x", "status"]);
    assert_eq!(
        run.code(),
        Some(2),
        "not a number is a usage error: {run:?}"
    );
    let data = home.status();
    assert_eq!(data["protocol_version"], 1);
    assert!(data["capabilities"].is_array());
}

#[test]
fn status_counts_each_connection_by_its_role_while_it_is_open() {
    let home = TestHome::new();
    home.start();
    let rlxd = Home::at(home.root());
    let token = std::fs::read_to_string(rlxd.token()).unwrap();
    let mut held = Vec::new();
    for role in [Role::Tui, Role::Tui, Role::Remote] {
        let mut connection = Connection::open(&rlxd).unwrap();
        let hello = HelloParams::new(token.trim(), "test", "1", role).unwrap();
        assert!(connection.hello(&hello).unwrap().is_ok());
        held.push(connection);
    }
    let clients = home.status()["clients"].clone();
    assert_eq!(clients, json!({"app": 0, "tui": 2, "remote": 1, "cli": 1}));
    drop(held);
    eventually("the closed connections are no longer counted", || {
        home.status()["clients"] == json!({"app": 0, "tui": 0, "remote": 0, "cli": 1})
    });
}

#[test]
fn a_frame_that_is_not_a_request_is_answered_and_the_connection_goes_on() {
    let home = TestHome::new();
    home.start();
    let rlxd = Home::at(home.root());
    let token = std::fs::read_to_string(rlxd.token()).unwrap();
    let mut connection = Connection::open(&rlxd).unwrap();
    let hello = HelloParams::new(token.trim(), "test", "1", Role::Cli).unwrap();
    let reply = connection.hello(&hello).unwrap();
    assert!(reply.is_ok(), "{}", reply.line);
    let data: Value = serde_json::from_str(&reply.line).unwrap();
    assert_valid("hello-reply.json", &data["data"]);
    for frame in [
        "not json",
        "[]",
        "{\"id\":5}",
        "{\"id\":6,\"method\":\"status\",\"params\":[]}",
        "{\"id\":7,\"method\":\"hello\",\"params\":{}}",
    ] {
        connection.send_line(frame).unwrap();
        let reply = connection.read_reply().unwrap();
        let envelope: Value = serde_json::from_str(&reply.line).unwrap();
        assert_eq!(
            envelope["error"]["code"], "invalid_request",
            "{frame}: {envelope}"
        );
    }
    connection.send_line("").unwrap();
    let reply = connection.request("status", Default::default()).unwrap();
    assert!(reply.is_ok(), "{}", reply.line);
}

#[test]
fn a_killed_rlxd_leaves_nothing_that_stops_the_next_start() {
    let home = TestHome::new();
    let pid = home.start()["pid"].as_u64().unwrap();
    kill_hard(pid);
    eventually("the killed rlxd is not running", || {
        home.rlxd(&["status", "--json"]).code() == Some(3)
    });
    let again = home.start();
    assert_ne!(again["pid"], pid);
}

#[test]
fn rlx_home_wins_over_aisquare_home() {
    let home = TestHome::new();
    let other = TestHome::new();
    let mut command = home.command(&["serve", "--detach"]);
    command.env("RLX_HOME", other.root());
    let run = common::run(command, None);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_eq!(
        run.data()["paths"]["home"],
        other.root().display().to_string()
    );
    assert_eq!(home.rlxd(&["status", "--json"]).code(), Some(3));
    assert_eq!(
        other.status()["paths"]["home"],
        other.root().display().to_string()
    );
}

#[test]
fn the_log_says_what_rlxd_did() {
    let home = TestHome::new();
    home.start();
    home.rlxd(&["call", "--token", "wrong", "status"]);
    home.rlxd(&["stop"]);
    let log = std::fs::read_to_string(Home::at(home.root()).log_file()).unwrap();
    for said in [
        "starting",
        "listening on",
        "hello refused: a wrong token",
        "stopped",
    ] {
        assert!(log.contains(said), "{said:?} in:\n{log}");
    }
    let token = std::fs::read_to_string(Home::at(home.root()).token()).unwrap();
    assert!(!log.contains(token.trim()), "the token is never logged");
}

// --- the daemon registry (4.1) ----------------------------------------------------------

/// The registry's records: each `<id>.json` in `<home>/daemons/`, read.
fn records(home: &TestHome) -> Vec<(String, Value)> {
    let dir = Home::at(home.root()).daemons();
    let mut found = Vec::new();
    for entry in std::fs::read_dir(&dir).into_iter().flatten() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        if let Some(id) = name.strip_suffix(".json") {
            let text = std::fs::read_to_string(dir.join(&name)).unwrap();
            found.push((id.to_owned(), serde_json::from_str(&text).unwrap()));
        }
    }
    found
}

/// Whether another process holds the lock file at `path`, probed as Python's registry
/// probes it: taken without waiting, and let go at once.
fn lock_held(path: &std::path::Path) -> bool {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    match file.try_lock() {
        Ok(()) => {
            file.unlock().unwrap();
            false
        }
        Err(std::fs::TryLockError::WouldBlock) => true,
        Err(std::fs::TryLockError::Error(e)) => panic!("{}: {e}", path.display()),
    }
}

#[test]
fn while_rlxd_runs_its_record_is_in_the_registry_held_by_its_lock() {
    let home = TestHome::new();
    let pid = home.start()["pid"].clone();
    let found = records(&home);
    assert_eq!(found.len(), 1, "{found:?}");
    let (id, record) = &found[0];
    assert_valid("daemon-record.json", record);
    assert_eq!(&record["id"], id);
    assert_eq!(record["kind"], "rlxd");
    assert_eq!(record["pid"], pid);
    assert_eq!(record["version"], VERSION);
    assert_eq!(record["restartable"], true);
    let argv: Vec<&str> = record["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert!(std::path::Path::new(argv[0]).is_absolute(), "{argv:?}");
    assert_eq!(argv[1..], ["serve", "--detach"]);
    let dir = Home::at(home.root()).daemons();
    assert!(
        lock_held(&dir.join(format!("{id}.lock"))),
        "the record's lock is held"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        for name in [
            format!("{id}.json"),
            format!("{id}.lock"),
            ".gate.lock".to_owned(),
        ] {
            assert_eq!(mode(&dir.join(&name)), 0o600, "{name}");
        }
    }

    let run = home.rlxd(&["stop"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert!(records(&home).is_empty(), "the record is gone after a stop");
    assert!(
        !dir.join(format!("{id}.lock")).exists(),
        "and its lock file"
    );
}

#[test]
fn the_heartbeat_rewrites_the_record() {
    let home = TestHome::new();
    home.start();
    let first = records(&home)[0].1["heartbeat_at"].clone();
    eventually("a heartbeat within 10 s", || {
        records(&home)
            .first()
            .is_some_and(|(_, record)| record["heartbeat_at"] != first)
    });
    let (_, record) = &records(&home)[0];
    assert_valid("daemon-record.json", record);
    assert!(record["heartbeat_at"].as_str() > record["started_at"].as_str());
}

#[test]
fn after_a_kill_the_record_stays_but_its_lock_is_free() {
    let home = TestHome::new();
    let pid = home.start()["pid"].as_u64().unwrap();
    let (id, _) = records(&home).remove(0);
    kill_hard(pid);
    let lock = Home::at(home.root()).daemons().join(format!("{id}.lock"));
    eventually("the OS freed the killed rlxd's lock", || !lock_held(&lock));
    assert_eq!(records(&home).len(), 1, "nothing ran to remove the record");
}
