//! The scheduler's rules (handoff 0.10.3, 2.3 and 7.6), fast: each rlxd runs on a test
//! clock (`RLXD_TEST_CLOCK`), which only `debug.clock_advance` and `debug.clock_set` move,
//! and which answer once every decision due by then is made. So a minute's schedule takes
//! a call, not a minute. The 7.6 Accept lines on real one-minute schedules are in
//! `scheduler_accept.rs`.
//!
//! Each test gets a fresh home with its own `CLAUDE_CONFIG_DIR` and user home, fake-claude
//! standing in for Claude Code, and gate dev mode, so the test client can hold the app
//! role: an "app connected" is a `rlxd events --as-app --follow` left running.

mod common;

use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

use common::{TestHome, assert_valid};
use serde_json::{Value, json};

/// The test clock's start, for most tests.
const T0: &str = "2027-01-01T00:00:00Z";

/// fake-claude, linked (or copied) to `<home>/bin/fake-claude/<scenario>`.
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

/// A home whose rlxd runs fake-claude `scenario` on a test clock at `clock`, not started.
fn unstarted(scenario: &str, clock: &str) -> TestHome {
    unstarted_with(scenario, clock, &[])
}

/// [`unstarted`], with `envs` set too.
fn unstarted_with(scenario: &str, clock: &str, envs: &[(&str, &str)]) -> TestHome {
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
        .with_env("USERPROFILE", user.to_str().unwrap())
        .with_env("RLXD_TEST_CLOCK", clock);
    for (name, value) in envs {
        home = home.with_env(name, value);
    }
    home
}

/// [`unstarted`], started in gate dev mode, the test client paired as the app.
fn home_at(scenario: &str, clock: &str) -> TestHome {
    let home = unstarted(scenario, clock);
    start(&home);
    home
}

fn start(home: &TestHome) {
    home.start_dev();
    let paired = home.rlxd(&["call", "--as-app", "status"]);
    assert_eq!(paired.code(), Some(0), "{paired:?}");
}

/// An app gate client, connected until dropped.
struct App<'a> {
    home: &'a TestHome,
    child: Child,
}

impl Drop for App<'_> {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        common::eventually("the app is gone", || self.home.app_clients() == 0);
    }
}

fn app(home: &TestHome) -> App<'_> {
    let since = seq(home).to_string();
    let child = home.spawn(&[
        "events", "--as-app", "--topics", "daemon", "--since", &since, "--follow",
    ]);
    common::eventually("the app is connected", || home.app_clients() == 1);
    App { home, child }
}

fn seq(home: &TestHome) -> u64 {
    home.status()["seq"].as_u64().unwrap()
}

fn call(home: &TestHome, method: &str, params: &Value) -> Value {
    let run = home.rlxd(&["call", method, &params.to_string()]);
    assert_eq!(run.code(), Some(0), "{method}: {run:?}");
    run.data()
}

/// The error code `method` is refused with.
fn refused(home: &TestHome, method: &str, params: &Value) -> String {
    let run = home.rlxd(&["call", method, &params.to_string()]);
    assert_eq!(run.code(), Some(1), "{method}: {run:?}");
    run.error_code()
}

/// A run job `id` of fake-claude's prompt `prompt`, on `schedule`.
fn job(id: &str, schedule: Value, catch_up: &str) -> Value {
    json!({"job_id": id, "type": "run", "schedule": schedule,
        "run": {"kind": "task", "prompt": "hi", "mount": {"kind": "no_tools"},
                "space": "default"},
        "catch_up": catch_up, "enabled": true})
}

fn every_minute(id: &str, catch_up: &str) -> Value {
    job(id, json!({"every_minutes": 1}), catch_up)
}

fn put(home: &TestHome, job: &Value) {
    let put = call(home, "job.put", job);
    assert_eq!(put["job_id"], job["job_id"]);
}

/// Moves the test clock on `seconds`, and back with every decision made.
fn advance(home: &TestHome, seconds: u64) -> String {
    let now = call(home, "debug.clock_advance", &json!({"seconds": seconds}));
    now["now"].as_str().unwrap().to_owned()
}

