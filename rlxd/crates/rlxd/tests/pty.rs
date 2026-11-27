//! The PTY host (handoff 0.10.3, 1.4) against a real rlxd in a fresh home: task #28's
//! acceptance, by number where it has one.
//!
//! An app-role connection is a device `gate.pair` paired, in gate dev mode, as the
//! settings tests pair one. A `tui` or `remote` connection says its role in its hello.

mod common;

use std::process::Command;
use std::time::{Duration, Instant};

use common::{TestHome, assert_valid, eventually};
use rlxd_client::proto::{HelloParams, PtySubscribeParams, Role};
use rlxd_client::{Connection, Home, Incoming, Next};
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

/// A program that counts, a line every 200 ms, for as long as it runs.
fn counter_argv() -> Value {
    if cfg!(windows) {
        json!([
            "powershell",
            "-NoProfile",
            "-Command",
            "$i = 0; while ($true) { $i++; \"tick $i\"; Start-Sleep -Milliseconds 200 }"
        ])
    } else {
        json!([
            "sh",
            "-c",
            "i=0; while :; do i=$((i+1)); echo \"tick $i\"; sleep 0.2; done"
        ])
    }
}

/// The highest count on `id`'s screen, 0 before the first.
fn count(home: &TestHome, id: &str) -> u64 {
    let shown = screen(home, id);
    shown["text"]
        .as_str()
        .unwrap()
        .lines()
        .filter_map(|line| line.trim().strip_prefix("tick ")?.parse().ok())
        .max()
        .unwrap_or(0)
}

