//! The PTY host (handoff 0.10.3, 1.4) against a real rlxd in a fresh home: task #28's
//! acceptance, by number where it has one.
//!
//! An app-role connection is a device `gate.pair` paired, in gate dev mode, as the
//! settings tests pair one. A `tui` or `remote` connection says its role in its hello.

mod common;

#[cfg(unix)]
use std::process::Command;
#[cfg(unix)]
use std::time::{Duration, Instant};

use common::{TestHome, assert_valid, eventually};
use rlxd_client::proto::{HelloParams, Role};
use rlxd_client::{Connection, Home};
use serde_json::{Value, json};

/// A command that prints `ready` then waits, as the handoff's check runs.
fn ready_argv() -> Value {
    if cfg!(windows) {
        json!(["cmd", "/c", "echo ready & timeout /t 30"])
    } else {
        json!(["sh", "-c", "echo ready; sleep 30"])
    }
}

fn call(home: &TestHome, method: &str, params: &Value) -> common::Run {
    home.rlxd(&["call", method, &params.to_string()])
}

fn spawn(home: &TestHome, params: Value) -> String {
    let run = call(home, "pty.spawn", &params);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_valid("pty-ref.json", &run.data());
    run.data()["pty_id"].as_str().unwrap().to_owned()
}

fn screen(home: &TestHome, id: &str) -> Value {
    let run = call(home, "pty.screen", &json!({ "pty_id": id }));
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_valid("pty-screen-reply.json", &run.data());
    run.data()
}