/// The job and run events since `since`.
fn events(home: &TestHome, since: u64) -> Vec<Value> {
    let run = home.rlxd(&[
        "events",
        "--topics",
        "job,run",
        "--since",
        &since.to_string(),
    ]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    run.lines()
}

/// Events since `since` until `done` holds of them, for up to `seconds`.
fn wait_until(
    home: &TestHome,
    since: u64,
    seconds: u64,
    what: &str,
    done: impl Fn(&[Value]) -> bool,
) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        let seen = events(home, since);
        if done(&seen) {
            return seen;
        }
        assert!(Instant::now() < deadline, "never: {what}: {seen:#?}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `events` of `type`, for job `job` (or any job).
fn typed<'a>(events: &'a [Value], r#type: &str, job: Option<&str>) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|e| e["type"] == r#type)
        .filter(|e| job.is_none_or(|job| e["data"]["job_id"] == job))
        .collect()
}

/// The skips of job `job`, by reason.
fn skips(events: &[Value], job: &str) -> Vec<String> {
    typed(events, "job.skipped", Some(job))
        .iter()
        .map(|e| e["data"]["reason"].as_str().unwrap().to_owned())
        .collect()
}

fn listed(home: &TestHome, job: &str) -> Value {
    let list = call(home, "job.list", &json!({}));
    list["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["job_id"] == job)
        .cloned()
        .unwrap_or(Value::Null)
}

fn run_ended(events: &[Value], run_id: &Value) -> bool {
    events.iter().any(|e| {
        ["run.done", "run.failed", "run.needs_you"].contains(&e["type"].as_str().unwrap_or(""))
            && e["data"]["run_id"] == *run_id
    })
}

#[test]
fn a_job_fires_with_the_app_connected_and_its_run_ends() {
    let home = home_at("hello", T0);
    let since = seq(&home);
    let _app = app(&home);
    put(&home, &every_minute("t1", "skip"));
    assert_eq!(listed(&home, "t1")["next_fire_at"], "2027-01-01T00:01:00Z");
    advance(&home, 59);
    assert!(
        typed(&events(&home, since), "job.fired", None).is_empty(),
        "not yet"
    );
    advance(&home, 1);
    let seen = events(&home, since);
    let fired = typed(&seen, "job.fired", Some("t1"));
    assert_eq!(fired.len(), 1, "{seen:#?}");
    let data = &fired[0]["data"];
    assert_eq!(data["scheduled_for"], "2027-01-01T00:01:00Z");
    assert_eq!(data["cause"], "due");
    let run_id = data["run_id"].clone();
    let seen = wait_until(&home, since, 30, "the fired run ends", |e| {
        run_ended(e, &run_id)
    });
    let done = seen
        .iter()
        .find(|e| e["type"] == "run.done" && e["data"]["run_id"] == run_id)
        .unwrap_or_else(|| panic!("{seen:#?}"));
    assert_eq!(done["data"]["job_id"], "t1", "7.5's job_id on run events");
    for event in typed(&seen, "job.put", None)
        .into_iter()
        .chain(typed(&seen, "job.fired", None))
    {
        assert_valid("job-event.json", event);
    }
    let after = listed(&home, "t1");
    assert_eq!(after["last_run_id"], run_id);
    assert_eq!(after["last_fire_at"], "2027-01-01T00:01:00Z");
    assert_eq!(after["next_fire_at"], "2027-01-01T00:02:00Z");
    common::eventually("the run's end is the job's last state", || {
        listed(&home, "t1")["last_state"] == "done"
    });
}

#[test]
fn with_no_app_a_due_fire_is_skipped_app_absent_and_nothing_runs() {
    let home = home_at("hello", T0);
    let since = seq(&home);
    put(&home, &every_minute("t1", "once"));
    advance(&home, 60);
    let seen = events(&home, since);
    assert_eq!(skips(&seen, "t1"), ["app_absent"], "{seen:#?}");
    assert!(typed(&seen, "job.fired", None).is_empty());
    assert!(typed(&seen, "run.started", None).is_empty());
    assert_eq!(listed(&home, "t1")["skipped_count"], 1);
    for event in typed(&seen, "job.skipped", None) {
        assert_valid("job-event.json", event);
    }
}

#[test]
fn run_now_needs_the_app_and_then_fires_at_once() {
    let home = home_at("hello", T0);
    put(&home, &every_minute("t1", "skip"));
    assert_eq!(
        refused(&home, "job.run_now", &json!({"job_id": "t1"})),
        "app_absent"
    );
    let since = seq(&home);
    let _app = app(&home);
    let now = call(&home, "job.run_now", &json!({"job_id": "t1"}));
    assert!(now["run_id"].is_string(), "{now:#}");
    let seen = wait_until(&home, since, 30, "the run ends", |e| {
        run_ended(e, &now["run_id"])
    });
    let fired = typed(&seen, "job.fired", Some("t1"));
    assert_eq!(fired[0]["data"]["cause"], "run_now");
    assert_eq!(
        refused(&home, "job.run_now", &json!({"job_id": "nope"})),
        "unknown_job"
    );
}

#[test]
fn catch_up_once_fires_once_when_the_app_comes_back_and_skip_waits() {
    let home = home_at("hello", T0);
    let since = seq(&home);
    put(&home, &every_minute("once", "once"));
    put(&home, &every_minute("skip", "skip"));
    advance(&home, 60);
    advance(&home, 60);
    let seen = events(&home, since);
    assert_eq!(skips(&seen, "once"), ["app_absent", "app_absent"]);
    assert_eq!(skips(&seen, "skip"), ["app_absent", "app_absent"]);
    let back = seq(&home);
    let _app = app(&home);
    wait_until(&home, back, 10, "the owed fire", |e| {
        !typed(e, "job.fired", Some("once")).is_empty()
    });
    std::thread::sleep(Duration::from_millis(500));
    let seen = events(&home, back);
    let once = typed(&seen, "job.fired", Some("once"));
    assert_eq!(once.len(), 1, "once, for both skips: {seen:#?}");
    assert_eq!(once[0]["data"]["cause"], "catch_up");
    assert_eq!(once[0]["data"]["scheduled_for"], "2027-01-01T00:02:00Z");
    assert!(
        typed(&seen, "job.fired", Some("skip")).is_empty(),
        "skip waits for its next time"
    );
    // Which comes at 00:03.
    advance(&home, 60);
    assert_eq!(
        typed(&events(&home, back), "job.fired", Some("skip")).len(),
        1
    );
}

#[test]
fn a_job_survives_a_restart() {
    let home = home_at("hello", T0);
    put(&home, &every_minute("t1", "skip"));
    let before = listed(&home, "t1");
    home.stop();
    start(&home);
    let after = listed(&home, "t1");
    assert_eq!(after, before);
}

#[test]
fn a_second_put_replaces_the_job_and_starts_its_schedule_again() {
    let home = home_at("hello", T0);
    put(&home, &every_minute("t1", "skip"));
    advance(&home, 30);
    put(&home, &job("t1", json!({"every_minutes": 5}), "skip"));
    let list = call(&home, "job.list", &json!({}));
    assert_eq!(list["jobs"].as_array().unwrap().len(), 1);
    assert_eq!(listed(&home, "t1")["next_fire_at"], "2027-01-01T00:05:30Z");
    assert_eq!(listed(&home, "t1")["schedule"], json!({"every_minutes": 5}));
}

#[test]
fn a_paused_job_skips_paused_and_fires_again_once_resumed() {
    let home = home_at("hello", T0);
    let since = seq(&home);
    let _app = app(&home);
    put(&home, &every_minute("t1", "skip"));
    call(&home, "job.pause", &json!({"job_id": "t1"}));
    assert_eq!(listed(&home, "t1")["paused"], true);
    advance(&home, 60);
    assert_eq!(skips(&events(&home, since), "t1"), ["paused"]);
    call(&home, "job.resume", &json!({"job_id": "t1"}));
    advance(&home, 60);
    let seen = events(&home, since);
    assert_eq!(typed(&seen, "job.fired", Some("t1")).len(), 1, "{seen:#?}");
    for r#type in ["job.paused", "job.resumed"] {
        assert_eq!(typed(&seen, r#type, Some("t1")).len(), 1, "{type}");
    }
}

#[test]
fn a_deleted_job_is_gone_and_never_fires() {
    let home = home_at("hello", T0);
    let since = seq(&home);
    let _app = app(&home);
    put(&home, &every_minute("t1", "skip"));
    call(&home, "job.delete", &json!({"job_id": "t1"}));
    assert_eq!(listed(&home, "t1"), Value::Null);
    advance(&home, 120);
    let seen = events(&home, since);
    assert!(typed(&seen, "job.fired", None).is_empty(), "{seen:#?}");
    assert_eq!(typed(&seen, "job.deleted", Some("t1")).len(), 1);
    assert_eq!(
        refused(&home, "job.delete", &json!({"job_id": "t1"})),
        "unknown_job"
    );
}

#[test]
fn a_job_never_overlaps_itself() {
    let home = home_at("slow", T0);
    let since = seq(&home);
    let _app = app(&home);
    let mut slow = every_minute("t1", "skip");
    slow["run"]["prompt"] = json!("take your time");
    put(&home, &slow);
    advance(&home, 60);
    let first = typed(&events(&home, since), "job.fired", Some("t1"))[0]["data"]["run_id"].clone();
    wait_until(&home, since, 20, "the run starts", |e| {
        e.iter()
            .any(|e| e["type"] == "run.started" && e["data"]["run_id"] == first)
    });
    advance(&home, 60);
    assert_eq!(skips(&events(&home, since), "t1"), ["overlap"]);
    assert_eq!(
        refused(&home, "job.run_now", &json!({"job_id": "t1"})),
        "job_running"
    );
    call(&home, "run.cancel", &json!({"run_id": first}));
}

#[test]
fn a_rate_limited_run_holds_its_jobs_fires_until_the_reset() {
    // The fixture's reset is five minutes from the real time: the test clock starts there.
    let real_now = jiff_now();
    let home = home_at("rate-limit", &real_now);
    let since = seq(&home);
    let _app = app(&home);
    put(&home, &every_minute("t1", "skip"));
    advance(&home, 60);
    let run_id = typed(&events(&home, since), "job.fired", Some("t1"))[0]["data"]["run_id"].clone();
    wait_until(&home, since, 30, "the run ends", |e| run_ended(e, &run_id));
    // The scheduler has heard of the limit once the run's end is in.
    std::thread::sleep(Duration::from_millis(300));
    advance(&home, 60);
    assert_eq!(skips(&events(&home, since), "t1"), ["rate_limited"]);
    let run = home.rlxd(&["call", "job.run_now", r#"{"job_id":"t1"}"#]);
    assert_eq!(run.error_code(), "rate_limited", "{run:?}");
    assert!(run.json()["error"]["detail"]["resets_at"].is_string());
    // Minute by minute, it is held until the reset, five minutes on, then fires again.
    let mut minutes = 0;
    while typed(&events(&home, since), "job.fired", Some("t1")).len() < 2 {
        minutes += 1;
        assert!(minutes <= 8, "{:#?}", events(&home, since));
        advance(&home, 60);
    }
    assert!(
        skips(&events(&home, since), "t1")
            .iter()
            .all(|r| r == "rate_limited")
    );
}

/// Now, as RFC 3339 in UTC, to the second.
fn jiff_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let days = secs / 86_400;
    let (year, month, day) = civil(days);
    let of_day = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        of_day / 3600,
        of_day / 60 % 60,
        of_day % 60
    )
}

/// Days since 1970-01-01 as a civil date (Howard Hinnant's algorithm).
fn civil(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// A job due while rlxd was down: rlxd restarts with its test clock past the fire.
fn missed(catch_up: &str) -> (TestHome, u64) {
    let mut home = home_at("hello", T0);
    put(&home, &every_minute("t1", catch_up));
    home.stop();
    home.set_env("RLXD_TEST_CLOCK", "2027-01-01T00:05:30Z");
    // Not start(): its pairing call is an app connection, which would decide the fire.
    home.start_dev();
    // The restarted rlxd's buffer holds only its own events: all of them.
    (home, 0)
}

#[test]
fn a_fire_missed_while_rlxd_was_down_follows_catch_up_once_an_app_connects() {
    for (catch_up, fired, reason) in [("skip", 0, Some("missed")), ("once", 1, None)] {
        let (home, since) = missed(catch_up);
        let _app = app(&home);
        wait_until(&home, since, 15, "the missed fire is decided", |e| {
            !typed(e, "job.fired", Some("t1")).is_empty()
                || !typed(e, "job.skipped", Some("t1")).is_empty()
        });
        std::thread::sleep(Duration::from_millis(300));
        let seen = events(&home, since);
        assert_eq!(
            typed(&seen, "job.fired", Some("t1")).len(),
            fired,
            "{catch_up}: {seen:#?}"
        );
        let skipped = skips(&seen, "t1");
        assert_eq!(skipped.first().map(String::as_str), reason, "{catch_up}");
        // And the next is on the grid after now.
        assert_eq!(listed(&home, "t1")["next_fire_at"], "2027-01-01T00:06:00Z");
    }
}

#[test]
fn a_fire_missed_while_rlxd_was_down_with_no_app_is_app_absent() {
    let (home, since) = missed("once");
    // The missed fire waits ten seconds for an app; none comes.
    advance(&home, 9);
    assert!(skips(&events(&home, since), "t1").is_empty());
    advance(&home, 2);
    assert_eq!(skips(&events(&home, since), "t1"), ["app_absent"]);
}

#[test]
fn a_clock_jump_is_one_decision_for_every_fire_it_passed() {
    let home = home_at("hello", T0);
    let since = seq(&home);
    let _app = app(&home);
    put(&home, &every_minute("once", "once"));
    put(&home, &every_minute("skip", "skip"));
    advance(&home, 3 * 3600 + 30);
    let seen = events(&home, since);
    let fired = typed(&seen, "job.fired", Some("once"));
    assert_eq!(fired.len(), 1, "{seen:#?}");
    assert_eq!(fired[0]["data"]["cause"], "catch_up");
    assert_eq!(skips(&seen, "skip"), ["missed"]);
    for id in ["once", "skip"] {
        assert_eq!(
            listed(&home, id)["next_fire_at"],
            "2027-01-01T03:01:00Z",
            "{id}"
        );
    }
}

#[test]
fn every_minutes_keeps_its_grid_when_a_fire_is_late() {
    let home = home_at("hello", T0);
    let since = seq(&home);
    let _app = app(&home);
    put(&home, &every_minute("t1", "skip"));
    // Due at 00:01, decided at 00:01:30: on time still, and the next stays at 00:02.
    advance(&home, 90);
    let fired = typed(&events(&home, since), "job.fired", Some("t1"))
        .iter()
        .map(|e| e["data"]["scheduled_for"].clone())
        .collect::<Vec<_>>();
    assert_eq!(fired, [json!("2027-01-01T00:01:00Z")]);
    assert_eq!(listed(&home, "t1")["next_fire_at"], "2027-01-01T00:02:00Z");
}

#[test]
fn cron_fires_at_its_matches_and_a_dst_gap_once_at_its_end() {
    let home = home_at("hello", "2027-03-28T00:40:00Z");
    let since = seq(&home);
    let _app = app(&home);
    put(
        &home,
        &job("c", json!({"cron": "* * * * *", "tz": "UTC"}), "skip"),
    );
    assert_eq!(listed(&home, "c")["next_fire_at"], "2027-03-28T00:41:00Z");
    // London's 01:30 doesn't happen on 2027-03-28: it fires once, at 02:00 BST.
    put(
        &home,
        &job(
            "london",
            json!({"cron": "30 1 * * *", "tz": "Europe/London"}),
            "skip",
        ),
    );
    assert_eq!(
        listed(&home, "london")["next_fire_at"],
        "2027-03-28T01:00:00Z"
    );
    call(&home, "job.delete", &json!({"job_id": "c"}));
    call(
        &home,
        "debug.clock_set",
        &json!({"at": "2027-03-28T01:00:00Z"}),
    );
    let seen = events(&home, since);
    assert_eq!(
        typed(&seen, "job.fired", Some("london")).len(),
        1,
        "{seen:#?}"
    );
    assert_eq!(
        listed(&home, "london")["next_fire_at"],
        "2027-03-29T00:30:00Z"
    );
}

#[test]
fn loud_is_echoed_on_job_fired_and_on_the_runs_end() {
    let home = home_at("hello", T0);
    let since = seq(&home);
    let _app = app(&home);
    let mut loud = every_minute("t1", "skip");
    loud["loud"] = json!(true);
    put(&home, &loud);
    assert_eq!(listed(&home, "t1")["loud"], true);
    advance(&home, 60);
    let fired = typed(&events(&home, since), "job.fired", Some("t1"))[0].clone();
    assert_eq!(fired["data"]["loud"], true);
    let run_id = fired["data"]["run_id"].clone();
    let seen = wait_until(&home, since, 30, "the run ends", |e| run_ended(e, &run_id));
    let done = typed(&seen, "run.done", None)
        .into_iter()
        .find(|e| e["data"]["run_id"] == run_id)
        .unwrap();
    assert_eq!(done["data"]["loud"], true, "{done:#}");
}

#[test]
fn loud_is_echoed_on_the_runs_needs_you() {
    let home = home_at("asks", T0);
    let since = seq(&home);
    let _app = app(&home);
    let mut asks = every_minute("t1", "skip");
    asks["loud"] = json!(true);
    asks["run"]["prompt"] = json!("post the standup");
    asks["run"]["needs_you_when"] =
        json!({"pointer": "/status", "equals": "needs_you", "question_pointer": "/question"});
    put(&home, &asks);
    advance(&home, 60);
    let run_id = typed(&events(&home, since), "job.fired", Some("t1"))[0]["data"]["run_id"].clone();
    let seen = wait_until(&home, since, 30, "the run needs you", |e| {
        run_ended(e, &run_id)
    });
    let needs = typed(&seen, "run.needs_you", None)
        .into_iter()
        .find(|e| e["data"]["run_id"] == run_id)
        .unwrap_or_else(|| panic!("{seen:#?}"));
    assert_eq!(needs["data"]["loud"], true, "{needs:#}");
}

#[test]
fn a_template_run_start_would_refuse_is_refused_at_put() {
    let home = home_at("hello", T0);
    let with = |field: &str, value: Value| {
        let mut bad = every_minute("bad", "skip");
        bad["run"][field] = value;
        bad
    };
    for (bad, code) in [
        (
            with(
                "mount",
                json!({"kind": "connectors", "connectors": ["claude.ai Slack"]}),
            )
            .pipe(|mut b| {
                b["run"]["kind"] = json!("check");
                b
            }),
            "kind_mount_mismatch",
        ),
        (with("kind", json!("send_executor")), "pinned_call_required"),
        (with("space", json!("../x")), "invalid_params"),
        (
            with("run_id", json!("0f8e2d6c-3b1a-4c5d-9e7f-1a2b3c4d5e6f")),
            "invalid_params",
        ),
        (
            job("bad", json!({"cron": "61 * * * *", "tz": "UTC"}), "skip"),
            "invalid_params",
        ),
        (
            job(
                "bad",
                json!({"cron": "* * * * *", "tz": "Mars/Olympus"}),
                "skip",
            ),
            "invalid_params",
        ),
        (
            every_minute("bad", "skip").pipe(|mut b| {
                b["run_when"] = json!("always");
                b
            }),
            "invalid_params",
        ),
        (
            json!({"job_id": "bad", "type": "watch", "schedule": {"every_minutes": 5},
                "connector": "claude.ai GitHub", "tool": "x", "input": {},
                "catch_up": "skip"}),
            "not_implemented",
        ),
    ] {
        assert_eq!(refused(&home, "job.put", &bad), code, "{bad:#}");
    }
    assert_eq!(listed(&home, "bad"), Value::Null, "nothing was stored");
    // run.start serves needs_connectors (5.5), so a template naming connectors is put.
    let mut gated = every_minute("gated", "skip");
    gated["run"]["mount"] = json!({"kind": "connectors", "connectors": ["claude.ai Slack"]});
    gated["run"]["needs_connectors"] = json!(["claude.ai Slack"]);
    put(&home, &gated);
    assert_eq!(listed(&home, "gated")["job_id"], "gated");
}

trait Pipe: Sized {
    fn pipe(self, f: impl FnOnce(Self) -> Self) -> Self {
        f(self)
    }
}
impl Pipe for Value {}

#[test]
fn a_corrupt_database_is_set_aside_reported_and_replaced() {
    let home = unstarted("hello", T0);
    let dir = home.root().join("rlxd");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("rlxd.db"), "not a database").unwrap();
    start(&home);
    let jobs = home.status()["jobs"].clone();
    assert_eq!(jobs["db"], "set_aside", "{jobs:#}");
    assert!(
        jobs["set_aside"]
            .as_str()
            .unwrap()
            .contains("rlxd.db.unreadable-"),
        "{jobs:#}"
    );
    put(&home, &every_minute("t1", "skip"));
    assert_eq!(home.status()["jobs"]["count"], 1);
}

#[cfg(unix)]
#[test]
fn the_database_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let home = home_at("hello", T0);
    put(&home, &every_minute("t1", "skip"));
    let dir = home.root().join("rlxd");
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir), 0o700);
    for name in ["rlxd.db", "rlxd.db-wal", "rlxd.db-shm"] {
        assert_eq!(mode(&dir.join(name)), 0o600, "{name}");
    }
}