/// #2479: a panic in a PTY's screen model, which parses what a program writes, ends that
/// PTY, `error: screen_failed`, and neither rlxd nor its other PTYs. A test-only method
/// plants the panic: in the parser on the program's next output, in printing the screen,
/// or in a resize.
#[test]
fn a_panic_in_one_ptys_screen_model_ends_that_pty_alone() {
    let home = TestHome::new();
    home.start();
    let pid = home.status()["pid"].clone();
    let since = home.status()["seq"].as_u64().unwrap().to_string();
    let bystander = spawn(
        &home,
        json!({"argv": counter_argv(), "cols": 80, "rows": 24}),
    );
    let mut failed = Vec::new();
    for at in ["output", "screen", "resize"] {
        let id = spawn(
            &home,
            json!({"argv": counter_argv(), "cols": 80, "rows": 24}),
        );
        eventually("it counts", || count(&home, &id) > 0);
        let run = call(&home, "debug.pty_panic", &json!({"pty_id": id, "at": at}));
        assert_eq!(run.code(), Some(0), "{at}: {run:?}");
        match at {
            // The call that panics is answered, from the blank screen in its place.
            "screen" => assert_eq!(screen(&home, &id)["error"], "screen_failed", "{at}"),
            "resize" => {
                let resize = json!({"pty_id": id, "cols": 100, "rows": 30});
                let run = call(&home, "pty.resize", &resize);
                assert_eq!(run.error_code(), "pty_exited", "{at}: {run:?}");
            }
            _ => {}
        }
        eventually("rlxd ends the pty", || screen(&home, &id)["exited"] == true);
        let shown = screen(&home, &id);
        assert_eq!(shown["error"], "screen_failed", "{at}: {shown}");
        assert!(
            !shown["text"].as_str().unwrap().contains("tick"),
            "{at}: {shown}"
        );
        let run = call(&home, "pty.write", &json!({"pty_id": id, "text": "x"}));
        assert_eq!(run.error_code(), "pty_exited", "{at}: {run:?}");
        #[cfg(unix)]
        {
            let pgid = list(&home)["ptys"]
                .as_array()
                .unwrap()
                .iter()
                .find(|pty| pty["pty_id"] == id.as_str())
                .unwrap()["pid"]
                .as_u64()
                .unwrap();
            eventually("its processes are gone", || group(pgid).is_empty());
        }

        // The same rlxd goes on, and so does the other PTY: its output, screen and size.
        assert_eq!(home.status()["pid"], pid, "{at}");
        let before = count(&home, &bystander);
        eventually("the other pty counts on", || {
            count(&home, &bystander) > before
        });
        let resize = json!({"pty_id": bystander, "cols": 81, "rows": 24});
        assert_eq!(call(&home, "pty.resize", &resize).code(), Some(0), "{at}");
        assert!(screen(&home, &bystander)["error"].is_null(), "{at}");
        failed.push(id);
    }

    let ptys = list(&home)["ptys"].as_array().unwrap().clone();
    assert_eq!(ptys.len(), 4);
    for pty in &ptys {
        let id = pty["pty_id"].as_str().unwrap();
        if id == bystander {
            assert_eq!(
                (&pty["exited"], &pty["error"]),
                (&json!(false), &Value::Null)
            );
        } else {
            assert_eq!(
                (&pty["exited"], &pty["error"]),
                (&json!(true), &json!("screen_failed"))
            );
        }
    }
    let events = home.rlxd(&["events", "--topics", "pty", "--since", &since]);
    let exited: Vec<Value> = events
        .lines()
        .into_iter()
        .filter(|e| e["type"] == "pty.exited")
        .collect();
    assert_eq!(exited.len(), 3, "{events:?}");
    for (event, id) in exited.iter().zip(&failed) {
        assert_eq!(event["data"]["pty_id"], id.as_str());
        assert_eq!(event["data"]["error"], "screen_failed");
        assert_valid("pty-exited.json", &event["data"]);
    }
    let log = home.log();
    assert_eq!(log.matches("the screen model failed").count(), 3, "{log}");
    assert!(log.contains("debug.pty_panic: the screen model panics on the program's output"));
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

/// `ps`'s state for `pid`: "" once there is no such process.
#[cfg(unix)]
fn process_state(pid: u64) -> String {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// #2574 major 2: a process that ended stays a zombie until `pty.kill`, so its pid and
/// group id can be no one else's when the kill signals the group; then it is reaped.
/// `rlxd stop` reaps the ones it holds too.
#[cfg(unix)]
#[test]
fn an_ended_leader_stays_a_zombie_until_pty_kill_reaps_it() {
    let home = TestHome::new();
    home.start();
    let mut pids = Vec::new();
    for code in [3, 4] {
        let argv = json!(["sh", "-c", format!("exit {code}")]);
        let id = spawn(&home, json!({"argv": argv, "cols": 80, "rows": 24}));
        eventually("it ends", || screen(&home, &id)["exited"] == true);
        let pid = list(&home)["ptys"]
            .as_array()
            .unwrap()
            .iter()
            .find(|pty| pty["pty_id"] == id.as_str())
            .unwrap()["pid"]
            .as_u64()
            .unwrap();
        assert!(
            process_state(pid).starts_with('Z'),
            "{}",
            process_state(pid)
        );
        pids.push((id, pid));
    }
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        process_state(pids[0].1).starts_with('Z'),
        "kept, not reaped"
    );
    let run = call(&home, "pty.kill", &json!({ "pty_id": pids[0].0 }));
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_eq!(process_state(pids[0].1), "", "reaped by pty.kill");
    assert!(process_state(pids[1].1).starts_with('Z'));
    home.stop();
    assert_eq!(process_state(pids[1].1), "", "reaped by rlxd stop");
}

/// #2585 minor 7: `pty.kill` doesn't wait for the screen to take a heavy write. rlxd
/// signals the group before it takes the PTY's lock, and the reader lets go within 64
/// bytes. ESC#8 fills the whole screen: at 1000x1000, a million cells for three bytes.
#[cfg(unix)]
#[test]
fn pty_kill_is_quick_while_the_screen_takes_a_heavy_write() {
    let home = TestHome::new();
    home.start();
    let fill = "\\033#8".repeat(16);
    let argv = json!(["sh", "-c", format!("while :; do printf '{fill}'; done")]);
    let id = spawn(&home, json!({"argv": argv, "cols": 1000, "rows": 1000}));
    // Deep in it by now: the program writes far faster than the screen takes it.
    std::thread::sleep(Duration::from_secs(2));
    let start = Instant::now();
    let run = call(&home, "pty.kill", &json!({ "pty_id": id }));
    let took = start.elapsed();
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert!(took < Duration::from_secs(5), "pty.kill took {took:?}");
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

/// #2574 minor 5: the longest write there is, of control characters, which JSON writes
/// in six bytes each, fits a frame, and one byte more is refused, with the connection
/// left open for the next request.
#[cfg(unix)]
#[test]
fn the_longest_write_fits_a_frame_and_a_longer_one_is_refused() {
    let home = TestHome::new();
    home.start();
    let id = spawn(
        &home,
        json!({"argv": ["sh", "-c", "stty raw -echo; cat > /dev/null"], "cols": 80, "rows": 24}),
    );
    let mut connection = connect(&home, Role::Cli);
    let most = "\u{1}".repeat(rlxd_proto::MAX_PTY_WRITE);
    let reply = request(
        &mut connection,
        "pty.write",
        json!({"pty_id": id, "text": most}),
    );
    assert!(reply.is_ok(), "{reply:?}");
    let over = "\u{1}".repeat(rlxd_proto::MAX_PTY_WRITE + 1);
    let reply = request(
        &mut connection,
        "pty.write",
        json!({"pty_id": id, "text": over}),
    );
    assert_eq!(reply.error().unwrap().code, "invalid_params", "{reply:?}");
    let reply = request(&mut connection, "pty.list", json!({}));
    assert!(reply.is_ok(), "the connection goes on: {reply:?}");
}

/// #2574 minor 6: a program that never reads its input fills rlxd's queue for it, and
/// then a write gets `pty_busy`, a code of its own, not `internal`.
#[cfg(unix)]
#[test]
fn a_program_that_reads_nothing_makes_a_write_busy() {
    let home = TestHome::new();
    home.start();
    let id = spawn(
        &home,
        json!({"argv": ["sh", "-c", "stty raw -echo; sleep 60"], "cols": 80, "rows": 24}),
    );
    let mut connection = connect(&home, Role::Cli);
    let most = "x".repeat(rlxd_proto::MAX_PTY_WRITE);
    let busy = (0..200).find_map(|_| {
        let reply = request(
            &mut connection,
            "pty.write",
            json!({"pty_id": id, "text": most}),
        );
        reply.error().cloned()
    });
    let busy = busy.expect("a write is refused once 16 MiB wait");
    assert_eq!(busy.code, "pty_busy", "{busy:?}");
}

/// #2574 minor 8: a screen with more history than a client reads in one line (16 MiB,
/// which JSON's escapes reach sooner) is refused as `screen_too_long`, and the screen
/// without its history still comes.
#[cfg(unix)]
#[test]
fn a_screen_longer_than_a_client_reads_is_refused() {
    let home = TestHome::new();
    home.start();
    // Each cell another colour pair: the capture writes both for every cell.
    let program = r#"
import sys
a = "\x1b[38;2;200;10;10;48;2;10;200;10mx"
b = "\x1b[38;2;10;10;200;48;2;200;200;10my"
line = (a + b) * 40 + "\x1b[0m\r\n"
sys.stdout.write(line * 7000)
sys.stdout.flush()
print("done")
import time; time.sleep(60)
"#;
    let id = spawn(
        &home,
        json!({"argv": ["python3", "-c", program], "cols": 80, "rows": 24}),
    );
    eventually("the history fills", || {
        let shown = screen(&home, &id);
        shown["history"].as_u64().unwrap() >= 6900
            && shown["text"].as_str().unwrap().contains("done")
    });
    let all = call(
        &home,
        "pty.screen",
        &json!({"pty_id": id, "scrollback": 50_000}),
    );
    assert_eq!(all.error_code(), "screen_too_long", "{all:?}");
    let some = call(
        &home,
        "pty.screen",
        &json!({"pty_id": id, "scrollback": 100}),
    );
    assert_eq!(some.code(), Some(0), "{some:?}");
}

/// The #28 tester's minor: `rlxd call pty.subscribe` prints the stream, a
/// `pty.snapshot` first, as `rlxd call events.subscribe` does, and ends with the PTY.
#[test]
fn rlxd_call_prints_a_pty_stream() {
    let home = TestHome::new();
    home.start();
    let id = spawn(&home, json!({"argv": ready_argv(), "cols": 80, "rows": 24}));
    let params = json!({"pty_id": id, "read_only": true}).to_string();
    let stream = home.spawn(&["call", "pty.subscribe", &params]);
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(
        call(&home, "pty.kill", &json!({ "pty_id": id })).code(),
        Some(0)
    );
    let run = common::finish(stream);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let first: Value = serde_json::from_str(run.stdout.lines().next().unwrap()).unwrap();
    assert_eq!(first["type"], "pty.snapshot", "{run:?}");
    assert!(run.stdout.contains("\"pty.exited\""), "{run:?}");
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

/// #2574 major 3: a PTY's process doesn't inherit what ties rlxd's starter to a session,
/// a board, an agent or a tmux server, as the fleet's tmux server gives a window none of
/// it; `env` can put it back.
#[test]
fn spawn_leaves_out_what_ties_rlxds_starter_to_a_session() {
    if cfg!(windows) {
        return;
    }
    let mut home = TestHome::new();
    for (name, value) in [
        ("CLAUDECODE", "1"),
        ("TERM_PROGRAM", "vscode"),
        ("TERM_PROGRAM_VERSION", "1.99"),
        ("TMUX", "/private/tmp/tmux-501/asq,1,0"),
        ("TMUX_PANE", "%9"),
        ("AISQUARE_PERSONA", "Ava"),
        ("AISQUARE_BROKERED_SCOPE", "x"),
        ("ANTHROPIC_CUSTOM_HEADERS", "x-run: 1"),
        ("CODEX_THREAD_ID", "t1"),
        ("CODEX_HOME", "/somewhere/codex"),
        ("RLXD_KEPT", "yes"),
    ] {
        home = home.with_env(name, value);
    }
    home.start();
    let names = "CLAUDECODE TERM_PROGRAM TERM_PROGRAM_VERSION TMUX TMUX_PANE AISQUARE_PERSONA \
                 AISQUARE_BROKERED_SCOPE ANTHROPIC_CUSTOM_HEADERS CODEX_THREAD_ID CODEX_HOME \
                 RLXD_KEPT";
    let script = format!("for v in {names}; do eval \"echo $v=\\${{$v-unset}}\"; done; sleep 30");
    let id = spawn(
        &home,
        json!({"argv": ["sh", "-c", script], "cols": 80, "rows": 24,
               "env": {"TMUX_PANE": "%1"}}),
    );
    let expected = [
        "CLAUDECODE=unset",
        "TERM_PROGRAM=unset",
        "TERM_PROGRAM_VERSION=unset",
        "TMUX=unset",
        "TMUX_PANE=%1",
        "AISQUARE_PERSONA=unset",
        "AISQUARE_BROKERED_SCOPE=unset",
        "ANTHROPIC_CUSTOM_HEADERS=unset",
        "CODEX_THREAD_ID=unset",
        "CODEX_HOME=unset",
        "RLXD_KEPT=yes",
    ];
    let shown = || screen(&home, &id)["text"].as_str().unwrap().to_owned();
    eventually("the program prints its environment", || {
        shown().contains("RLXD_KEPT=")
    });
    let lines: Vec<String> = shown().lines().map(str::to_owned).collect();
    assert_eq!(lines[..expected.len()], expected, "{lines:?}");
}

/// The lists rlxd takes out of a PTY's environment (`rlxd::pty::env`) are the fleet's,
/// as core/spawn.py has them, and both leave the same of one environment: checked
/// against Python's own, through `uv run python`, so the two can't drift. It skips
/// unless `RLXD_INTEROP_REQUIRED` or `RLXD_BIN` is set, as registry_interop.rs does.
#[test]
fn the_environment_rlxd_takes_out_is_the_fleets() {
    use rlxd::pty::env;
    if !["RLXD_INTEROP_REQUIRED", "RLXD_BIN"]
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|v| !v.is_empty()))
    {
        eprintln!("skipped: set RLXD_INTEROP_REQUIRED=1 to run Python's side (rlxd.yml does)");
        return;
    }
    const PYTHON: &str = r#"
import json, sys
from aisquare.core import spawn
from aisquare.core.agent_adapters import adapters
bases = json.loads(sys.stdin.read())
print(json.dumps({
    "identity": list(spawn.IDENTITY_ENV_VARS),
    "team": list(spawn.TEAM_ENV_VARS),
    "agent": list(spawn.agent_identity_env_vars()),
    "homes": [[a.home_env, [n for n in (a.capabilities.tool_shell_env, *a.capabilities.tool_shell_vars) if n]]
              for a in adapters() if a.home_env],
    "kept": [sorted(spawn.tmux_client_env(base)) for base in bases],
}))
"#;
    let names: Vec<&str> = [env::IDENTITY, env::TEAM, env::AGENT, env::TERMINAL]
        .concat()
        .into_iter()
        .chain(
            env::HOMES
                .iter()
                .flat_map(|(home, markers)| std::iter::once(*home).chain(markers.iter().copied())),
        )
        .chain([
            "AISQUARE_BROKERED_X",
            "AISQUARE_HOME",
            "AISQUARE_TEAM",
            "PATH",
            "RLXD_KEPT",
        ])
        .collect();
    // One as a person's shell is, one as a Codex tool shell is.
    let plain: serde_json::Map<String, Value> = names
        .iter()
        .filter(|name| !name.starts_with("CODEX_") || **name == "CODEX_HOME")
        .map(|name| ((*name).to_owned(), json!("v")))
        .collect();
    let codex: serde_json::Map<String, Value> = names
        .iter()
        .map(|name| ((*name).to_owned(), json!("v")))
        .collect();
    let bases = vec![plain, codex];
    let repo = common::workspace().parent().unwrap().to_path_buf();
    let mut python = Command::new("uv")
        .args(["run", "--quiet", "python", "-c", PYTHON])
        .current_dir(&repo)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("uv must run here (RLXD_INTEROP_REQUIRED is set): {e}"));
    use std::io::Write as _;
    python
        .stdin
        .take()
        .unwrap()
        .write_all(serde_json::to_string(&bases).unwrap().as_bytes())
        .unwrap();
    let out = python.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let python: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        python["identity"],
        json!(env::IDENTITY),
        "IDENTITY_ENV_VARS"
    );
    assert_eq!(python["team"], json!(env::TEAM), "TEAM_ENV_VARS");
    assert_eq!(
        python["agent"],
        json!(env::AGENT),
        "agent_identity_env_vars()"
    );
    let homes: Vec<Value> = env::HOMES
        .iter()
        .map(|(home, markers)| json!([home, markers]))
        .collect();
    assert_eq!(python["homes"], json!(homes), "tool_shell_homes's agents");
    for (n, base) in bases.iter().enumerate() {
        let ours: Vec<String> = env::inherited(
            base.iter()
                .map(|(k, v)| (k.into(), v.as_str().unwrap().into())),
        )
        .into_iter()
        .map(|(k, _)| k.into_string().unwrap())
        .collect();
        let mut theirs: Vec<String> = python["kept"][n]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k.as_str().unwrap().to_owned())
            .filter(|k| !env::TERMINAL.contains(&k.as_str()))
            .collect();
        let mut ours = ours;
        ours.sort();
        theirs.sort();
        assert_eq!(
            ours, theirs,
            "base {n}: rlxd keeps what tmux_client_env keeps, less tmux's own"
        );
    }
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

