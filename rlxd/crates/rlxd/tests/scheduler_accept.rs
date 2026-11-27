//! The scheduler's Accept lines (handoff 0.10.3, 7.6), as written: real one-minute
//! schedules on the system's clock, captured with `rlxd events --follow --until …
//! --timeout …`. Each takes one to three minutes, so they are `#[ignore]`d in the main
//! test run, and rlxd.yml runs them in a job of their own on all three OSes:
//! `cargo test -p rlxd --test scheduler_accept -- --ignored`. `scheduler.rs` has every
//! rule fast, on a test clock.
//!
//! Each test gets a fresh home, fake-claude as Claude Code (`hello` unless it names
//! another), and gate dev mode, so the test client can hold the app role.

mod common;

use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::Duration;

use common::{Run, TestHome, finish_within};
use serde_json::{Value, json};

fn link(root: &Path, scenario: &str) -> PathBuf {
    let dir = root.join("bin").join("fake-claude");
    std::fs::create_dir_all(&dir).unwrap();
    let name = if cfg!(windows) {
        format!("{scenario}.exe")
    } else {
        scenario.to_owned()
    };
    let path = dir.join(name);
    let binary = Path::new(env!("CARGO_BIN_EXE_fake-claude"));
    if std::fs::hard_link(binary, &path).is_err() {
        std::fs::copy(binary, &path).unwrap();
    }
    dir.join(scenario)
}

/// A home running fake-claude `scenario`, started in gate dev mode, the test client
/// paired as the app (then gone again: no app is connected after this).
fn home(scenario: &str) -> TestHome {
    home_with(scenario, &[])
}

/// [`home`], with `envs` set too.
fn home_with(scenario: &str, envs: &[(&str, &str)]) -> TestHome {
    let home = TestHome::new();
    let claude = link(home.root(), scenario);
    let config = home.root().join("claude-config");
    std::fs::create_dir_all(&config).unwrap();
    let user = home.root().join("user-home");
    std::fs::create_dir_all(&user).unwrap();
    let mut home = home
        .with_claude(claude)
        .with_env("CLAUDE_CONFIG_DIR", config.to_str().unwrap())
        .with_env("HOME", user.to_str().unwrap())
        .with_env("USERPROFILE", user.to_str().unwrap());
    for (name, value) in envs {
        home = home.with_env(name, value);
    }
    home.start_dev();
    let paired = home.rlxd(&["call", "--as-app", "status"]);
    assert_eq!(paired.code(), Some(0), "{paired:?}");
    common::eventually("no app is connected", || home.app_clients() == 0);
    home
}

fn seq(home: &TestHome) -> String {
    home.status()["seq"].as_u64().unwrap().to_string()
}

fn call(home: &TestHome, method: &str, params: &str) -> Run {
    home.rlxd(&["call", method, params])
}

/// 7.6's run job `j`, every minute, of `prompt`, with `catch_up`.
fn run_job(j: &str, prompt: &str, catch_up: &str) -> String {
    json!({"job_id": j, "type": "run", "schedule": {"every_minutes": 1},
        "run": {"kind": "task", "prompt": prompt, "mount": {"kind": "no_tools"},
                "space": "default"},
        "catch_up": catch_up, "enabled": true})
    .to_string()
}

fn put(home: &TestHome, job: &str) {
    let run = call(home, "job.put", job);
    assert_eq!(run.code(), Some(0), "{run:?}");
}

/// `rlxd events [--as-app] --topics topics --since s --follow --until until --timeout t`,
/// started.
fn capture(home: &TestHome, app: bool, topics: &str, s: &str, until: &str, t: u64) -> Child {
    let t = t.to_string();
    let mut args = vec!["events"];
    if app {
        args.push("--as-app");
    }
    args.extend([
        "--topics",
        topics,
        "--since",
        s,
        "--follow",
        "--until",
        until,
        "--timeout",
        &t,
    ]);
    let mut child = home.spawn(&args);
    if app {
        // Connected, or done already: an owed fire may come, and end the capture, at once.
        common::eventually("the app is connected", || {
            home.app_clients() >= 1 || child.try_wait().is_ok_and(|done| done.is_some())
        });
    }
    child
}

fn of<'a>(events: &'a [Value], r#type: &str, j: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|e| e["type"] == r#type && e["data"]["job_id"] == j)
        .collect()
}