// --- job.put is idempotent (review #2687) -------------------------------------------------

#[test]
fn the_same_put_again_changes_nothing_and_an_every_minutes_grid_stays() {
    let home = home_at("hello", T0);
    let five = job("t1", json!({"every_minutes": 5}), "skip");
    put(&home, &five);
    for _ in 0..4 {
        advance(&home, 60);
        // The app pushes its jobs again on each connect.
        put(&home, &five);
    }
    assert_eq!(listed(&home, "t1")["next_fire_at"], "2027-01-01T00:05:00Z");
    // A field other than the schedule keeps it too.
    let mut loud = five.clone();
    loud["loud"] = json!(true);
    put(&home, &loud);
    assert_eq!(listed(&home, "t1")["next_fire_at"], "2027-01-01T00:05:00Z");
    // Disabled, it has none; enabled again, its schedule starts from now.
    loud["enabled"] = json!(false);
    put(&home, &loud);
    assert_eq!(listed(&home, "t1")["next_fire_at"], Value::Null);
    loud["enabled"] = json!(true);
    put(&home, &loud);
    assert_eq!(listed(&home, "t1")["next_fire_at"], "2027-01-01T00:09:00Z");
}

#[test]
fn an_owed_fire_survives_the_same_put_before_the_app_connects() {
    let home = home_at("hello", T0);
    let once = every_minute("t1", "once");
    put(&home, &once);
    advance(&home, 60);
    // The app's reconnect: it pushes its job again, then connects.
    put(&home, &once);
    let back = seq(&home);
    let _app = app(&home);
    let seen = wait_until(&home, back, 10, "the owed fire", |e| {
        !typed(e, "job.fired", Some("t1")).is_empty()
    });
    let fired = typed(&seen, "job.fired", Some("t1"));
    assert_eq!(fired[0]["data"]["cause"], "catch_up");
    assert_eq!(fired[0]["data"]["scheduled_for"], "2027-01-01T00:01:00Z");
}