/// The events of a `pty.subscribe` stream, until `stop` says so or it ends.
#[cfg(unix)]
fn stream_until(
    subscription: &mut rlxd_client::Subscription,
    mut stop: impl FnMut(&Value) -> bool,
) -> Vec<Value> {
    let mut events = Vec::new();
    loop {
        match subscription.next_frame().unwrap() {
            Next::Event(line) => {
                let event: Value = serde_json::from_str(&line.line).unwrap();
                let done = stop(&event);
                events.push(event);
                if done {
                    return events;
                }
            }
            Next::End(reply) => {
                events.push(serde_json::from_str(&reply.line).unwrap());
                return events;
            }
        }
    }
}

#[cfg(unix)]
fn output_text(events: &[Value]) -> String {
    events
        .iter()
        .filter(|e| e["type"] == "pty.output")
        .map(|e| {
            let bytes = rlxd::pty::unbase64(e["data"]["bytes_b64"].as_str().unwrap()).unwrap();
            String::from_utf8_lossy(&bytes).into_owned()
        })
        .collect()
}

#[cfg(unix)]
#[test]
fn pty_subscribe_streams_its_screen_then_its_output_and_end() {
    let home = TestHome::new();
    home.start();
    let id = spawn(
        &home,
        json!({"argv": ["sh", "-c", "echo one; read x; echo two; sleep 1; exit 4"],
               "cols": 80, "rows": 24}),
    );
    // Another PTY's output goes on meanwhile, and stays out of this stream.
    let other = spawn(
        &home,
        json!({"argv": ["sh", "-c", "while :; do echo noise; sleep 0.05; done"],
               "cols": 80, "rows": 24}),
    );
    eventually("the first line", || {
        screen(&home, &id)["text"].as_str().unwrap().contains("one")
    });
    let params = PtySubscribeParams::new(&id, true).unwrap();
    let mut stream = connect(&home, Role::Tui).subscribe_pty(&params).unwrap();
    let first = stream_until(&mut stream, |_| true);
    assert_eq!(first[0]["type"], "pty.snapshot", "{first:?}");
    assert_valid("pty-screen-reply.json", &first[0]["data"]);
    assert!(first[0]["data"]["text"].as_str().unwrap().contains("one"));
    assert_eq!(first[0]["seq"], first[0]["data"]["seq"]);
    assert_eq!(
        call(&home, "pty.write", &json!({"pty_id": id, "text": "\r"})).code(),
        Some(0)
    );
    let events = stream_until(&mut stream, |e| e["type"] == "pty.exited");
    let exited = events.last().unwrap();
    assert_eq!(exited["data"], json!({"pty_id": id, "status": 4}));
    for event in &events {
        assert_eq!(
            event["data"]["pty_id"],
            id.as_str(),
            "only this PTY's: {event}"
        );
        assert_valid("event.json", event);
    }
    assert!(output_text(&events).contains("two"), "{events:?}");
    let seqs: Vec<u64> = std::iter::once(&first[0])
        .chain(&events)
        .map(|e| e["seq"].as_u64().unwrap())
        .collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "seqs rise: {seqs:?}");
    // Killed, the PTY ends the stream with its reply.
    assert_eq!(
        call(&home, "pty.kill", &json!({ "pty_id": id })).code(),
        Some(0)
    );
    let end = stream_until(&mut stream, |_| false);
    let reply = end.last().unwrap();
    assert_eq!(reply["ok"], true, "{end:?}");
    assert!(reply["data"]["seq"].as_u64().unwrap() >= *seqs.last().unwrap());
    assert_eq!(
        call(&home, "pty.kill", &json!({ "pty_id": other })).code(),
        Some(0)
    );
}

