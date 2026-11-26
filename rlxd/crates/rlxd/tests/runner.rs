//! The runner's Accept checks (handoff 0.10.3, 2.1, 2.5 and 7.5), through the real socket,
//! with fake-claude standing in for Claude Code on every OS ("How to read the checks").
//!
//! Each test gets a fresh home, a `CLAUDE_CONFIG_DIR` inside it, and fake-claude linked
//! into it as `<home>/bin/fake-claude/<scenario>`, which `RLXD_CLAUDE_BIN` names: the
//! process's command line names its scenario, as `pgrep -f fake-claude/slow` expects. No
//! check needs tmux.
//!
//! `gated_start` is the handoff's helper: an app-role capture of the `run` and `needs_you`
//! topics from the current `seq`, started before `run.start`. rlxd runs in gate dev mode
//! so the test client can hold the app role (the first `rlxd call --as-app` pairs).

mod common;

use std::path::{Path, PathBuf};
use std::process::Child;
use std::thread;
use std::time::{Duration, Instant};

use common::{TestHome, assert_valid, finish};
use serde_json::{Value, json};

/// fake-claude, linked (or copied) to `<home>/bin/fake-claude/<scenario>`, `.exe` on
/// Windows.
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

/// A home whose rlxd runs fake-claude `scenario`, with `envs` set too, in gate dev mode,
/// with the test client paired as the app.
fn home_with(scenario: &str, envs: &[(&str, &str)]) -> TestHome {
    let home = TestHome::new();
    let claude = link(home.root(), scenario);
    let config = home.root().join("claude-config");
    std::fs::create_dir_all(&config).unwrap();
    let mut home = home
        .with_claude(claude)
        .with_env("CLAUDE_CONFIG_DIR", config.to_str().unwrap());
    for (name, value) in envs {
        home = home.with_env(name, value);
    }
    home.start_dev();
    let paired = home.rlxd(&["call", "--as-app", "status"]);
    assert_eq!(paired.code(), Some(0), "{paired:?}");
    home
}

fn home(scenario: &str) -> TestHome {
    home_with(scenario, &[])
}

/// A new run id, as `uuidgen | tr A-Z a-z` makes one.
fn run_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let mixed = nanos ^ (u128::from(std::process::id()) << 80) ^ (u128::from(n) << 64);
    let hex = format!("{mixed:032x}");
    format!(
        "{}-{}-4{}-8{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    )
}

/// `status`'s seq: where a capture starts.
fn seq(home: &TestHome) -> u64 {
    home.status()["seq"].as_u64().unwrap()
}

/// `params` with `run_id`.
fn with_id(params: &Value, id: &str) -> String {
    let mut params = params.clone();
    params["run_id"] = Value::String(id.to_owned());
    params.to_string()
}

/// `rlxd call run.start params`, which must succeed.
fn start(home: &TestHome, params: &Value, id: &str) {
    let run = home.rlxd(&["call", "run.start", &with_id(params, id)]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_eq!(run.data()["run_id"], id, "{run:?}");
}

/// The handoff's `gated_start`: an app-role capture of the run and needs_you topics from
/// the current seq, until the first run.done, run.failed or run.needs_you, then the run.
struct Gated {
    id: String,
    since: u64,
    capture: Child,
}

fn gated_start(home: &TestHome, params: &Value) -> Gated {
    let since = seq(home);
    let id = run_id();
    let capture = home.spawn(&[
        "events",
        "--as-app",
        "--topics",
        "run,needs_you",
        "--since",
        &since.to_string(),
        "--follow",
        "--until",
        "run.done,run.failed,run.needs_you",
        "--timeout",
        "60",
    ]);
    common::eventually("the capture holds the app role", || home.app_clients() >= 1);
    start(home, params, &id);
    Gated { id, since, capture }
}

impl Gated {
    /// `wait "$cap"`: the capture's events, `ev.jsonl`.
    fn wait(self) -> (String, u64, Vec<Value>) {
        let run = finish(self.capture);
        assert_eq!(run.code(), Some(0), "the capture: {run:?}");
        (self.id, self.since, run.lines())
    }
}

/// The run and needs_you events rlxd holds after `since`, replayed without following.
fn replay(home: &TestHome, since: u64) -> Vec<Value> {
    let run = home.rlxd(&[
        "events",
        "--topics",
        "run,needs_you",
        "--since",
        &since.to_string(),
    ]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    run.lines()
}

/// Waits up to `seconds` until the replay after `since` satisfies `done`, and returns it.
fn wait_until(
    home: &TestHome,
    since: u64,
    seconds: u64,
    what: &str,
    done: impl Fn(&[Value]) -> bool,
) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        let events = replay(home, since);
        if done(&events) {
            return events;
        }
        assert!(Instant::now() < deadline, "never: {what}: {events:#?}");
        thread::sleep(Duration::from_millis(100));
    }
}