#[test]
fn a_fire_missed_while_down_survives_the_same_put_inside_its_wait() {
    let (home, since) = missed("once");
    // The same put, inside the 10 s wait: the missed fire is still decided.
    put(&home, &every_minute("t1", "once"));
    advance(&home, 11);
    assert_eq!(skips(&events(&home, since), "t1"), ["app_absent"]);
}

// --- a fire's run, started once (review #2687) --------------------------------------------

#[test]
fn a_crash_while_a_fire_starts_never_fires_that_time_again() {
    let mut home = unstarted_with("hello", T0, &[("RLXD_TEST_CRASH_AFTER_FIRE", "1")]);
    start(&home);
    put(&home, &every_minute("t1", "once"));
    // An app, as a bare process: rlxd is gone by the time it ends.
    let since = seq(&home).to_string();
    let mut bare = home.spawn(&[
        "events", "--as-app", "--topics", "daemon", "--since", &since, "--follow",
    ]);
    common::eventually("the app is connected", || home.app_clients() == 1);
    // rlxd exits once the 00:01 fire's run has started, before it is noted.
    let crashed = home.rlxd(&["call", "debug.clock_advance", r#"{"seconds":60}"#]);
    assert_ne!(crashed.code(), Some(0), "{crashed:?}");
    common::eventually("rlxd is gone", || {
        home.rlxd(&["call", "status"]).code() == Some(3)
    });
    let _ = bare.kill();
    let _ = bare.wait();
    home.set_env("RLXD_TEST_CRASH_AFTER_FIRE", "");
    home.set_env("RLXD_TEST_CLOCK", "2027-01-01T00:01:30Z");
    start(&home);
    let since = 0;
    let _app = app(&home);
    advance(&home, 30);
    let seen = events(&home, since);
    let fired: Vec<String> = typed(&seen, "job.fired", Some("t1"))
        .iter()
        .map(|e| e["data"]["scheduled_for"].as_str().unwrap().to_owned())
        .collect();
    // The 00:01 fire is the run that started before the crash, settled, not fired again.
    assert_eq!(
        fired,
        ["2027-01-01T00:01:00Z", "2027-01-01T00:02:00Z"],
        "{seen:#?}"
    );
    let runs = call(&home, "run.list", &json!({"job_id": "t1"}));
    assert_eq!(runs["runs"].as_array().unwrap().len(), 2, "{runs:#}");
}

#[test]
fn a_fire_whose_run_cannot_start_is_skipped_start_failed_with_its_code() {
    let home = home_at_with("hello", T0, &[("RLXD_TEST_REFUSE_FIRES", "hook_not_found")]);
    let since = seq(&home);
    let _app = app(&home);
    put(&home, &every_minute("t1", "skip"));
    advance(&home, 60);
    let seen = events(&home, since);
    let skipped = typed(&seen, "job.skipped", Some("t1"));
    assert_eq!(skipped.len(), 1, "{seen:#?}");
    assert_eq!(skipped[0]["data"]["reason"], "start_failed");
    assert_eq!(skipped[0]["data"]["code"], "hook_not_found");
    assert_valid("job-event.json", skipped[0]);
    assert!(typed(&seen, "job.fired", None).is_empty());
    assert_eq!(
        refused(&home, "job.run_now", &json!({"job_id": "t1"})),
        "hook_not_found"
    );
}

fn home_at_with(scenario: &str, clock: &str, envs: &[(&str, &str)]) -> TestHome {
    let home = unstarted_with(scenario, clock, envs);
    start(&home);
    home
}

#[test]
fn a_rate_limit_warning_holds_no_fires() {
    let real_now = jiff_now();
    let home = home_at_with(
        "rate-limit",
        &real_now,
        &[("FAKE_CLAUDE_RATE_STATUS", "allowed_warning")],
    );
    let since = seq(&home);
    let _app = app(&home);
    put(&home, &every_minute("t1", "skip"));
    advance(&home, 60);
    let run_id = typed(&events(&home, since), "job.fired", Some("t1"))[0]["data"]["run_id"].clone();
    let seen = wait_until(&home, since, 30, "the run ends", |e| run_ended(e, &run_id));
    let limited = seen
        .iter()
        .find(|e| e["type"] == "run.rate_limited" && e["data"]["run_id"] == run_id)
        .unwrap_or_else(|| panic!("{seen:#?}"));
    assert_eq!(limited["data"]["status"], "allowed_warning");
    std::thread::sleep(Duration::from_millis(300));
    advance(&home, 60);
    let seen = events(&home, since);
    assert!(skips(&seen, "t1").is_empty(), "{seen:#?}");
    assert_eq!(typed(&seen, "job.fired", Some("t1")).len(), 2);
}

#[test]
fn a_jobs_last_state_is_its_runs_running_and_then_after_a_restart_failed() {
    let home = home_at("slow", T0);
    let since = seq(&home);
    let _app = app(&home);
    let mut slow = every_minute("t1", "skip");
    slow["run"]["prompt"] = json!("take your time");
    put(&home, &slow);
    advance(&home, 60);
    let run_id = typed(&events(&home, since), "job.fired", Some("t1"))[0]["data"]["run_id"].clone();
    wait_until(&home, since, 20, "the run starts", |e| {
        e.iter()
            .any(|e| e["type"] == "run.started" && e["data"]["run_id"] == run_id)
    });
    assert_eq!(listed(&home, "t1")["last_state"], "running");
    drop(_app);
    home.stop();
    start(&home);
    let after = listed(&home, "t1");
    assert_eq!(after["last_run_id"], run_id);
    assert_eq!(
        after["last_state"], "failed",
        "ended engine_restarted: {after:#}"
    );
}

#[test]
fn an_hourly_job_put_again_every_fifty_minutes_still_fires_every_hour() {
    // The tester's repro (#2689): it fired 0 times in 10 hours when each put moved it.
    let home = home_at("hello", T0);
    let since = seq(&home);
    let _app = app(&home);
    let hourly = job("t1", json!({"every_minutes": 60}), "skip");
    put(&home, &hourly);
    // Ten minutes at a time, so each fire is decided on time; put again every fifty.
    for step in 1..=60 {
        advance(&home, 10 * 60);
        if step % 5 == 0 {
            put(&home, &hourly);
        }
    }
    // 600 minutes: one decision at each of hours 1 to 10, the grid never moved by a put.
    // (A fire's run may still be going an hour later on this clock: then that hour is
    // skipped overlap, which is a decision on the grid too.)
    let seen = events(&home, since);
    let decided: Vec<String> = seen
        .iter()
        .filter(|e| {
            (e["type"] == "job.fired" || e["type"] == "job.skipped") && e["data"]["job_id"] == "t1"
        })
        .map(|e| e["data"]["scheduled_for"].as_str().unwrap().to_owned())
        .collect();
    let hours: Vec<String> = (1..=10)
        .map(|h| format!("2027-01-01T{h:02}:00:00Z"))
        .collect();
    assert_eq!(decided, hours, "{seen:#?}");
    assert!(
        skips(&seen, "t1").iter().all(|reason| reason == "overlap"),
        "{seen:#?}"
    );
    assert!(!typed(&seen, "job.fired", Some("t1")).is_empty());
}

// --- round 2 (review #2704) ---------------------------------------------------------------

/// The review's repro: an app_absent skip owed with catch_up once, then the job disabled.
fn owed_then_disabled() -> (TestHome, Value) {
    let home = home_at("hello", T0);
    let once = every_minute("t1", "once");
    put(&home, &once);
    advance(&home, 60);
    let mut off = once.clone();
    off["enabled"] = json!(false);
    put(&home, &off);
    assert_eq!(listed(&home, "t1")["next_fire_at"], Value::Null);
    (home, off)
}

#[test]
fn a_disabled_job_never_fires_the_catch_up_it_owed() {
    let (home, _) = owed_then_disabled();
    let back = seq(&home);
    let _app = app(&home);
    std::thread::sleep(Duration::from_millis(500));
    advance(&home, 120);
    let seen = events(&home, back);
    assert!(typed(&seen, "job.fired", None).is_empty(), "{seen:#?}");
    assert!(typed(&seen, "job.skipped", None).is_empty(), "{seen:#?}");
    assert_eq!(
        refused(&home, "job.run_now", &json!({"job_id": "t1"})),
        "job_disabled"
    );
}

#[test]
fn a_disabled_job_never_fires_the_catch_up_it_owed_across_a_restart() {
    let (home, _) = owed_then_disabled();
    home.stop();
    // Not start(): its pairing call is an app connection.
    home.start_dev();
    let _app = app(&home);
    std::thread::sleep(Duration::from_millis(500));
    advance(&home, 120);
    let seen = events(&home, 0);
    assert!(typed(&seen, "job.fired", None).is_empty(), "{seen:#?}");
}

#[test]
fn a_schedule_respelled_in_an_alias_zone_keeps_an_owed_fire() {
    let home = home_at("hello", T0);
    put(
        &home,
        &job("c", json!({"cron": "* * * * *", "tz": "UTC"}), "once"),
    );
    advance(&home, 60);
    // The same schedule, written another way.
    put(
        &home,
        &job("c", json!({"cron": "*/1 * * * *", "tz": "Etc/UTC"}), "once"),
    );
    let back = seq(&home);
    let _app = app(&home);
    let seen = wait_until(&home, back, 10, "the owed fire", |e| {
        !typed(e, "job.fired", Some("c")).is_empty()
    });
    let fired = typed(&seen, "job.fired", Some("c"));
    assert_eq!(fired[0]["data"]["cause"], "catch_up");
    assert_eq!(fired[0]["data"]["scheduled_for"], "2027-01-01T00:01:00Z");
}

#[cfg(unix)]
#[test]
#[allow(unsafe_code)]
fn a_database_it_cannot_open_leaves_the_scheduler_unavailable_with_why() {
    use std::os::unix::fs::PermissionsExt;
    let home = home_at("hello", T0);
    put(&home, &every_minute("t1", "skip"));
    home.stop();
    let db = home.root().join("rlxd").join("rlxd.db");
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o000)).unwrap();
    home.start_dev();
    let jobs = home.status()["jobs"].clone();
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o600)).unwrap();
    if jobs["db"] == Value::Null {
        return; // root opens anything
    }
    assert_eq!(jobs["db"], "unavailable", "{jobs:#}");
    assert!(
        jobs["reason"].as_str().unwrap().contains("left as it is"),
        "{jobs:#}"
    );
    assert!(jobs.get("set_aside").is_none(), "{jobs:#}");
    assert!(db.exists(), "the file stays where it is");
}