#[test]
fn a_read_only_watcher_may_not_write_and_only_cli_and_tui_may_watch_to_write() {
    let home = TestHome::new();
    home.start_dev();
    let id = spawn(&home, json!({"argv": ready_argv(), "cols": 80, "rows": 24}));
    let params = PtySubscribeParams::new(&id, true).unwrap();
    let mut stream = connect(&home, Role::Tui).subscribe_pty(&params).unwrap();
    assert!(matches!(stream.next_frame().unwrap(), Next::Event(_)));
    let mut text = serde_json::Map::new();
    text.insert("pty_id".to_owned(), json!(id));
    text.insert("text".to_owned(), json!("x"));
    stream.send("pty.write", text).unwrap();
    let refused = loop {
        if let Incoming::Reply(reply) = stream.next_incoming().unwrap() {
            break reply;
        }
    };
    assert_eq!(
        refused.error().unwrap().code,
        "read_only",
        "{}",
        refused.line
    );
    // Another connection, not watching, may.
    assert_eq!(
        call(&home, "pty.write", &json!({"pty_id": id, "text": "x"})).code(),
        Some(0)
    );

    let writable = PtySubscribeParams::new(&id, false).unwrap();
    let mut refused = connect(&home, Role::Remote)
        .subscribe_pty(&writable)
        .unwrap();
    match refused.next_frame().unwrap() {
        Next::End(reply) => {
            assert_eq!(
                reply.error().unwrap().code,
                "role_not_allowed",
                "{}",
                reply.line
            );
        }
        Next::Event(event) => panic!("remote watched to write: {}", event.line),
    }
    let pair = call(&home, "gate.pair", &json!({"device": "Sam MacBook"}));
    let credential = pair.data()["gate_credential"].as_str().unwrap().to_owned();
    let app = || {
        let home = Home::at(home.root());
        let token = std::fs::read_to_string(home.token()).unwrap();
        let mut connection = Connection::open(&home).unwrap();
        let hello = HelloParams::new(token.trim(), "pty-test", "0", Role::App)
            .unwrap()
            .with_gate_credential(credential.clone())
            .unwrap();
        assert!(connection.hello(&hello).unwrap().is_ok());
        connection
    };
    // #2574's nit: remote and app may watch read-only, and get the screen first.
    for (role, connection) in [("remote", connect(&home, Role::Remote)), ("app", app())] {
        let mut watching = connection.subscribe_pty(&params).unwrap();
        match watching.next_frame().unwrap() {
            Next::Event(event) => {
                let event: Value = serde_json::from_str(&event.line).unwrap();
                assert_eq!(event["type"], "pty.snapshot", "{role}");
            }
            Next::End(reply) => panic!("{role} may watch read-only: {}", reply.line),
        }
    }
    let writable = PtySubscribeParams::new(&id, false).unwrap();
    let mut refused = app().subscribe_pty(&writable).unwrap();
    match refused.next_frame().unwrap() {
        Next::End(reply) => assert_eq!(reply.error().unwrap().code, "role_not_allowed"),
        Next::Event(event) => panic!("the app watched to write: {}", event.line),
    }
}