fn next_fire_in(home: &TestHome, j: &str) -> i64 {
    let list = call(home, "job.list", "{}").data();
    let next = list["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|job| job["job_id"] == j)
        .unwrap()["next_fire_at"]
        .as_str()
        .unwrap()
        .to_owned();
    seconds_until(&next)
}

/// Seconds from now to `at` (RFC 3339, `Z`, as rlxd writes it).
fn seconds_until(at: &str) -> i64 {
    let date: Vec<i64> = at[..10].split('-').map(|n| n.parse().unwrap()).collect();
    let time: Vec<i64> = at[11..19].split(':').map(|n| n.parse().unwrap()).collect();
    let (y, m, d) = (date[0], date[1], date[2]);
    // Days from 1970-01-01 (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let then = days * 86_400 + time[0] * 3600 + time[1] * 60 + time[2];
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap();
    then - now
}

#[test]
#[ignore = "real one-minute schedules: rlxd.yml's scheduler job runs it"]
fn a_fire_with_the_app_connected() {
    fire_with_app(json!({"every_minutes": 1}));
}

#[test]
#[ignore = "real one-minute schedules: rlxd.yml's scheduler job runs it"]
fn cron() {
    fire_with_app(json!({"cron": "* * * * *", "tz": "UTC"}));
}

fn fire_with_app(schedule: Value) {
    let home = home("hello");
    let s = seq(&home);
    let cap = capture(&home, true, "job,run", &s, "run.done", 75);
    let mut job: Value = serde_json::from_str(&run_job("t1", "hi", "skip")).unwrap();
    job["schedule"] = schedule;
    put(&home, &job.to_string());
    let run = finish_within(cap, Duration::from_secs(200));
    assert_eq!(run.code(), Some(0), "{run:?}");
    let events = run.lines();
    let r = of(&events, "job.fired", "t1")
        .first()
        .map(|e| e["data"]["run_id"].clone())
        .unwrap_or_else(|| panic!("never fired: {events:#?}"));
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "run.done" && e["data"]["run_id"] == r),
        "{events:#?}"
    );
}

/// With no app, a job whose template's prompt makes `call` (a read or a write) is
/// skipped `app_absent`, and nothing runs.
fn app_absent(call_kind: &str) {
    let home = home("hello");
    assert_eq!(home.app_clients(), 0);
    let s = seq(&home);
    let cap = capture(&home, false, "job,run", &s, "job.skipped", 75);
    let job = json!({"job_id": "t1", "type": "run", "schedule": {"every_minutes": 1},
        "run": {"kind": "task", "prompt": format!("make one {call_kind} call to Slack"),
                "mount": {"kind": "connectors", "connectors": ["claude.ai Slack"]},
                "space": "default"},
        "catch_up": "once", "enabled": true});
    put(&home, &job.to_string());
    let run = finish_within(cap, Duration::from_secs(200));
    assert_eq!(run.code(), Some(0), "{run:?}");
    let events = run.lines();
    assert!(
        of(&events, "job.skipped", "t1")
            .iter()
            .any(|e| e["data"]["reason"] == "app_absent"),
        "{events:#?}"
    );
    assert!(of(&events, "job.fired", "t1").is_empty());
    assert!(events.iter().all(|e| e["type"] != "run.started"));
}

#[test]
#[ignore = "real one-minute schedules: rlxd.yml's scheduler job runs it"]
fn app_absent_with_a_read_tool() {
    app_absent("read");
}

#[test]
#[ignore = "real one-minute schedules: rlxd.yml's scheduler job runs it"]
fn app_absent_with_a_write_tool() {
    app_absent("write");
}

