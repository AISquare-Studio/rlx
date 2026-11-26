//! The settings API (the owner's multi-device brief, §4) against a real rlxd in a fresh
//! home: `settings.get`, `settings.set`, `settings.list` and `settings.changed`. The
//! checks are task #13's acceptance, by number.
//!
//! An app-role connection, the only one that may set `send_safety_mode`, is a device
//! that `gate.pair` paired (5.1). These tests pair one for real, in gate dev mode
//! (`rlxd serve --gate-dev-mode`, which relaxes only the peer identity check), and keep its
//! credential in `<home>/test-gate-credential`, which `rlxd call --as-app` sends ([`pair`]).

mod common;

use std::time::{Duration, Instant};

use common::{TestHome, assert_valid, eventually, finish};
use rlxd_client::proto::{HelloParams, Role, SubscribeParams};
use rlxd_client::{Connection, Home, Next};
use serde_json::{Value, json};

/// `rlxd call [--as-app] settings.set <params>`.
fn set(home: &TestHome, as_app: bool, params: Value) -> common::Run {
    let params = params.to_string();
    let mut args = vec!["call"];
    if as_app {
        args.push("--as-app");
    }
    args.extend(["settings.set", params.as_str()]);
    home.rlxd(&args)
}

fn get(home: &TestHome, key: &str) -> Value {
    let params = json!({ "key": key }).to_string();
    let run = home.rlxd(&["call", "settings.get", &params]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_valid("setting.json", &run.data());
    run.data()
}

/// A connection to `home`'s rlxd that has said hello as `cli`.
fn connect(home: &TestHome) -> Connection {
    let home = Home::at(home.root());
    let token = std::fs::read_to_string(home.token()).unwrap();
    let mut connection = Connection::open(&home).unwrap();
    let hello = HelloParams::new(token.trim(), "settings-test", "0", Role::Cli).unwrap();
    assert!(connection.hello(&hello).unwrap().is_ok());
    connection
}

/// Pairs the device `device` with `home`'s rlxd, started in gate dev mode, through
/// `gate.pair`, and keeps its credential where `rlxd call --as-app` finds it.
fn pair(home: &TestHome, device: &str) {
    let params = json!({ "device": device }).to_string();
    let run = home.rlxd(&["call", "gate.pair", &params]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let data = run.data();
    assert_eq!(data["device"], device, "{data}");
    let credential = data["gate_credential"].as_str().unwrap();
    std::fs::write(home.root().join("test-gate-credential"), credential).unwrap();
}

/// A home whose rlxd runs in gate dev mode, with `device` paired.
fn paired_home(device: &str) -> TestHome {
    let home = TestHome::new();
    home.start_dev();
    pair(&home, device);
    home
}

fn log(home: &TestHome) -> String {
    std::fs::read_to_string(Home::at(home.root()).log_file()).unwrap()
}

#[test]
fn check_1_a_set_reaches_a_second_subscribed_client_within_1_s_and_get_returns_it() {
    let home = TestHome::new();
    home.start();
    let since = home.status()["seq"].as_u64().unwrap();
    let params = SubscribeParams::new()
        .with_topics(["settings"])
        .unwrap()
        .with_since(since);
    let mut second = connect(&home).subscribe(&params).unwrap();
    let follower = home.spawn(&[
        "events",
        "--topics",
        "settings",
        "--since",
        &since.to_string(),
        "--until",
        "settings.changed",
        "--timeout",
        "20",
    ]);

    let started = Instant::now();
    let reply = set(&home, false, json!({"key": "theme", "value": "rlx_light"}));
    assert_eq!(reply.code(), Some(0), "{reply:?}");
    assert_valid("setting.json", &reply.data());
    assert_eq!(reply.data()["version"], 1);
    let event = match second.next_frame().unwrap() {
        Next::Event(event) => event,
        Next::End(end) => panic!("an event, not {}", end.line),
    };
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(1),
        "settings.changed took {took:?}"
    );
    let event: Value = serde_json::from_str(&event.line).unwrap();
    assert_valid("event.json", &event);
    assert_valid("settings-changed.json", &event["data"]);
    assert_eq!(event["type"], "settings.changed");
    assert_eq!(event["data"], json!({"key": "theme", "version": 1}));

    let got = get(&home, "theme");
    assert_eq!(got["value"], "rlx_light");
    assert_eq!(got["version"], 1);

    let run = finish(follower);
    assert_eq!(run.code(), Some(0), "rlxd events sees it too: {run:?}");
    assert_eq!(run.lines()[0]["data"]["key"], "theme");
}

#[test]
fn check_2_two_writes_from_the_same_version_the_first_wins_the_second_conflicts() {
    let home = TestHome::new();
    home.start();
    let first = set(
        &home,
        false,
        json!({"key": "layout", "value": {"panes": 2}, "expected_version": 0}),
    );
    assert_eq!(first.code(), Some(0), "{first:?}");
    assert_eq!(first.data()["version"], 1);
    let second = set(
        &home,
        false,
        json!({"key": "layout", "value": {"panes": 3}, "expected_version": 0}),
    );
    assert_eq!(second.code(), Some(1), "{second:?}");
    assert_eq!(second.error_code(), "conflict");
    assert_eq!(second.json()["error"]["detail"], json!({"version": 1}));
    assert_valid("envelope.json", &second.json());
    assert_eq!(get(&home, "layout")["value"], json!({"panes": 2}));
    // From the version it now has, the write goes through.
    let again = set(
        &home,
        false,
        json!({"key": "layout", "value": {"panes": 3}, "expected_version": 1}),
    );
    assert_eq!(again.data()["version"], 2);
    // With no expected_version, last write wins.
    let blind = set(
        &home,
        false,
        json!({"key": "layout", "value": {"panes": 4}}),
    );
    assert_eq!(blind.data()["version"], 3);
}

#[test]
fn check_3_only_an_app_role_connection_sets_send_safety_mode_and_its_change_is_logged() {
    let home = paired_home("Sam MacBook");
    let auto = json!({"key": "send_safety_mode", "value": {"default": "auto"}});
    let cli = set(&home, false, auto.clone());
    assert_eq!(cli.code(), Some(1), "{cli:?}");
    assert_eq!(cli.error_code(), "not_gate");
    let params = auto.to_string();
    for role in ["tui", "remote"] {
        let run = home.rlxd(&["call", "--role", role, "settings.set", &params]);
        assert_eq!(run.error_code(), "not_gate", "{role}: {run:?}");
    }
    assert_eq!(
        get(&home, "send_safety_mode")["version"],
        0,
        "nothing was set"
    );

    let since = home.status()["seq"].as_u64().unwrap().to_string();
    let app = set(&home, true, auto);
    assert_eq!(app.code(), Some(0), "{app:?}");
    assert_eq!(app.data()["value"], json!({"default": "auto"}));
    assert_eq!(app.data()["by_device"], "Sam MacBook");
    let events = home
        .rlxd(&["events", "--topics", "settings", "--since", &since])
        .lines();
    assert_eq!(
        events[0]["data"],
        json!({"key": "send_safety_mode", "version": 1, "by_device": "Sam MacBook"}),
        "the event names the device"
    );
    let log = log(&home);
    assert!(
        log.contains(r#"send_safety_mode set, version 1, by device "Sam MacBook""#)
            && log.contains("(lowers send safety)"),
        "{log}"
    );
    assert!(
        log.contains("send_safety_mode refused for cli rlxd-cli (not_gate)"),
        "{log}"
    );

    // Raising it back is logged too, as not lowering.
    let draft = set(
        &home,
        true,
        json!({"key": "send_safety_mode", "value": {"default": "draft"}}),
    );
    assert_eq!(draft.data()["version"], 2);
    let log = self::log(&home);
    let line = log
        .lines()
        .find(|l| l.contains("send_safety_mode set, version 2"))
        .unwrap();
    assert!(!line.contains("lowers"), "{line}");

    // Any role may still read it, and set the other settings.
    assert_eq!(get(&home, "send_safety_mode")["by_device"], "Sam MacBook");
    let theme = set(&home, false, json!({"key": "theme", "value": "rlx"}));
    assert_eq!(theme.code(), Some(0), "{theme:?}");
    assert!(
        theme.data().get("by_device").is_none(),
        "a cli is no device"
    );
}

#[test]
fn an_app_role_hello_without_a_paired_credential_is_not_gate() {
    // A credential rlxd never minted.
    let home = TestHome::new();
    home.start();
    std::fs::write(home.root().join("test-gate-credential"), "anything").unwrap();
    let run = home.rlxd(&["call", "--as-app", "settings.list"]);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_eq!(run.error_code(), "not_gate");
    // A wrong one, beside a device that is paired.
    let paired = paired_home("d");
    let wrong = paired.rlxd(&[
        "call",
        "--as-app",
        "--gate-credential",
        "wrong",
        "settings.list",
    ]);
    assert_eq!(wrong.error_code(), "not_gate");
    // An unpaired device's credential: send_safety_mode can no longer be set with it.
    assert_eq!(paired.rlxd(&["gate", "unpair", "d"]).code(), Some(0));
    let unpaired = set(
        &paired,
        true,
        json!({"key": "send_safety_mode", "value": {"default": "auto"}}),
    );
    assert_eq!(unpaired.error_code(), "not_gate", "{unpaired:?}");
    assert_eq!(get(&paired, "send_safety_mode")["version"], 0);
}

#[test]
fn check_4_values_survive_a_restart_in_a_private_file() {
    let home = paired_home("Sam MacBook");
    set(&home, false, json!({"key": "theme", "value": "rlx_light"}));
    set(&home, false, json!({"key": "theme", "value": "rlx"}));
    set(
        &home,
        false,
        json!({"key": "agent_sign_off", "value": "— Ava, Sam's AI Agent"}),
    );
    set(
        &home,
        true,
        json!({"key": "send_safety_mode",
               "value": {"default": "standing_approval", "connectors": {"claude.ai Slack": "draft"}}}),
    );
    set(
        &home,
        false,
        json!({"key": "rlxapp.sidebar", "value": {"width": 240}}),
    );
    let before = home.rlxd(&["call", "settings.list"]).data();
    assert_valid("settings-list-reply.json", &before);
    assert_eq!(before["settings"].as_array().unwrap().len(), 4);

    assert_eq!(home.rlxd(&["stop"]).code(), Some(0));
    home.start_dev();
    let after = home.rlxd(&["call", "settings.list"]).data();
    assert_eq!(after, before, "every value, version and device, as it was");
    assert_eq!(get(&home, "theme")["version"], 2);

    let file = Home::at(home.root()).rlxd_dir().join("settings.json");
    let stored: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(stored["schema_version"], 1);
    assert_eq!(stored["settings"]["theme"]["value"], "rlx");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn keys_and_values_of_the_wrong_shape_are_refused_with_their_codes() {
    let home = TestHome::new();
    home.start();
    for (params, code) in [
        (json!({"key": "nope", "value": 1}), "unknown_setting"),
        (json!({"key": "theme", "value": 1}), "invalid_params"),
        (json!({"key": "theme", "value": ""}), "invalid_params"),
        (json!({"key": "layout", "value": "wide"}), "invalid_params"),
        (
            json!({"key": "send_safety_mode", "value": {"default": "yolo"}}),
            "invalid_params",
        ),
        (json!({"key": "rlxapp.x", "value": null}), "invalid_params"),
        (json!({"key": "Theme", "value": "x"}), "invalid_params"),
        (json!({"key": "theme"}), "invalid_params"),
    ] {
        let run = set(&home, false, params.clone());
        assert_eq!(run.code(), Some(1), "{params}: {run:?}");
        assert_eq!(run.error_code(), code, "{params}");
        assert_valid("envelope.json", &run.json());
    }
    // A value past 64 KiB, sent on a connection of the test's own: as an argument to
    // `rlxd call` it would pass Windows' 32,767-character command line.
    let huge = json!({"key": "rlxapp.huge", "value": "x".repeat(70_000)});
    let reply = connect(&home)
        .request("settings.set", huge.as_object().unwrap().clone())
        .unwrap();
    assert_eq!(
        reply.error().unwrap().code,
        "invalid_params",
        "{}",
        reply.line
    );
    assert!(
        reply.error().unwrap().message.contains("65536"),
        "{}",
        reply.line
    );

    let run = home.rlxd(&["call", "settings.get", r#"{"key":"nope"}"#]);
    assert_eq!(run.error_code(), "unknown_setting");
    assert!(
        run.json()["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("send_safety_mode")
    );
    let run = home.rlxd(&["call", "settings.get", "{}"]);
    assert_eq!(run.error_code(), "invalid_params");
}

#[test]
fn a_key_never_set_is_version_0_and_list_filters_by_prefix() {
    let home = TestHome::new();
    home.start();
    let unset = get(&home, "send_safety_mode");
    assert_eq!(unset, json!({"key": "send_safety_mode", "version": 0}));
    assert_eq!(get(&home, "rlxapp.anything")["version"], 0);
    for key in ["rlxapp.b", "theme", "rlxapp.a", "tray.style"] {
        let value = if key == "theme" {
            json!("rlx")
        } else {
            json!(1)
        };
        set(&home, false, json!({"key": key, "value": value}));
    }
    let keys = |prefix: Option<&str>| -> Vec<String> {
        let params = prefix.map_or_else(|| "{}".to_owned(), |p| json!({ "prefix": p }).to_string());
        let data = home.rlxd(&["call", "settings.list", &params]).data();
        assert_valid("settings-list-reply.json", &data);
        data["settings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["key"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(keys(None), ["rlxapp.a", "rlxapp.b", "theme", "tray.style"]);
    assert_eq!(keys(Some("rlxapp.")), ["rlxapp.a", "rlxapp.b"]);
}

#[test]
fn settings_are_served_mid_stream_and_each_keys_events_come_in_version_order() {
    let home = TestHome::new();
    home.start();
    let since = home.status()["seq"].as_u64().unwrap();
    let mut connection = connect(&home);
    connection
        .send(
            "events.subscribe",
            json!({"topics": ["settings"], "since": since})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap();
    for n in 1..=5 {
        let run = set(&home, false, json!({"key": "rlxapp.counter", "value": n}));
        assert_eq!(run.data()["version"], n);
    }
    eventually("five settings events", || {
        home.status()["seq"].as_u64() >= Some(since + 5)
    });
    let mut versions = Vec::new();
    while versions.len() < 5 {
        match connection.read_incoming().unwrap() {
            rlxd_client::Incoming::Event(event) => {
                versions.push(event.event.data["version"].as_u64().unwrap());
            }
            rlxd_client::Incoming::Reply(reply) => panic!("an event, not {}", reply.line),
        }
    }
    assert_eq!(versions, [1, 2, 3, 4, 5]);
    // A settings request on the streaming connection is answered between its events.
    let asked = connection
        .send(
            "settings.get",
            json!({"key": "rlxapp.counter"})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap();
    match connection.read_incoming().unwrap() {
        rlxd_client::Incoming::Reply(reply) => {
            assert_eq!(reply.envelope.id, Some(asked));
            assert_eq!(reply.data().unwrap()["value"], 5);
        }
        rlxd_client::Incoming::Event(event) => panic!("a reply, not {}", event.line),
    }
}

#[test]
fn an_unreadable_settings_file_is_set_aside_and_rlxd_still_starts() {
    let home = TestHome::new();
    let dir = Home::at(home.root()).rlxd_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("settings.json"), "{not json").unwrap();
    home.start();
    assert_eq!(get(&home, "theme")["version"], 0);
    assert_eq!(
        std::fs::read_to_string(dir.join("settings.json.unreadable")).unwrap(),
        "{not json"
    );
    assert!(log(&home).contains("cannot be read, so rlxd starts with no settings"));
}