fn list(home: &TestHome) -> Value {
    let run = home.rlxd(&["call", "pty.list"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_valid("pty-list-reply.json", &run.data());
    run.data()
}

/// A connection to `home`'s rlxd that has said hello as `role`.
fn connect(home: &TestHome, role: Role) -> Connection {
    let home = Home::at(home.root());
    let token = std::fs::read_to_string(home.token()).unwrap();
    let mut connection = Connection::open(&home).unwrap();
    let hello = HelloParams::new(token.trim(), "pty-test", "0", role).unwrap();
    assert!(connection.hello(&hello).unwrap().is_ok());
    connection
}

fn request(connection: &mut Connection, method: &str, params: Value) -> rlxd_client::Reply {
    connection
        .request(method, params.as_object().cloned().unwrap_or_default())
        .unwrap()
}

#[test]
fn check_1_spawn_then_the_screen_says_ready() {
    let home = TestHome::new();
    home.start();
    let id = spawn(&home, json!({"argv": ready_argv(), "cols": 80, "rows": 24}));
    assert_eq!(id.len(), 32);
    eventually("the screen says ready", || {
        screen(&home, &id)["text"]
            .as_str()
            .unwrap()
            .contains("ready")
    });
    let shown = screen(&home, &id);
    assert_eq!(
        (shown["cols"].clone(), shown["rows"].clone()),
        (json!(80), json!(24))
    );
    assert_eq!(shown["exited"], false);
    assert_eq!(
        shown["text"].as_str().unwrap().matches('\n').count(),
        24,
        "a newline after every row"
    );
}

#[test]
fn check_1_a_pty_outlives_the_connection_that_spawned_it() {
    let home = TestHome::new();
    home.start();
    let mut first = connect(&home, Role::Tui);
    let params = json!({"argv": ready_argv(), "cols": 80, "rows": 24, "label": "coder"});
    let reply = request(&mut first, "pty.spawn", params);
    assert!(reply.is_ok(), "{}", reply.line);
    let id = reply.data().unwrap()["pty_id"].as_str().unwrap().to_owned();
    drop(first);
    // Another process, another connection: the PTY is still the home's.
    let ptys = list(&home);
    let found: Vec<&Value> = ptys["ptys"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["pty_id"] == id.as_str())
        .collect();
    assert_eq!(found.len(), 1, "{ptys}");
    assert_eq!(found[0]["label"], "coder");
    assert_eq!(found[0]["exited"], false);
    assert!(found[0]["pid"].as_u64().unwrap() > 0);
}

#[test]
fn check_1_an_ended_process_keeps_its_screen_and_status() {
    let home = TestHome::new();
    home.start();
    let argv = if cfg!(windows) {
        json!(["cmd", "/c", "echo bye & exit 3"])
    } else {
        json!(["sh", "-c", "echo bye; exit 3"])
    };
    let id = spawn(&home, json!({"argv": argv, "cols": 80, "rows": 24}));
    eventually("the process ends", || screen(&home, &id)["exited"] == true);
    let shown = screen(&home, &id);
    assert_eq!(shown["status"], 3, "{shown}");
    assert!(shown["text"].as_str().unwrap().contains("bye"), "{shown}");
    let ptys = list(&home);
    assert_eq!(ptys["ptys"][0]["status"], 3);
    // Its screen stays; it takes no more input or size, until it is killed.
    for (method, params) in [
        ("pty.write", json!({"pty_id": id, "text": "x"})),
        ("pty.resize", json!({"pty_id": id, "cols": 100, "rows": 30})),
    ] {
        let run = call(&home, method, &params);
        assert_eq!(run.error_code(), "pty_exited", "{method}: {run:?}");
    }
    assert_eq!(
        call(&home, "pty.kill", &json!({ "pty_id": id })).code(),
        Some(0)
    );
    assert_eq!(list(&home)["ptys"], json!([]));
    let gone = call(&home, "pty.screen", &json!({ "pty_id": id }));
    assert_eq!(gone.error_code(), "unknown_pty");
}

#[test]
fn check_1_a_process_ended_by_a_signal_says_so() {
    if cfg!(windows) {
        return;
    }
    let home = TestHome::new();
    home.start();
    let id = spawn(
        &home,
        json!({"argv": ["sh", "-c", "kill -TERM $$"], "cols": 80, "rows": 24}),
    );
    eventually("the process ends", || screen(&home, &id)["exited"] == true);
    let shown = screen(&home, &id);
    assert_eq!(
        (shown["status"].clone(), shown["signal"].clone()),
        (json!(143), json!(15))
    );
}

#[test]
fn the_pty_topic_carries_pty_exited() {
    let home = TestHome::new();
    home.start();
    let since = home.status()["seq"].as_u64().unwrap().to_string();
    let argv = if cfg!(windows) {
        json!(["cmd", "/c", "exit 3"])
    } else {
        json!(["sh", "-c", "exit 3"])
    };
    spawn(&home, json!({"argv": argv, "cols": 80, "rows": 24}));
    let events = home.rlxd(&[
        "events",
        "--topics",
        "pty",
        "--since",
        &since,
        "--follow",
        "--until",
        "pty.exited",
        "--timeout",
        "10",
    ]);
    let exited: Vec<Value> = events
        .lines()
        .into_iter()
        .filter(|e| e["type"] == "pty.exited")
        .collect();
    assert_eq!(exited.len(), 1, "{events:?}");
    assert_eq!(exited[0]["data"]["status"], 3);
    assert_valid("pty-exited.json", &exited[0]["data"]);
}

#[test]
fn text_goes_to_the_program_as_typed() {
    let home = TestHome::new();
    home.start();
    let argv = if cfg!(windows) {
        json!(["cmd", "/q"])
    } else {
        json!(["cat"])
    };
    let id = spawn(&home, json!({"argv": argv, "cols": 80, "rows": 24}));
    let text = if cfg!(windows) {
        "echo hello\r"
    } else {
        "hello\r"
    };
    let run = call(&home, "pty.write", &json!({"pty_id": id, "text": text}));
    assert_eq!(run.code(), Some(0), "{run:?}");
    eventually("the program echoes it", || {
        screen(&home, &id)["text"]
            .as_str()
            .unwrap()
            .matches("hello")
            .count()
            >= 2
    });
}

#[cfg(unix)]
#[test]
fn a_resize_reaches_the_program() {
    let home = TestHome::new();
    home.start();
    let id = spawn(
        &home,
        json!({"argv": ["sh", "-c", "read x; stty size; sleep 30"], "cols": 80, "rows": 24}),
    );
    let run = call(
        &home,
        "pty.resize",
        &json!({"pty_id": id, "cols": 100, "rows": 30}),
    );
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_eq!(
        call(&home, "pty.write", &json!({"pty_id": id, "text": "\r"})).code(),
        Some(0)
    );
    eventually("stty sees the new size", || {
        screen(&home, &id)["text"]
            .as_str()
            .unwrap()
            .contains("30 100")
    });
    let shown = screen(&home, &id);
    assert_eq!(
        (shown["cols"].clone(), shown["rows"].clone()),
        (json!(100), json!(30))
    );
}

/// The pids in process group `pgid`, by `pgrep`.
#[cfg(unix)]
fn group(pgid: u64) -> Vec<String> {
    let out = Command::new("pgrep")
        .args(["-g", &pgid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

#[cfg(unix)]
#[test]
fn check_4_pty_kill_leaves_no_process_of_the_tree() {
    let home = TestHome::new();
    home.start();
    // The whole tree ignores SIGHUP and SIGTERM, as an ignored signal stays ignored
    // across exec: only the SIGKILL after the grace period ends it.
    let script = "trap '' HUP TERM; sleep 300 & sleep 300 & echo started; wait";
    let id = spawn(
        &home,
        json!({"argv": ["sh", "-c", script], "cols": 80, "rows": 24}),
    );
    eventually("the tree is up", || {
        screen(&home, &id)["text"]
            .as_str()
            .unwrap()
            .contains("started")
    });
    let pid = list(&home)["ptys"][0]["pid"].as_u64().unwrap();
    assert!(group(pid).len() >= 3, "sh and two sleeps: {:?}", group(pid));
    let started = Instant::now();
    assert_eq!(
        call(&home, "pty.kill", &json!({ "pty_id": id })).code(),
        Some(0)
    );
    assert_eq!(
        group(pid),
        Vec::<String>::new(),
        "after {:?}",
        started.elapsed()
    );
    assert_eq!(list(&home)["ptys"], json!([]));
}

#[cfg(unix)]
#[test]
fn rlxd_stop_ends_every_pty() {
    let home = TestHome::new();
    home.start();
    // It ignores the hangup a closing terminal sends: only rlxd's own kill ends it.
    let script = "trap '' HUP; sleep 300";
    let id = spawn(
        &home,
        json!({"argv": ["sh", "-c", script], "cols": 80, "rows": 24}),
    );
    let pid = list(&home)["ptys"][0]["pid"].as_u64().unwrap();
    eventually("the program runs", || !group(pid).is_empty());
    assert_eq!(home.rlxd(&["stop"]).code(), Some(0));
    eventually("the group is gone", || group(pid).is_empty());
    let _ = id;
}

#[test]
fn check_6_each_role_gets_what_it_may() {
    let home = TestHome::new();
    home.start_dev();
    let id = spawn(&home, json!({"argv": ready_argv(), "cols": 80, "rows": 24}));
    let spawn_params = json!({"argv": ready_argv(), "cols": 80, "rows": 24});
    let calls = [
        ("pty.list", json!({})),
        ("pty.screen", json!({ "pty_id": id })),
        ("pty.write", json!({"pty_id": id, "text": "x"})),
        ("pty.resize", json!({"pty_id": id, "cols": 81, "rows": 24})),
        ("pty.spawn", spawn_params),
    ];
    let allowed = |role: Role, method: &str| match role {
        Role::Remote => matches!(method, "pty.list" | "pty.screen" | "pty.write"),
        _ => true,
    };
    for role in [Role::Tui, Role::Remote] {
        let mut connection = connect(&home, role);
        for (method, params) in &calls {
            let reply = request(&mut connection, method, params.clone());
            if allowed(role, method) {
                assert!(reply.is_ok(), "{role:?} {method}: {}", reply.line);
            } else {
                let error = reply.error().unwrap();
                assert_eq!(error.code, "role_not_allowed", "{role:?} {method}");
                let line: Value = serde_json::from_str(&reply.line).unwrap();
                assert_eq!(
                    line["error"]["detail"],
                    json!({"role": "remote", "method": method})
                );
            }
        }
        let kill = request(
            &mut connection,
            "pty.kill",
            json!({"pty_id": "0".repeat(32)}),
        );
        let expected = if role == Role::Remote {
            "role_not_allowed"
        } else {
            "unknown_pty"
        };
        assert_eq!(kill.error().unwrap().code, expected, "{role:?}");
    }

    // The app reads and nothing more.
    let pair = call(&home, "gate.pair", &json!({"device": "Sam MacBook"}));
    assert_eq!(pair.code(), Some(0), "{pair:?}");
    let credential = pair.data()["gate_credential"].as_str().unwrap().to_owned();
    std::fs::write(home.root().join("test-gate-credential"), credential).unwrap();
    for (method, params) in &calls {
        let run = home.rlxd(&["call", "--as-app", method, &params.to_string()]);
        if matches!(*method, "pty.list" | "pty.screen") {
            assert_eq!(run.code(), Some(0), "app {method}: {run:?}");
        } else {
            assert_eq!(
                run.error_code(),
                "role_not_allowed",
                "app {method}: {run:?}"
            );
            assert_eq!(
                run.json()["error"]["detail"],
                json!({"role": "app", "method": method})
            );
        }
    }
    let kill = home.rlxd(&[
        "call",
        "--as-app",
        "pty.kill",
        &json!({ "pty_id": id }).to_string(),
    ]);
    assert_eq!(kill.error_code(), "role_not_allowed");
    // And a cli connection may do everything, killing included.
    assert_eq!(
        call(&home, "pty.kill", &json!({ "pty_id": id })).code(),
        Some(0)
    );
}

#[test]
fn a_home_holds_at_most_64_ptys_exited_ones_included() {
    let home = TestHome::new();
    home.start();
    let argv = if cfg!(windows) {
        json!(["cmd", "/c", "exit 0"])
    } else {
        json!(["true"])
    };
    let ids: Vec<String> = (0..64)
        .map(|_| spawn(&home, json!({"argv": argv, "cols": 20, "rows": 5})))
        .collect();
    let refused = call(
        &home,
        "pty.spawn",
        &json!({"argv": argv, "cols": 20, "rows": 5}),
    );
    assert_eq!(refused.error_code(), "too_many_ptys", "{refused:?}");
    assert_eq!(refused.json()["error"]["detail"], json!({"max": 64}));
    assert_eq!(
        call(&home, "pty.kill", &json!({"pty_id": ids[0]})).code(),
        Some(0)
    );
    spawn(&home, json!({"argv": argv, "cols": 20, "rows": 5}));
}

#[test]
fn a_spawn_that_cannot_start_and_an_unknown_pty_say_why() {
    let home = TestHome::new();
    home.start();
    let missing_dir = home.root().join("no-such-dir");
    for params in [
        json!({"argv": ["rlxd-test-no-such-program"], "cols": 80, "rows": 24}),
        json!({"argv": ready_argv(), "cols": 80, "rows": 24,
               "cwd": missing_dir.to_string_lossy()}),
        json!({"argv": ready_argv(), "cols": 80, "rows": 24, "cwd": "relative"}),
    ] {
        let run = call(&home, "pty.spawn", &params);
        assert_eq!(run.error_code(), "spawn_failed", "{params}: {run:?}");
    }
    let wrong = call(
        &home,
        "pty.spawn",
        &json!({"argv": [], "cols": 80, "rows": 24}),
    );
    assert_eq!(wrong.error_code(), "invalid_params");
    let unknown = call(&home, "pty.screen", &json!({"pty_id": "0".repeat(32)}));
    assert_eq!(unknown.error_code(), "unknown_pty");
    assert_eq!(list(&home)["ptys"], json!([]));
}

#[test]
fn spawn_sets_the_terminal_and_the_environment_asked_for() {
    if cfg!(windows) {
        return;
    }
    let home = TestHome::new();
    home.start();
    let script = "echo \"T=$TERM C=$COLORTERM X=$RLXD_X H=${HOME+set}\"; pwd; sleep 30";
    let dir = std::fs::canonicalize(home.root()).unwrap();
    let id = spawn(
        &home,
        json!({"argv": ["sh", "-c", script], "cols": 80, "rows": 24,
               "cwd": dir.to_string_lossy(), "env": {"RLXD_X": "1", "HOME": null}}),
    );
    eventually("the program prints its environment", || {
        let text = screen(&home, &id)["text"].as_str().unwrap().to_owned();
        text.contains("T=xterm-256color C=truecolor X=1 H=\n")
            && text.contains(&dir.to_string_lossy().to_string())
    });
}

/// The handoff's own check, with `cmp`: tmux and rlxd show the same frame for the same
/// screen, byte for byte.
#[cfg(unix)]
#[test]
fn check_2_the_frame_matches_tmux_byte_for_byte() {
    let required = std::env::var_os("RLXD_TMUX_REQUIRED").is_some_and(|v| v == "1");
    if Command::new("tmux").arg("-V").output().is_err() {
        assert!(!required, "RLXD_TMUX_REQUIRED=1, and tmux is not on PATH");
        eprintln!("tmux is not on PATH: the frame was not compared");
        return;
    }
    let home = TestHome::new();
    home.start();
    let socket = format!("rlxd-cmp-{}", std::process::id());
    let tmux = |args: &[&str]| {
        Command::new("tmux")
            .args(["-L", &socket, "-f", "/dev/null"])
            .args(args)
            .output()
            .unwrap()
    };
    let script = "printf '\\033[31mred\\033[0m plain'; sleep 30";
    assert!(
        tmux(&["new-session", "-d", "-x", "80", "-y", "24", script])
            .status
            .success()
    );
    let id = spawn(
        &home,
        json!({"argv": ["sh", "-c", script], "cols": 80, "rows": 24}),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let (theirs, ours) = loop {
        let theirs = tmux(&["capture-pane", "-e", "-p"]).stdout;
        let ours = screen(&home, &id)["text"]
            .as_str()
            .unwrap()
            .as_bytes()
            .to_vec();
        if (theirs == ours && !theirs.is_empty()) || Instant::now() > deadline {
            break (theirs, ours);
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let _ = tmux(&["kill-server"]);
    assert!(!theirs.is_empty(), "test -s tmux.txt");
    assert_eq!(
        String::from_utf8_lossy(&theirs),
        String::from_utf8_lossy(&ours),
        "cmp tmux.txt rlxd.txt"
    );
    assert!(String::from_utf8_lossy(&ours).starts_with("\x1b[31mred\x1b[39m plain\n"));
}

/// `pty.write`'s keys and pastes reach the program as tmux would send them, through the
/// PTY, in order: a raw-mode program that asked for application cursor keys and
/// bracketed paste prints what it reads, in hex.
#[cfg(unix)]
#[test]
fn check_3_keys_and_a_paste_reach_the_program_as_tmux_sends_them() {
    if Command::new("python3").arg("-V").output().is_err() {
        eprintln!("python3 is not on PATH: the keys were not sent");
        return;
    }
    let home = TestHome::new();
    home.start();
    let script = "import os,tty\ntty.setraw(0)\nos.write(1,b'\\x1b[?1h\\x1b[?2004hREADY\\r\\n')\n\
                  while True:\n    b=os.read(0,4096)\n    if not b: break\n    \
                  os.write(1,b.hex().encode())\n";
    let id = spawn(
        &home,
        json!({"argv": ["python3", "-c", script], "cols": 200, "rows": 40}),
    );
    eventually("the program is ready", || {
        screen(&home, &id)["text"]
            .as_str()
            .unwrap()
            .contains("READY")
    });
    let keys = ["Up", "M-x", "C-c", "Enter", "F5", "BTab", "S-Enter"];
    for params in [
        json!({"pty_id": id, "keys": keys}),
        json!({"pty_id": id, "paste": "one\ntwo"}),
        json!({"pty_id": id, "text": "é"}),
    ] {
        let run = call(&home, "pty.write", &params);
        assert_eq!(run.code(), Some(0), "{run:?}");
    }
    let modes = rlxd::vt::InputModes {
        cursor_keys: true,
        bracketed_paste: true,
        ..Default::default()
    };
    let names: Vec<String> = keys.iter().map(|k| (*k).to_owned()).collect();
    let mut expected = rlxd::pty::keys::send_keys(&names, modes);
    expected.extend(rlxd::pty::paste("one\ntwo", true));
    expected.extend("é".as_bytes());
    let expected: String = expected.iter().map(|b| format!("{b:02x}")).collect();
    eventually("every byte arrives, in order", || {
        let text = screen(&home, &id)["text"].as_str().unwrap().to_owned();
        let after: String = text
            .split_once("READY")
            .map(|(_, rest)| rest.split_whitespace().collect())
            .unwrap_or_default();
        after == expected
    });
}