#[test]
#[ignore = "real one-minute schedules: rlxd.yml's scheduler job runs it"]
fn run_now_needs_the_app_and_catch_up_once_and_skip() {
    for (catch_up, fires) in [("once", 1), ("skip", 0)] {
        let home = home("hello");
        let s = seq(&home);
        let cap = capture(&home, false, "job", &s, "job.skipped", 75);
        put(&home, &run_job("t1", "hi", catch_up));
        assert_eq!(
            finish_within(cap, Duration::from_secs(200)).code(),
            Some(0),
            "{catch_up}: skipped app_absent"
        );
        // Run now needs the app.
        let run = call(&home, "job.run_now", r#"{"job_id":"t1"}"#);
        assert_eq!(run.code(), Some(1), "{run:?}");
        assert_eq!(run.error_code(), "app_absent");
        // Catch-up: connect an app; until 2 s before next_fire_at.
        let s2 = seq(&home);
        let w = next_fire_in(&home, "t1") - 2;
        assert!(w > 5, "{w} s left to the next fire");
        let cap = capture(
            &home,
            true,
            "job",
            &s2,
            "job.fired",
            u64::try_from(w).unwrap(),
        );
        let run = finish_within(cap, Duration::from_secs(200));
        let events = run.lines();
        assert_eq!(
            of(&events, "job.fired", "t1").len(),
            fires,
            "{catch_up}: {events:#?}"
        );
        let expected = if fires == 1 { Some(0) } else { Some(4) };
        assert_eq!(run.code(), expected, "{catch_up}: {run:?}");
    }
}

#[test]
#[ignore = "real one-minute schedules: rlxd.yml's scheduler job runs it"]
fn durable_put_replaces_pause_resume_and_delete() {
    let home = home("hello");
    put(&home, &run_job("t1", "hi", "skip"));
    // Durable.
    home.stop();
    home.start_dev();
    let list = call(&home, "job.list", "{}").data();
    assert!(
        list["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|j| j["job_id"] == "t1")
    );
    // Put replaces.
    let mut five: Value = serde_json::from_str(&run_job("t1", "hi", "skip")).unwrap();
    five["schedule"] = json!({"every_minutes": 5});
    put(&home, &five.to_string());
    let list = call(&home, "job.list", "{}").data();
    assert_eq!(list["jobs"].as_array().unwrap().len(), 1);
    let left = next_fire_in(&home, "t1");
    assert!((290..=300).contains(&left), "about 5 minutes away: {left}");
    put(&home, &run_job("t1", "hi", "skip"));
    // Pause, and a due fire is skipped paused; resume, and the next fires.
    let s = seq(&home);
    let paused = call(&home, "job.pause", r#"{"job_id":"t1"}"#);
    assert_eq!(paused.code(), Some(0), "{paused:?}");
    let cap = capture(&home, true, "job", &s, "job.skipped", 75);
    let events = finish_within(cap, Duration::from_secs(200)).lines();
    assert_eq!(
        of(&events, "job.skipped", "t1")[0]["data"]["reason"],
        "paused",
        "{events:#?}"
    );
    let resumed = call(&home, "job.resume", r#"{"job_id":"t1"}"#);
    assert_eq!(resumed.code(), Some(0));
    let s = seq(&home);
    let cap = capture(&home, true, "job", &s, "job.fired", 75);
    let run = finish_within(cap, Duration::from_secs(200));
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_eq!(of(&run.lines(), "job.fired", "t1").len(), 1);
    // Delete.
    let deleted = call(&home, "job.delete", r#"{"job_id":"t1"}"#);
    assert_eq!(deleted.code(), Some(0));
    let list = call(&home, "job.list", "{}").data();
    assert!(list["jobs"].as_array().unwrap().is_empty());
    let s = seq(&home);
    let cap = capture(&home, true, "job", &s, "job.fired", 75);
    assert_eq!(
        finish_within(cap, Duration::from_secs(200)).code(),
        Some(4),
        "nothing fires"
    );
}

#[test]
#[ignore = "real one-minute schedules: rlxd.yml's scheduler job runs it"]
fn run_now() {
    let home = home("hello");
    put(&home, &run_job("t1", "hi", "skip"));
    let s = seq(&home);
    let cap = capture(&home, true, "run", &s, "run.done", 60);
    let run = home.rlxd(&["call", "--as-app", "job.run_now", r#"{"job_id":"t1"}"#]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let r = run.data()["run_id"].clone();
    assert!(r.is_string());
    let events = finish_within(cap, Duration::from_secs(200)).lines();
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "run.done" && e["data"]["run_id"] == r),
        "{events:#?}"
    );
}

#[test]
#[ignore = "real one-minute schedules: rlxd.yml's scheduler job runs it"]
fn overlap() {
    // The fixture outlasts the minute to the next fire, and then some.
    let home = home_with("slow", &[("FAKE_CLAUDE_SLOW_MS", "150000")]);
    let s = seq(&home);
    let cap = capture(&home, true, "job", &s, "job.skipped", 150);
    put(&home, &run_job("t1", "take your time", "skip"));
    let run = finish_within(cap, Duration::from_secs(200));
    let events = run.lines();
    let skipped = of(&events, "job.skipped", "t1");
    assert!(
        skipped.len() == 1 && skipped[0]["data"]["reason"] == "overlap",
        "{events:#?}"
    );
    let list = call(&home, "job.list", "{}").data();
    let last = list["jobs"][0]["last_run_id"].as_str().unwrap().to_owned();
    call(&home, "run.cancel", &json!({"run_id": last}).to_string());
}

#[test]
#[ignore = "real one-minute schedules: rlxd.yml's scheduler job runs it"]
fn rate_limited() {
    let home = home("rate-limit");
    let s = seq(&home);
    let cap = capture(&home, true, "job", &s, "job.skipped", 150);
    put(&home, &run_job("t1", "hi", "skip"));
    let events = finish_within(cap, Duration::from_secs(200)).lines();
    let skipped = of(&events, "job.skipped", "t1");
    assert!(
        !skipped.is_empty() && skipped[0]["data"]["reason"] == "rate_limited",
        "{events:#?}"
    );
}

#[test]
#[ignore = "real one-minute schedules: rlxd.yml's scheduler job runs it"]
fn missed() {
    for (catch_up, app, want) in [
        ("skip", true, "missed"),
        ("once", true, "fired"),
        ("once", false, "app_absent"),
    ] {
        let home = home("hello");
        put(&home, &run_job("t1", "hi", catch_up));
        // Stop across a fire time.
        let left = next_fire_in(&home, "t1");
        home.stop();
        std::thread::sleep(Duration::from_secs(u64::try_from(left + 3).unwrap_or(63)));
        home.start_dev();
        let until = if want == "fired" {
            "job.fired"
        } else {
            "job.skipped"
        };
        let cap = capture(&home, app, "job", "0", until, 30);
        let run = finish_within(cap, Duration::from_secs(200));
        let events = run.lines();
        match want {
            "fired" => assert_eq!(of(&events, "job.fired", "t1").len(), 1, "{events:#?}"),
            reason => assert_eq!(
                of(&events, "job.skipped", "t1")[0]["data"]["reason"],
                reason,
                "{catch_up}: {events:#?}"
            ),
        }
    }
}