#[test]
fn a_real_schedule_change_lets_an_owed_fire_go() {
    // Tester r2's surviving mutant R4: a changed schedule starts again from now, and
    // what the old one owed goes with it.
    let home = home_at("hello", T0);
    put(&home, &every_minute("t1", "once"));
    advance(&home, 60);
    put(&home, &job("t1", json!({"every_minutes": 5}), "once"));
    let back = seq(&home);
    let _app = app(&home);
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        typed(&events(&home, back), "job.fired", None).is_empty(),
        "{:#?}",
        events(&home, back)
    );
    assert_eq!(listed(&home, "t1")["next_fire_at"], "2027-01-01T00:06:00Z");
}

#[test]
fn a_clock_set_back_counts_the_next_fire_from_now() {
    // Tester r2's surviving mutant R12: rlxd restarted with its clock earlier than the
    // job's next fire (NTP, or set by hand) counts it again from now, on the job's grid,
    // whether the job has fired or not, before its anchor too.
    let mut home = home_at("hello", T0);
    put(&home, &job("fired", json!({"every_minutes": 5}), "skip"));
    put(&home, &job("never", json!({"every_minutes": 5}), "skip"));
    call(&home, "job.pause", &json!({"job_id": "never"}));
    advance(&home, 600);
    assert_eq!(
        listed(&home, "fired")["next_fire_at"],
        "2027-01-01T00:15:00Z"
    );
    home.stop();
    home.set_env("RLXD_TEST_CLOCK", "2027-01-01T00:02:00Z");
    home.start_dev();
    common::eventually("the next fire counted from now", || {
        listed(&home, "fired")["next_fire_at"] == "2027-01-01T00:05:00Z"
    });
    home.stop();
    home.set_env("RLXD_TEST_CLOCK", "2026-12-31T23:31:00Z");
    home.start_dev();
    common::eventually("before the anchor, on its grid", || {
        listed(&home, "fired")["next_fire_at"] == "2026-12-31T23:35:00Z"
    });
}