/// rlxd's resident memory, in KiB.
#[cfg(unix)]
fn rss_kib(pid: u64) -> u64 {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

#[cfg(unix)]
#[test]
fn check_5_a_stalled_subscriber_holds_up_neither_the_pty_nor_memory() {
    let home = TestHome::new();
    home.start();
    let rlxd_pid = home.status()["pid"].as_u64().unwrap();
    let since = home.status()["seq"].as_u64().unwrap();
    let (first, last) = home.emit(50);
    assert_eq!(last - first + 1, 50);
    let script = "read x; yes 'the quick brown fox jumps over the lazy dog 0123456789 abcdefghij' \
                  | head -c 50000000; echo DONE; sleep 60";
    let id = spawn(
        &home,
        json!({"argv": ["sh", "-c", script], "cols": 80, "rows": 24}),
    );
    // A subscriber that never reads, while 50 MB go by.
    let params = PtySubscribeParams::new(&id, true).unwrap();
    let mut stalled = connect(&home, Role::Tui).subscribe_pty(&params).unwrap();
    let before = rss_kib(rlxd_pid);
    assert_eq!(
        call(&home, "pty.write", &json!({"pty_id": id, "text": "\r"})).code(),
        Some(0)
    );
    let deadline = Instant::now() + Duration::from_secs(240);
    let mut slowest = Duration::ZERO;
    loop {
        let asked = Instant::now();
        let text = screen(&home, &id)["text"].as_str().unwrap().to_owned();
        slowest = slowest.max(asked.elapsed());
        if text.contains("DONE") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the PTY never got through its output"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    let after = rss_kib(rlxd_pid);
    assert!(
        slowest < Duration::from_secs(2),
        "pty.screen took {slowest:?}"
    );
    assert!(
        after < before + 400 * 1024,
        "rlxd grew from {before} KiB to {after} KiB for a stalled subscriber"
    );
    // The daemon topic lost nothing to the flood: its replay is whole, with no gap.
    let replay = home.rlxd(&[
        "events",
        "--topics",
        "daemon",
        "--since",
        &since.to_string(),
    ]);
    let debug: Vec<Value> = replay
        .lines()
        .into_iter()
        .filter(|e| e["type"] == "daemon.debug" || e["type"] == "daemon.gap")
        .collect();
    assert_eq!(debug.len(), 50, "{:?}", debug.first());
    assert!(debug.iter().all(|e| e["type"] == "daemon.debug"));
    // The stalled subscriber, reading again, is put back on the screen with a snapshot,
    // never a gap.
    let resumed = stream_until(&mut stalled, |e| {
        e["type"] == "pty.snapshot" && e["data"]["text"].as_str().unwrap().contains("DONE")
            || e["type"] == "daemon.gap"
    });
    assert!(resumed.iter().all(|e| e["type"] != "daemon.gap"));
    let snapshots = resumed
        .iter()
        .filter(|e| e["type"] == "pty.snapshot")
        .count();
    assert!(
        snapshots >= 2,
        "the first, and one after the output it missed"
    );
}

/// The end of a process comes after its last output: a burst right before it exits
/// still reaches the stream, and the screen, before `pty.exited`.
#[cfg(unix)]
#[test]
fn pty_exited_comes_after_every_byte_the_process_wrote() {
    let home = TestHome::new();
    home.start();
    for round in 0..5 {
        let script = "read x; yes y | head -c 2000000; echo END; exit 7";
        let id = spawn(
            &home,
            json!({"argv": ["sh", "-c", script], "cols": 80, "rows": 24}),
        );
        let params = PtySubscribeParams::new(&id, true).unwrap();
        let mut stream = connect(&home, Role::Tui).subscribe_pty(&params).unwrap();
        assert_eq!(
            call(&home, "pty.write", &json!({"pty_id": id, "text": "\r"})).code(),
            Some(0)
        );
        let events = stream_until(&mut stream, |e| e["type"] == "pty.exited");
        assert_eq!(events.last().unwrap()["data"]["status"], 7, "round {round}");
        let text = output_text(&events);
        assert!(
            text.ends_with("END\r\n"),
            "round {round}: the output ends {:?}",
            &text[text.len().saturating_sub(20)..]
        );
        let shown = screen(&home, &id);
        assert!(
            shown["text"].as_str().unwrap().contains("END"),
            "round {round}"
        );
        assert_eq!(
            call(&home, "pty.kill", &json!({ "pty_id": id })).code(),
            Some(0)
        );
    }
}

/// `rlxd attach` in a PTY of its own, as a user's terminal: it shows the PTY it attaches
/// to, types into it, sizes it to its own terminal, and Ctrl-] detaches.
#[cfg(unix)]
#[test]
fn rlxd_attach_puts_a_pty_in_a_terminal_and_types_into_it() {
    let home = TestHome::new();
    home.start();
    let inner = spawn(
        &home,
        json!({"argv": ["sh", "-c", "echo INNER; cat"], "cols": 80, "rows": 24}),
    );
    eventually("the inner PTY starts", || {
        screen(&home, &inner)["text"]
            .as_str()
            .unwrap()
            .contains("INNER")
    });
    let rlxd = env!("CARGO_BIN_EXE_rlxd");
    let root = home.root().to_string_lossy().to_string();
    let attach = |extra: &[&str]| {
        let mut argv = vec![rlxd, "attach"];
        argv.extend(extra);
        argv.push(&inner);
        spawn(
            &home,
            json!({"argv": argv, "cols": 100, "rows": 30, "env": {"RLX_HOME": root}}),
        )
    };
    let outer = attach(&[]);
    eventually("attach shows the inner screen", || {
        screen(&home, &outer)["text"]
            .as_str()
            .unwrap()
            .contains("INNER")
    });
    eventually("the inner PTY takes attach's size", || {
        list(&home)["ptys"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["pty_id"] == inner.as_str() && p["cols"] == 100 && p["rows"] == 30)
    });
    for params in [
        json!({"pty_id": outer, "text": "hello"}),
        json!({"pty_id": outer, "keys": ["Enter"]}),
    ] {
        assert_eq!(call(&home, "pty.write", &params).code(), Some(0));
    }
    eventually("cat gets the line, and attach shows it", || {
        let inside = screen(&home, &inner)["text"]
            .as_str()
            .unwrap()
            .matches("hello")
            .count();
        let shown = screen(&home, &outer)["text"]
            .as_str()
            .unwrap()
            .matches("hello")
            .count();
        inside >= 2 && shown >= 2
    });
    // #2574 minor 4: a no-break space or an ideographic space, which no key name
    // holds, goes as text, and attach goes on.
    for params in [
        json!({"pty_id": outer, "text": "a\u{a0}b\u{3000}c"}),
        json!({"pty_id": outer, "keys": ["Enter"]}),
    ] {
        assert_eq!(call(&home, "pty.write", &params).code(), Some(0));
    }
    eventually("cat gets the spaces", || {
        screen(&home, &inner)["text"]
            .as_str()
            .unwrap()
            .contains("a\u{a0}b\u{3000}c")
    });
    assert_eq!(screen(&home, &outer)["exited"], false, "attach goes on");
    assert_eq!(
        call(
            &home,
            "pty.write",
            &json!({"pty_id": outer, "text": "\u{1d}"})
        )
        .code(),
        Some(0)
    );
    eventually("Ctrl-] detaches", || {
        screen(&home, &outer)["exited"] == true
    });
    let ended = screen(&home, &outer);
    assert_eq!(ended["status"], 0, "{ended}");
    // #2574's nit: a PTY that ended before attach came is said to have, at once.
    let done = spawn(
        &home,
        json!({"argv": ["sh", "-c", "exit 3"], "cols": 80, "rows": 24}),
    );
    eventually("it ends", || screen(&home, &done)["exited"] == true);
    let mut argv = vec![rlxd, "attach"];
    argv.push(&done);
    let late = spawn(
        &home,
        json!({"argv": argv, "cols": 100, "rows": 30, "env": {"RLX_HOME": root}}),
    );
    eventually("attach says it ended", || {
        let shown = screen(&home, &late);
        shown["exited"] == true
            && shown["text"]
                .as_str()
                .unwrap()
                .contains("[the process exited, status 3]")
    });
    assert!(
        ended["text"].as_str().unwrap().contains("[detached]"),
        "{ended}"
    );
    assert_eq!(
        screen(&home, &inner)["exited"],
        false,
        "detaching leaves the PTY running"
    );

    // Read-only, it watches and types nothing.
    let watcher = attach(&["--read-only"]);
    eventually("the watcher shows the screen", || {
        screen(&home, &watcher)["text"]
            .as_str()
            .unwrap()
            .contains("INNER")
    });
    let before = screen(&home, &inner)["text"]
        .as_str()
        .unwrap()
        .matches("typed")
        .count();
    assert_eq!(
        call(
            &home,
            "pty.write",
            &json!({"pty_id": watcher, "text": "typed\r"})
        )
        .code(),
        Some(0)
    );
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        screen(&home, &inner)["text"]
            .as_str()
            .unwrap()
            .matches("typed")
            .count(),
        before
    );
    assert_eq!(
        call(
            &home,
            "pty.write",
            &json!({"pty_id": watcher, "text": "\u{1d}"})
        )
        .code(),
        Some(0)
    );
    eventually("the watcher detaches", || {
        screen(&home, &watcher)["exited"] == true
    });
}

/// On Windows, `rlxd __pty-shim` runs only in a job rlxd made.
#[cfg(windows)]
#[test]
fn windows_the_shim_refuses_to_run_without_its_job() {
    let home = TestHome::new();
    let run = home.rlxd(&[
        "__pty-shim",
        "--job",
        "Local\\rlxd-pty-nope",
        "--",
        "cmd",
        "/c",
        "exit 0",
    ]);
    assert_eq!(run.code(), Some(2), "{run:?}");
    assert!(run.stderr.contains("no job"), "{run:?}");
    let not_ours = home.rlxd(&[
        "__pty-shim",
        "--job",
        "Global\\other",
        "--",
        "cmd",
        "/c",
        "exit 0",
    ]);
    assert_eq!(not_ours.code(), Some(2), "{not_ours:?}");
}

/// On Windows, the program runs inside its PTY's Job Object: it opens the job by the name
/// the shim gave it, and asks.
#[cfg(windows)]
#[test]
fn windows_the_program_runs_inside_its_ptys_job() {
    let home = TestHome::new();
    home.start();
    let script = "Add-Type -Namespace W -Name K -MemberDefinition '\
        [DllImport(\"kernel32.dll\", CharSet=CharSet.Unicode)] public static extern IntPtr OpenJobObjectW(uint a, bool i, string n); \
        [DllImport(\"kernel32.dll\")] public static extern bool IsProcessInJob(IntPtr p, IntPtr j, out bool r); \
        [DllImport(\"kernel32.dll\")] public static extern IntPtr GetCurrentProcess();'; \
        $j = [W.K]::OpenJobObjectW(4, $false, $env:RLXD_PTY_JOB); $r = $false; \
        [void][W.K]::IsProcessInJob([W.K]::GetCurrentProcess(), $j, [ref]$r); \
        Write-Output \"INJOB=$r\"; Start-Sleep 30";
    let id = spawn(
        &home,
        json!({"argv": ["powershell", "-NoProfile", "-Command", script], "cols": 120, "rows": 24}),
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let text = screen(&home, &id)["text"].as_str().unwrap().to_owned();
        if text.contains("INJOB=True") {
            break;
        }
        assert!(!text.contains("INJOB=False"), "{text}");
        assert!(Instant::now() < deadline, "no answer: {text}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// On Windows, an exit code above 255 comes through whole.
#[cfg(windows)]
#[test]
fn windows_an_exit_code_above_255_comes_through() {
    let home = TestHome::new();
    home.start();
    for code in [1000u64, 70000] {
        let argv = json!(["cmd", "/c", format!("exit {code}")]);
        let id = spawn(&home, json!({"argv": argv, "cols": 80, "rows": 24}));
        eventually("the process ends", || screen(&home, &id)["exited"] == true);
        assert_eq!(screen(&home, &id)["status"], code);
    }
}

/// The pid's process, by `tasklist`: whether it still runs.
#[cfg(windows)]
fn running(pid: &str) -> bool {
    let out = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
}

/// On Windows, pty.kill ends the program and what it started, by its job.
#[cfg(windows)]
#[test]
fn windows_pty_kill_ends_the_whole_tree() {
    let home = TestHome::new();
    home.start();
    let script = "$c = Start-Process -FilePath powershell -ArgumentList '-NoProfile','-Command','Start-Sleep 300' \
                  -NoNewWindow -PassThru; Write-Output \"PIDS $PID $($c.Id) END\"; Start-Sleep 300";
    let id = spawn(
        &home,
        json!({"argv": ["powershell", "-NoProfile", "-Command", script], "cols": 120, "rows": 24}),
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    let pids: Vec<String> = loop {
        let text = screen(&home, &id)["text"].as_str().unwrap().to_owned();
        if let Some(line) = text
            .lines()
            .find(|l| l.starts_with("PIDS ") && l.contains(" END"))
        {
            break line
                .split_whitespace()
                .skip(1)
                .take(2)
                .map(str::to_owned)
                .collect();
        }
        assert!(Instant::now() < deadline, "no pids: {text}");
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(pids.iter().all(|pid| running(pid)), "{pids:?}");
    assert_eq!(
        call(&home, "pty.kill", &json!({ "pty_id": id })).code(),
        Some(0)
    );
    for pid in &pids {
        assert!(!running(pid), "pid {pid} survived pty.kill");
    }
}

/// On Windows, Ctrl-C reaches the program, and the shim lives on to pass its end on.
#[cfg(windows)]
#[test]
fn windows_ctrl_c_reaches_the_program() {
    let home = TestHome::new();
    home.start();
    let id = spawn(
        &home,
        json!({"argv": ["ping", "-t", "127.0.0.1"], "cols": 120, "rows": 30}),
    );
    eventually("ping pings", || {
        screen(&home, &id)["text"]
            .as_str()
            .unwrap()
            .contains("127.0.0.1")
    });
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        call(&home, "pty.write", &json!({"pty_id": id, "keys": ["C-c"]})).code(),
        Some(0)
    );
    eventually("ping stops", || screen(&home, &id)["exited"] == true);
    let text = screen(&home, &id)["text"].as_str().unwrap().to_owned();
    assert!(
        text.contains("statistics"),
        "ping, not the shim, took the Ctrl-C: {text}"
    );
}