/// `id`'s events, of `type` when given.
fn of<'a>(events: &'a [Value], id: &str, r#type: Option<&str>) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|e| e["data"]["run_id"] == id)
        .filter(|e| r#type.is_none_or(|t| e["type"] == t))
        .collect()
}

/// Whether run `id` has ended in `events`.
fn ended(events: &[Value], id: &str) -> bool {
    of(events, id, None).iter().any(|e| {
        ["run.done", "run.failed", "run.needs_you"].contains(&e["type"].as_str().unwrap_or(""))
    })
}

/// The position in `events` of `id`'s first event of `type`.
fn at(events: &[Value], id: &str, r#type: &str) -> usize {
    events
        .iter()
        .position(|e| e["data"]["run_id"] == id && e["type"] == r#type)
        .unwrap_or_else(|| panic!("no {type} for {id}: {events:#?}"))
}

/// `$AISQUARE_HOME/fake-claude.log.json`.
fn fixture_log(home: &TestHome) -> Value {
    let path = home.root().join("fake-claude.log.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap()
}

/// The argv of the fixture log's start whose `--session-id` or `--resume` is `id`.
fn argv_of(home: &TestHome, id: &str) -> Vec<String> {
    let log = fixture_log(home);
    let start = log["starts"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|s| s["argv"].as_array().unwrap().iter().any(|a| a == id))
        .unwrap_or_else(|| panic!("no start of {id}: {log:#}"));
    start["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap().to_owned())
        .collect()
}

fn after<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
    let at = argv.iter().position(|a| a == flag)?;
    argv.get(at + 1).map(String::as_str)
}

fn has(argv: &[String], flag: &str) -> bool {
    argv.iter().any(|a| a == flag)
}

/// How many processes' command lines hold `pattern`: `pgrep -f`, or its Windows form.
fn processes_matching(pattern: &str) -> usize {
    let output = if cfg!(windows) {
        let script = format!(
            "@(Get-CimInstance Win32_Process | Where-Object {{ $_.CommandLine -like '*{}*' \
             -and $_.Name -ne 'powershell.exe' }}).Count",
            pattern.replace('/', "\\")
        );
        std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .output()
            .unwrap()
    } else {
        std::process::Command::new("pgrep")
            .args(["-f", pattern])
            .output()
            .unwrap()
    };
    let text = String::from_utf8_lossy(&output.stdout);
    if cfg!(windows) {
        text.trim().parse().unwrap_or(0)
    } else {
        text.lines().filter(|l| !l.trim().is_empty()).count()
    }
}

/// The pattern that finds this home's `scenario` processes.
fn pattern(home: &TestHome, scenario: &str) -> String {
    format!(
        "{}/bin/fake-claude/{scenario}",
        home.root().display().to_string().replace('\\', "/")
    )
}

fn task(scenario_prompt: &str) -> Value {
    json!({"kind": "task", "prompt": scenario_prompt, "mount": {"kind": "no_tools"},
        "space": "default"})
}

// --- 2.1, 2.5 and 7.5: start and stream ---------------------------------------------------

#[test]
fn start_and_stream_the_exact_argv_in_the_space_and_the_schema() {
    let home = home("hello");
    let ensured = home.rlxd(&["call", "space.ensure", r#"{"name":"default"}"#]);
    assert_eq!(ensured.code(), Some(0), "{ensured:?}");
    let p = ensured.data()["path"].as_str().unwrap().to_owned();
    assert!(
        Path::new(&p).is_dir() && !Path::new(&p).join(".git").exists(),
        "{p}"
    );
    assert!(Path::new(&p).is_absolute());

    let params = json!({"kind": "task", "prompt": "hi", "mount": {"kind": "no_tools"},
        "persist_session": true, "space": "default"});
    let (r, _, events) = gated_start(&home, &params).wait();

    // 2.1's first check: run.started, then run.text only, then run.done.
    let types: Vec<&str> = of(&events, &r, None)
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .filter(|t| ["run.started", "run.text", "run.done"].contains(t))
        .collect();
    assert!(types.len() >= 3, "{types:?}");
    assert_eq!(types[0], "run.started");
    assert_eq!(types[types.len() - 1], "run.done");
    assert!(
        types[1..types.len() - 1].iter().all(|t| *t == "run.text"),
        "{types:?}"
    );

    // 2.1's second check: the exact flags, and stdin still open.
    let log = fixture_log(&home);
    let last = &log["starts"].as_array().unwrap().last().unwrap().clone();
    let argv: Vec<String> = last["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(after(&argv, "--session-id"), Some(r.as_str()));
    assert_eq!(after(&argv, "--permission-mode"), Some("dontAsk"));
    assert_eq!(after(&argv, "--setting-sources"), Some("project"));
    assert!(has(&argv, "--verbose") && has(&argv, "--settings"));
    assert!(has(&argv, "--strict-mcp-config"));
    assert!(!has(&argv, "--allowedTools") && !has(&argv, "--bare"));
    assert_eq!(
        last["stdin_eof_at"],
        Value::Null,
        "stdin closed early: {last:#}"
    );
    assert!(
        Path::new(&argv[0]).is_absolute(),
        "claude is run by its absolute path"
    );
    assert_eq!(last["stdin"][0]["type"], "user");

    // 7.5: the capture is a valid run-event-list.json.
    assert_valid("run-event-list.json", &Value::Array(events.clone()));
    // 7.3: seq and run_seq increase strictly.
    let seqs: Vec<u64> = events.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
    let run_seqs: Vec<u64> = of(&events, &r, None)
        .iter()
        .map(|e| e["data"]["run_seq"].as_u64().unwrap())
        .collect();
    assert!(!run_seqs.is_empty() && run_seqs.windows(2).all(|w| w[0] < w[1]));

    // 2.5: the run worked in the space.
    let started = of(&events, &r, Some("run.started"));
    assert_eq!(started.len(), 1);
    assert_eq!(started[0]["data"]["cwd"], p.as_str());
    assert_eq!(started[0]["data"]["attempt"], 1);
    assert_eq!(started[0]["data"]["kind"], "task");

    // Durable before visible: once a client has seen run.done, the run's log on disk holds
    // every run event the client saw, the same lines, and its record says done.
    let dir = home.root().join("rlxd").join("runs").join(&r);
    let record: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("run.json")).unwrap()).unwrap();
    assert_eq!(record["state"], "done", "{record:#}");
    let logged: Vec<Value> = std::fs::read_to_string(dir.join("events.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let seen: Vec<Value> = of(&events, &r, None).into_iter().cloned().collect();
    assert_eq!(logged, seen);
    assert_eq!(
        record["last_run_seq"],
        seen.last().unwrap()["data"]["run_seq"]
    );

    // 7.5 Done: usage is there.
    let done = of(&events, &r, Some("run.done"));
    assert!(done[0]["data"]["usage"]["output_tokens"].is_number());
    assert_eq!(done[0]["data"]["session"], r.as_str());
}

#[test]
fn a_fake_claude_without_verbose_exits_1_as_claude_code_does() {
    let home = TestHome::new();
    let claude = link(home.root(), "hello");
    let program = if cfg!(windows) {
        claude.with_extension("exe")
    } else {
        claude
    };
    let run = home.run_of(&program, &["-p", "--output-format", "stream-json"]);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert!(run.stderr.contains("requires --verbose"), "{run:?}");
}

#[test]
fn refused_starts_name_their_reason() {
    let home = home("hello");
    let params = task("hi");
    let r = run_id();
    start(&home, &params, &r);
    let again = home.rlxd(&["call", "run.start", &with_id(&params, &r)]);
    assert_eq!(again.code(), Some(1), "{again:?}");
    assert_eq!(again.error_code(), "run_exists");

    let check = json!({"kind": "check", "prompt": "x",
        "mount": {"kind": "connectors", "connectors": ["claude.ai Slack"]}});
    let refused = home.rlxd(&["call", "run.start", &with_id(&check, &run_id())]);
    assert_eq!(refused.error_code(), "kind_mount_mismatch", "{refused:?}");

    let send = json!({"kind": "send_executor", "prompt": "x",
        "mount": {"kind": "connectors", "connectors": ["claude.ai Slack"]}});
    let refused = home.rlxd(&["call", "run.start", &with_id(&send, &run_id())]);
    assert_eq!(refused.error_code(), "pinned_call_required", "{refused:?}");

    let needs = json!({"kind": "task", "prompt": "x", "mount": {"kind": "no_tools"},
        "needs_connectors": ["claude.ai Slack"]});
    let refused = home.rlxd(&["call", "run.start", &with_id(&needs, &run_id())]);
    assert_eq!(refused.error_code(), "not_implemented", "{refused:?}");
    assert!(
        refused.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("needs_connectors")
    );

    let upper = home.rlxd(&[
        "call",
        "run.start",
        &with_id(&params, &run_id().to_uppercase()),
    ]);
    assert_eq!(upper.error_code(), "invalid_params", "{upper:?}");
    assert!(upper.stdout.contains("UUID in lower case"), "{upper:?}");

    for space in ["../x", ".hidden", "a/b", "a..b", "Default", "con", ""] {
        let mut bad = params.clone();
        bad["space"] = Value::String(space.to_owned());
        let refused = home.rlxd(&["call", "run.start", &with_id(&bad, &run_id())]);
        assert_eq!(
            refused.error_code(),
            "invalid_params",
            "{space}: {refused:?}"
        );
        assert!(
            refused.stdout.contains("space name"),
            "{space}: {refused:?}"
        );
        let ensured = home.rlxd(&["call", "space.ensure", &json!({"name": space}).to_string()]);
        assert_eq!(
            ensured.error_code(),
            "invalid_params",
            "{space}: {ensured:?}"
        );
    }
    assert!(!home.root().join("x").exists());
}

#[test]
fn structured_output_reaches_run_done_and_the_schema_reaches_claude() {
    let home = home("structured");
    let schema = json!({"type": "object", "required": ["verdict"]});
    let mut params = task("judge it");
    params["output_schema"] = schema.clone();
    let (r, _, events) = gated_start(&home, &params).wait();
    let argv = argv_of(&home, &r);
    let given: Value = serde_json::from_str(after(&argv, "--json-schema").unwrap()).unwrap();
    assert_eq!(given, schema);
    let path = common::workspace()
        .join("tests")
        .join("fixtures")
        .join("fake-claude")
        .join("structured")
        .join("output.json");
    let expected: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let done = of(&events, &r, Some("run.done"));
    assert_eq!(done.len(), 1, "{events:#?}");
    assert_eq!(done[0]["data"]["structured_output"], expected);
}

#[test]
fn partial_text_streams_only_when_asked_for() {
    let home = home("hello");
    let mut params = task("hi");
    params["partial"] = json!(true);
    let (r, _, events) = gated_start(&home, &params).wait();
    assert!(has(&argv_of(&home, &r), "--include-partial-messages"));
    let partial = |events: &[Value], r: &str| {
        of(events, r, Some("run.text"))
            .iter()
            .filter(|e| e["data"]["partial"] == true)
            .count()
    };
    assert!(partial(&events, &r) > 0, "{events:#?}");
    let whole = of(&events, &r, Some("run.text"));
    assert!(whole.iter().any(|e| e["data"]["partial"] == false));

    let (r, _, events) = gated_start(&home, &task("hi")).wait();
    assert!(!has(&argv_of(&home, &r), "--include-partial-messages"));
    assert_eq!(partial(&events, &r), 0, "{events:#?}");
}

#[test]
fn the_system_prompt_is_appended() {
    let home = home("hello");
    let mut params = task("hi");
    params["append_system_prompt"] = json!("You are Nova.");
    let (r, _, _) = gated_start(&home, &params).wait();
    assert_eq!(
        after(&argv_of(&home, &r), "--append-system-prompt"),
        Some("You are Nova.")
    );
}

#[test]
fn runs_but_tasks_are_session_less_by_default() {
    let home = home("hello");
    let refresh = json!({"kind": "widget_refresh", "prompt": "refresh",
        "mount": {"kind": "no_tools"}});
    let (r, _, events) = gated_start(&home, &refresh).wait();
    assert!(has(&argv_of(&home, &r), "--no-session-persistence"));
    let done = of(&events, &r, Some("run.done"));
    assert_eq!(done[0]["data"]["session"], Value::Null, "{events:#?}");

    let (r, _, _) = gated_start(&home, &task("hi")).wait();
    assert!(!has(&argv_of(&home, &r), "--no-session-persistence"));
}

#[test]
fn each_kind_queues_in_its_own_pool() {
    let home = home_with("slow", &[("FAKE_CLAUDE_SLOW_MS", "3000")]);
    let since = seq(&home);
    let slow = task("take your time");
    let (r1, r2, r3) = (run_id(), run_id(), run_id());
    start(&home, &slow, &r1);
    start(&home, &slow, &r2);
    start(&home, &slow, &r3);
    let r4 = run_id();
    start(
        &home,
        &json!({"kind": "check", "prompt": "ok?", "mount": {"kind": "no_tools"}}),
        &r4,
    );
    let events = wait_until(&home, since, 40, "all four end", |events| {
        [&r1, &r2, &r3, &r4].iter().all(|r| ended(events, r))
    });
    let queued = of(&events, &r3, Some("run.queued"));
    assert_eq!(queued.len(), 1, "{events:#?}");
    assert_eq!(queued[0]["data"]["position"], 1);
    let first_end = at(&events, &r1, "run.done").min(at(&events, &r2, "run.done"));
    assert!(at(&events, &r3, "run.started") > first_end, "{events:#?}");
    assert!(of(&events, &r4, Some("run.queued")).is_empty());
    assert!(at(&events, &r4, "run.started") < at(&events, &r1, "run.done"));
    for r in [&r1, &r2] {
        assert!(of(&events, r, Some("run.queued")).is_empty());
    }

    let since = seq(&home);
    let refresh = json!({"kind": "widget_refresh", "prompt": "x", "mount": {"kind": "no_tools"}});
    let (w1, w2) = (run_id(), run_id());
    start(&home, &refresh, &w1);
    start(&home, &refresh, &w2);
    let events = wait_until(&home, since, 30, "both refreshes end", |events| {
        ended(events, &w1) && ended(events, &w2)
    });
    assert_eq!(of(&events, &w2, Some("run.queued")).len(), 1);
    assert!(at(&events, &w2, "run.started") > at(&events, &w1, "run.done"));
}

#[test]
fn a_rate_limit_event_is_run_rate_limited_with_its_reset() {
    let home = home("rate-limit");
    let (r, _, events) = gated_start(&home, &task("hi")).wait();
    let limited = of(&events, &r, Some("run.rate_limited"));
    assert_eq!(limited.len(), 1, "{events:#?}");
    assert!(limited[0]["data"]["resets_at"].is_string());
    assert_valid("run-event-list.json", &Value::Array(events.clone()));
}

#[test]
fn a_run_past_its_timeout_fails_timeout_and_leaves_no_process() {
    let home = home("slow");
    let mut params = task("take your time");
    params["timeout_s"] = json!(2);
    let begun = Instant::now();
    let (r, _, events) = gated_start(&home, &params).wait();
    assert!(
        begun.elapsed() < Duration::from_secs(10),
        "{:?}",
        begun.elapsed()
    );
    let last = of(&events, &r, None).last().copied().unwrap().clone();
    assert_eq!(last["type"], "run.failed", "{events:#?}");
    assert_eq!(last["data"]["reason"], "timeout");
    assert!(
        last["data"]["partial_text"]
            .as_str()
            .unwrap()
            .contains("Working")
    );
    common::eventually("no slow process is left", || {
        processes_matching(&pattern(&home, "slow")) == 0
    });
}

#[test]
fn a_crash_mid_stream_fails_error_with_the_partial_text() {
    let home = home("crash");
    let (r, _, events) = gated_start(&home, &task("go")).wait();
    let last = of(&events, &r, None).last().copied().unwrap().clone();
    assert_eq!(last["type"], "run.failed", "{events:#?}");
    assert_eq!(last["data"]["reason"], "error");
    assert!(!last["data"]["partial_text"].as_str().unwrap().is_empty());
    assert!(
        last["data"]["message"]
            .as_str()
            .unwrap()
            .contains("crashed on purpose")
    );
}

#[test]
fn cancel_ends_a_running_run_and_its_tree_and_a_queued_one() {
    let home = home("slow");
    let since = seq(&home);
    let r = run_id();
    start(&home, &task("take your time"), &r);
    wait_until(&home, since, 20, "the run starts", |events| {
        !of(events, &r, Some("run.started")).is_empty()
    });
    assert!(processes_matching(&pattern(&home, "slow")) >= 1);
    let cancelled = home.rlxd(&["call", "run.cancel", &json!({"run_id": r}).to_string()]);
    assert_eq!(cancelled.code(), Some(0), "{cancelled:?}");
    assert_eq!(cancelled.data()["cancelled"], true);
    let capture = home.rlxd(&[
        "events",
        "--topics",
        "run",
        "--since",
        &since.to_string(),
        "--follow",
        "--until",
        "run.failed",
        "--timeout",
        "5",
    ]);
    assert_eq!(capture.code(), Some(0), "{capture:?}");
    let failed = of(&capture.lines(), &r, Some("run.failed"))
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["data"]["reason"], "cancelled");
    assert!(
        failed[0]["data"]["partial_text"]
            .as_str()
            .unwrap()
            .contains("Working")
    );
    common::eventually("no slow process is left", || {
        processes_matching(&pattern(&home, "slow")) == 0
    });
    let again = home.rlxd(&["call", "run.cancel", &json!({"run_id": r}).to_string()]);
    assert_eq!(
        again.data()["cancelled"],
        false,
        "an ended run stays as it ended"
    );

    // A queued run: two refreshes, the second waits; cancel it.
    let since = seq(&home);
    let refresh = json!({"kind": "widget_refresh", "prompt": "x", "mount": {"kind": "no_tools"}});
    let (w1, w2) = (run_id(), run_id());
    start(&home, &refresh, &w1);
    start(&home, &refresh, &w2);
    let cancelled = home.rlxd(&["call", "run.cancel", &json!({"run_id": w2}).to_string()]);
    assert_eq!(cancelled.data()["cancelled"], true, "{cancelled:?}");
    let events = wait_until(&home, since, 10, "the queued run fails", |e| ended(e, &w2));
    let failed = of(&events, &w2, Some("run.failed"));
    assert_eq!(failed[0]["data"]["reason"], "cancelled");
    assert!(of(&events, &w2, Some("run.started")).is_empty());
    let unknown = home.rlxd(&[
        "call",
        "run.cancel",
        &json!({"run_id": run_id()}).to_string(),
    ]);
    assert_eq!(unknown.error_code(), "unknown_run");
    home.rlxd(&["call", "run.cancel", &json!({"run_id": w1}).to_string()]);
}

#[test]
fn stopping_rlxd_ends_a_running_run_engine_restarted_and_its_tree() {
    let home = home("slow");
    let since = seq(&home);
    let r = run_id();
    start(&home, &task("take your time"), &r);
    wait_until(&home, since, 20, "the run starts", |events| {
        !of(events, &r, Some("run.started")).is_empty()
    });
    home.stop();
    common::eventually("no slow process is left", || {
        processes_matching(&pattern(&home, "slow")) == 0
    });
    let record: Value = serde_json::from_str(
        &std::fs::read_to_string(
            home.root()
                .join("rlxd")
                .join("runs")
                .join(&r)
                .join("run.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(record["state"], "failed");
    let log = std::fs::read_to_string(
        home.root()
            .join("rlxd")
            .join("runs")
            .join(&r)
            .join("events.jsonl"),
    )
    .unwrap();
    let last: Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
    assert_eq!(last["type"], "run.failed");
    assert_eq!(last["data"]["reason"], "engine_restarted");
}

#[test]
fn each_attempt_has_its_own_settings_with_the_absolute_hook() {
    let home = home("hello");
    let mut params = task("hi");
    params["deadline_ms"] = json!(1000);
    let (r, _, _) = gated_start(&home, &params).wait();
    let path = home
        .root()
        .join("rlxd")
        .join("runs")
        .join(&r)
        .join("1")
        .join("settings.json");
    let settings: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let hook = &settings["hooks"]["PreToolUse"][0];
    assert_eq!(hook["matcher"], "*");
    let command = hook["hooks"][0]["command"].as_str().unwrap();
    let rest = format!(" pre-tool-use --run {r} --attempt 1");
    assert!(command.ends_with(&rest), "{command}");
    let program = command.strip_suffix(&rest).unwrap().trim_matches('"');
    assert!(Path::new(program).is_absolute(), "{command}");
    assert!(
        program.ends_with("rlxd-hook") || program.ends_with("rlxd-hook.exe"),
        "{command}"
    );
    if !cfg!(windows) {
        // §5.1's own check: ^"?/[^"]*/rlxd-hook"? pre-tool-use
        let unquoted = command.strip_prefix('"').unwrap_or(command);
        assert!(
            unquoted.starts_with('/') && !program.contains('"'),
            "{command}"
        );
    }
    assert_eq!(hook["hooks"][0]["timeout"], 11);
    assert_eq!(after(&argv_of(&home, &r), "--settings"), path.to_str());
    let space = home
        .root()
        .join("spaces")
        .join("default")
        .join(".claude")
        .join("settings.json");
    let shared = std::fs::read_to_string(space).unwrap();
    assert!(
        !shared.contains(&r),
        "nothing run-specific in the space's settings"
    );
}

#[test]
fn a_tool_result_block_is_run_tool_result_with_its_content() {
    let home = home("tool-result");
    let (r, _, events) = gated_start(&home, &task("search")).wait();
    let results = of(&events, &r, Some("run.tool_result"));
    assert_eq!(results.len(), 1, "{events:#?}");
    let path = common::workspace()
        .join("tests")
        .join("fixtures")
        .join("fake-claude")
        .join("tool-result")
        .join("content.json");
    let expected: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let data = &results[0]["data"];
    assert_eq!(data["call_id"], "toolu_01FakeToolResult0000000001");
    assert_eq!(data["tool"], "mcp__claude_ai_Slack__slack_search_public");
    assert_eq!(data["content"], expected);
    assert_eq!(data["truncated"], false);
    assert_eq!(data["is_error"], false);
    assert_valid("run-event-list.json", &Value::Array(events.clone()));
}

#[test]
fn status_counts_live_and_queued_runs() {
    let home = home("slow");
    let since = seq(&home);
    let refresh = json!({"kind": "widget_refresh", "prompt": "x", "mount": {"kind": "no_tools"}});
    let (w1, w2) = (run_id(), run_id());
    start(&home, &refresh, &w1);
    start(&home, &refresh, &w2);
    let status = home.status();
    assert_eq!(
        status["runs"],
        json!({"live": 1, "queued": 1}),
        "{status:#}"
    );
    for w in [&w1, &w2] {
        home.rlxd(&["call", "run.cancel", &json!({"run_id": w}).to_string()]);
    }
    wait_until(&home, since, 10, "both end", |e| {
        ended(e, &w1) && ended(e, &w2)
    });
    assert_eq!(home.status()["runs"], json!({"live": 0, "queued": 0}));
}

#[test]
fn a_run_cancelled_as_its_turn_comes_never_starts_and_ends_once() {
    // A place given back waits 1.5 s before its next run starts (test builds only), so the
    // cancel lands after the run has left its queue and before it starts: the window a
    // real cancel can hit.
    let home = home_with("slow", &[("RLXD_TEST_LAUNCH_DELAY_MS", "1500")]);
    let since = seq(&home);
    let refresh = json!({"kind": "widget_refresh", "prompt": "x", "mount": {"kind": "no_tools"}});
    let (w1, w2, w3) = (run_id(), run_id(), run_id());
    start(&home, &refresh, &w1);
    start(&home, &refresh, &w2);
    wait_until(&home, since, 20, "w1 starts", |e| {
        !of(e, &w1, Some("run.started")).is_empty()
    });
    let first = home.rlxd(&["call", "run.cancel", &json!({"run_id": w1}).to_string()]);
    assert_eq!(first.code(), Some(0), "{first:?}");
    // w1's place is on its way to w2, which has left its queue.
    let cancelled = home
        .rlxd(&["call", "run.cancel", &json!({"run_id": w2}).to_string()])
        .data();
    assert_eq!(cancelled["cancelled"], true, "{cancelled:#}");
    thread::sleep(Duration::from_millis(2500));
    let events = replay(&home, since);
    let ends: Vec<&Value> = of(&events, &w2, None)
        .into_iter()
        .filter(|e| {
            ["run.done", "run.failed", "run.needs_you"].contains(&e["type"].as_str().unwrap())
        })
        .collect();
    assert_eq!(ends.len(), 1, "one end: {events:#?}");
    assert_eq!(ends[0]["data"]["reason"], "cancelled");
    assert!(
        of(&events, &w2, Some("run.started")).is_empty(),
        "{events:#?}"
    );
    let log = fixture_log(&home);
    let started_w2 = log["starts"].as_array().unwrap().iter().any(|s| {
        s["argv"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == w2.as_str())
    });
    assert!(
        !started_w2,
        "no Claude Code ran for the cancelled run: {log:#}"
    );
    let record: Value = serde_json::from_str(
        &std::fs::read_to_string(
            home.root()
                .join("rlxd")
                .join("runs")
                .join(&w2)
                .join("run.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(record["state"], "failed");
    // The place it never took went on: the next run starts.
    let mut quick = refresh.clone();
    quick["prompt"] = json!("quick");
    start(&home, &quick, &w3);
    wait_until(&home, since, 20, "w3 runs", |e| ended(e, &w3));
}

#[test]
fn what_claude_code_leaves_running_ends_with_its_run() {
    let home = home("orphan");
    let (r, _, events) = gated_start(&home, &task("start it")).wait();
    assert_eq!(
        of(&events, &r, None).last().unwrap()["type"],
        "run.done",
        "{events:#?}"
    );
    // The lingering process would sleep a minute: the run's end takes it, and nothing
    // of the run is left.
    common::eventually("nothing of the run is left", || {
        processes_matching(&pattern(&home, "orphan")) == 0
    });
}

#[cfg(unix)]
#[test]
fn a_claude_code_killed_by_a_signal_is_said_to_be() {
    let home = home("abort");
    let (r, _, events) = gated_start(&home, &task("go")).wait();
    let last = of(&events, &r, None).last().copied().unwrap().clone();
    assert_eq!(last["type"], "run.failed", "{events:#?}");
    assert_eq!(last["data"]["reason"], "error");
    let message = last["data"]["message"].as_str().unwrap();
    assert!(
        message.contains("signal: 6"),
        "the guard passes the signal on: {message}"
    );
}

#[test]
fn a_result_too_large_for_run_done_still_ends_the_run_with_its_reason() {
    let home = home("huge");
    let (r, _, events) = gated_start(&home, &task("go")).wait();
    let last = of(&events, &r, None).last().copied().unwrap().clone();
    assert_eq!(last["type"], "run.failed", "{events:#?}");
    assert!(
        last["data"]["message"]
            .as_str()
            .unwrap()
            .starts_with("output_too_large"),
        "{last:#}"
    );
}

#[test]
fn a_slow_disk_under_one_run_holds_up_no_other_run_or_topic() {
    // Test builds only: the state events of runs in the space "slowdisk" wait 3 s before
    // they are made durable, as under a slow fsync.
    let home = home_with("hello", &[("RLXD_TEST_SLOW_PERSIST", "slowdisk:3000")]);
    let since = seq(&home);
    let mut slow = task("hi");
    slow["space"] = json!("slowdisk");
    let a = run_id();
    start(&home, &slow, &a);
    // Wait until A's Claude Code has started: its run.started is then being made durable.
    common::eventually("A's Claude Code starts", || {
        std::fs::read_to_string(home.root().join("fake-claude.log.json"))
            .is_ok_and(|log| log.contains(&a))
    });
    thread::sleep(Duration::from_millis(300));
    let quick = |what: &str, args: &[&str]| {
        let began = Instant::now();
        let run = home.rlxd(args);
        assert_eq!(run.code(), Some(0), "{what}: {run:?}");
        assert!(
            began.elapsed() < Duration::from_secs(1),
            "{what} waited {:?}",
            began.elapsed()
        );
        run
    };
    let emitted = quick("debug.emit", &["call", "debug.emit", r#"{"count":3}"#]);
    let last = emitted.data()["last"].as_u64().unwrap();
    quick(
        "a follower of another topic",
        &[
            "events",
            "--topics",
            "daemon",
            "--since",
            &(last - 1).to_string(),
        ],
    );
    quick("status", &["call", "status"]);
    quick(
        "space.ensure",
        &["call", "space.ensure", r#"{"name":"other"}"#],
    );
    // Another run, in another space, starts and ends meanwhile.
    let b = run_id();
    start(&home, &task("hi"), &b);
    let events = wait_until(&home, since, 20, "B ends", |e| ended(e, &b));
    assert!(
        of(&events, &a, Some("run.started")).is_empty(),
        "B ended while A's run.started was still being made durable: {events:#?}"
    );
    // A goes on once its disk does.
    wait_until(&home, since, 30, "A ends", |e| ended(e, &a));
}
