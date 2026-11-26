//! The runner's Accept checks (handoff 0.10.3, 2.1, 2.5 and 7.5), through the real socket,
//! with fake-claude standing in for Claude Code on every OS ("How to read the checks").
//!
//! Each test gets a fresh home, a `CLAUDE_CONFIG_DIR` and a user home (`HOME`,
//! `USERPROFILE`) inside it, so no test reads or writes a real `~/.claude`, and fake-claude linked
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
    // A user home of its own too, so nothing reads or writes the real ~/.claude
    // (space.erase looks there as well as in CLAUDE_CONFIG_DIR).
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

    // A queued run: refreshes, one at a time; w2 and w3 wait; cancel w2. It leaves its
    // queue: the next run to wait is second, after w3, not third.
    let since = seq(&home);
    let refresh = json!({"kind": "widget_refresh", "prompt": "x", "mount": {"kind": "no_tools"}});
    let (w1, w2, w3, w4) = (run_id(), run_id(), run_id(), run_id());
    start(&home, &refresh, &w1);
    start(&home, &refresh, &w2);
    start(&home, &refresh, &w3);
    let cancelled = home.rlxd(&["call", "run.cancel", &json!({"run_id": w2}).to_string()]);
    assert_eq!(cancelled.data()["cancelled"], true, "{cancelled:?}");
    let events = wait_until(&home, since, 10, "the queued run fails", |e| ended(e, &w2));
    let failed = of(&events, &w2, Some("run.failed"));
    assert_eq!(failed[0]["data"]["reason"], "cancelled");
    assert!(of(&events, &w2, Some("run.started")).is_empty());
    start(&home, &refresh, &w4);
    let events = wait_until(&home, since, 10, "w4 waits", |e| {
        !of(e, &w4, Some("run.queued")).is_empty()
    });
    assert_eq!(
        of(&events, &w4, Some("run.queued"))[0]["data"]["position"],
        2
    );
    for w in [&w3, &w4] {
        home.rlxd(&["call", "run.cancel", &json!({"run_id": w}).to_string()]);
    }
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

// --- 5.4 and 7.5, the lifecycle: needs-you, resume, restart, list, attach, erase ---------

/// `rlxd call method params`, which must succeed: its data.
fn call(home: &TestHome, method: &str, params: &Value) -> Value {
    let run = home.rlxd(&["call", method, &params.to_string()]);
    assert_eq!(run.code(), Some(0), "{method}: {run:?}");
    run.data()
}

/// `rlxd call method params`, which must be refused: its error code.
fn refused(home: &TestHome, method: &str, params: &Value) -> String {
    let run = home.rlxd(&["call", method, &params.to_string()]);
    assert_eq!(run.code(), Some(1), "{method}: {run:?}");
    run.error_code()
}

#[test]
fn needs_you_then_resume_with_the_reply_as_attempt_2() {
    let home = home("asks");
    let mut params = task("post the standup");
    params["needs_you_when"] =
        json!({"pointer": "/status", "equals": "needs_you", "question_pointer": "/question"});
    let (r, s, events) = gated_start(&home, &params).wait();
    let mine = of(&events, &r, None);
    let last = mine.last().unwrap();
    assert_eq!(last["type"], "run.needs_you", "{events:#?}");
    assert_eq!(last["data"]["question"], "Which channel?");
    assert_eq!(last["data"]["reason"], "question");
    // 7.3: needs_you, from runs, on the same capture.
    let raised: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "needs_you.raised" && e["data"]["run_id"] == r.as_str())
        .collect();
    assert_eq!(raised.len(), 1, "{events:#?}");
    assert_eq!(raised[0]["data"]["source"], "run");
    assert_eq!(raised[0]["data"]["text"], "Which channel?");
    let raised_id = raised[0]["data"]["id"].clone();
    assert_valid("run-event-list.json", &Value::Array(events.clone()));
    assert_eq!(
        call(&home, "run.get", &json!({"run_id": r}))["state"],
        "needs_you"
    );

    let resumed = call(&home, "run.resume", &json!({"run_id": r, "reply": "#dev"}));
    assert_eq!(resumed["attempt"], 2);
    let resumed_run = home.rlxd(&[
        "events",
        "--topics",
        "run,needs_you",
        "--since",
        &s.to_string(),
        "--follow",
        "--until",
        "run.done",
        "--timeout",
        "30",
    ]);
    assert_eq!(resumed_run.code(), Some(0), "{resumed_run:?}");
    let events = resumed_run.lines();
    let started = of(&events, &r, Some("run.started"));
    assert_eq!(started.len(), 2, "{events:#?}");
    assert_eq!(started[1]["data"]["resumed"], true);
    assert_eq!(started[1]["data"]["attempt"], 2);
    let done = of(&events, &r, Some("run.done"));
    assert_eq!(done[0]["data"]["attempt"], 2);
    let result: Value = serde_json::from_str(done[0]["data"]["result"].as_str().unwrap()).unwrap();
    assert_eq!(
        result["channel"], "#dev",
        "the reply was attempt 2's first message"
    );
    let cleared: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "needs_you.cleared")
        .collect();
    assert_eq!(cleared.len(), 1, "{events:#?}");
    assert_eq!(cleared[0]["data"]["id"], raised_id);

    // The fixture log: two starts, the second resuming the run.
    let log = fixture_log(&home);
    let starts = log["starts"].as_array().unwrap();
    assert_eq!(starts.len(), 2, "{log:#}");
    let second: Vec<String> = starts[1]["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(after(&second, "--resume"), Some(r.as_str()));
    assert!(!has(&second, "--session-id"));
    // A resumed attempt gets its own settings, naming it.
    let settings = home
        .root()
        .join("rlxd")
        .join("runs")
        .join(&r)
        .join("2")
        .join("settings.json");
    assert_eq!(after(&second, "--settings"), settings.to_str());
    let text = std::fs::read_to_string(&settings).unwrap();
    assert!(text.contains(&format!("--run {r} --attempt 2")), "{text}");
}

#[test]
fn resume_refuses_a_live_run_a_send_and_a_session_less_run() {
    let home = home("slow");
    let since = seq(&home);
    let live = run_id();
    start(&home, &task("take your time"), &live);
    assert_eq!(
        refused(&home, "run.resume", &json!({"run_id": live})),
        "run_alive"
    );

    let send = json!({"kind": "send_executor", "prompt": "quick",
        "mount": {"kind": "connectors", "connectors": ["claude.ai Slack"]},
        "pinned_call": {"tool": "mcp__claude_ai_Slack__slack_send_message",
            "input_sha256": "cdab067e9f3beb32d1252cfd63e492592fecbf591b0d08cadb24bb17f3864246"}});
    let sent = run_id();
    start(&home, &send, &sent);
    let refresh =
        json!({"kind": "widget_refresh", "prompt": "quick", "mount": {"kind": "no_tools"}});
    let fresh = run_id();
    start(&home, &refresh, &fresh);
    wait_until(&home, since, 20, "both end", |e| {
        ended(e, &sent) && ended(e, &fresh)
    });
    assert_eq!(
        refused(&home, "run.resume", &json!({"run_id": sent})),
        "not_resumable"
    );
    assert_eq!(
        refused(&home, "run.resume", &json!({"run_id": fresh})),
        "no_session"
    );
    assert_eq!(
        refused(&home, "run.resume", &json!({"run_id": run_id()})),
        "unknown_run"
    );
    home.rlxd(&["call", "run.cancel", &json!({"run_id": live}).to_string()]);
}

#[test]
fn after_kill_9_and_a_restart_the_run_failed_engine_restarted_and_no_process_is_left() {
    let home = home("slow");
    let since = seq(&home);
    let r = run_id();
    start(&home, &task("take your time"), &r);
    wait_until(&home, since, 20, "the run starts", |events| {
        !of(events, &r, Some("run.started")).is_empty()
    });
    assert!(processes_matching(&pattern(&home, "slow")) >= 1);
    let pid = home.status()["pid"].as_u64().unwrap();
    common::kill_hard(pid);
    common::eventually("rlxd is gone", || {
        home.rlxd(&["call", "status"]).code() == Some(3)
    });
    home.start_dev();
    let listed = call(&home, "run.list", &json!({}));
    let mine: Vec<&Value> = listed["runs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|run| run["run_id"] == r.as_str())
        .collect();
    assert_eq!(mine.len(), 1, "{listed:#}");
    assert_eq!(mine[0]["state"], "failed");
    let got = call(&home, "run.get", &json!({"run_id": r}));
    assert_eq!(
        got["last_event"]["data"]["reason"], "engine_restarted",
        "{got:#}"
    );
    assert!(
        got["last_event"]["data"]["partial_text"]
            .as_str()
            .unwrap()
            .contains("Working"),
        "{got:#}"
    );
    common::eventually("no slow process is left", || {
        processes_matching(&pattern(&home, "slow")) == 0
    });
    // Its log goes on from the run_seq it had reached: no number twice.
    let logged = call(&home, "run.get", &json!({"run_id": r}))["last_run_seq"]
        .as_u64()
        .unwrap();
    let replayed = home.rlxd(&["call", "run.events", &json!({"run_id": r}).to_string()]);
    assert_eq!(replayed.code(), Some(0), "{replayed:?}");
    let run_seqs: Vec<u64> = replayed
        .lines()
        .iter()
        .map(|e| e["data"]["run_seq"].as_u64().unwrap())
        .collect();
    assert_eq!(run_seqs, (1..=logged).collect::<Vec<_>>());
    // And it resumes.
    let resumed = call(&home, "run.resume", &json!({"run_id": r}));
    assert_eq!(resumed["attempt"], 2);
    home.rlxd(&["call", "run.cancel", &json!({"run_id": r}).to_string()]);
}

#[test]
fn run_list_filters_by_state_job_and_limit() {
    let home = home("slow");
    let since = seq(&home);
    let live = run_id();
    start(&home, &task("take your time"), &live);
    let finished = run_id();
    start(&home, &task("quick"), &finished);
    wait_until(&home, since, 20, "the quick run ends", |e| {
        ended(e, &finished)
    });
    let running = call(&home, "run.list", &json!({"states": ["running"]}));
    let runs = running["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1, "{running:#}");
    assert_eq!(runs[0]["run_id"], live.as_str());
    let done = call(&home, "run.list", &json!({"states": ["done"]}));
    let runs = done["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1, "{done:#}");
    assert_eq!(runs[0]["run_id"], finished.as_str());
    assert_eq!(runs[0]["client"], "rlxd-cli");
    assert!(runs[0]["ended_at"].is_string());
    assert_valid("run-list-reply.json", &done);

    // job_id: the scheduler's, set here through the test-only debug.run_start.
    let (j1, j2) = (run_id(), run_id());
    let mut with_job = task("quick");
    with_job["job_id"] = json!("job-1");
    for r in [&j1, &j2] {
        let mut params = with_job.clone();
        params["run_id"] = json!(r);
        let started = home.rlxd(&["call", "debug.run_start", &params.to_string()]);
        assert_eq!(started.code(), Some(0), "{started:?}");
    }
    let events = wait_until(&home, since, 20, "both job runs end", |e| {
        ended(e, &j1) && ended(e, &j2)
    });
    assert!(
        of(&events, &j1, None)
            .iter()
            .all(|e| e["data"]["job_id"] == "job-1")
    );
    let jobs = call(&home, "run.list", &json!({"job_id": "job-1"}));
    let ids: Vec<&str> = jobs["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| run["run_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 2, "{jobs:#}");
    assert!(ids.contains(&j1.as_str()) && ids.contains(&j2.as_str()));
    let limited = call(&home, "run.list", &json!({"limit": 1}));
    assert_eq!(limited["runs"].as_array().unwrap().len(), 1);
    // run.start itself takes no job_id: a client can't claim a job's run.
    let mut claimed = with_job.clone();
    claimed["run_id"] = json!(run_id());
    let plain = home.rlxd(&["call", "run.start", &claimed.to_string()]);
    assert_eq!(
        plain.code(),
        Some(0),
        "an unknown field is ignored: {plain:?}"
    );
    let got = call(&home, "run.get", &json!({"run_id": claimed["run_id"]}));
    assert_eq!(got.get("job_id"), None, "{got:#}");
    home.rlxd(&["call", "run.cancel", &json!({"run_id": live}).to_string()]);
}

#[test]
fn run_events_replays_from_disk_after_the_buffer_dropped_the_run() {
    let home = home("hello");
    let (r, _, _) = gated_start(&home, &task("hi")).wait();
    let got = call(&home, "run.get", &json!({"run_id": r}));
    // 7.5 Done.
    assert_eq!(got["state"], "done");
    assert!(
        got["last_event"]["data"]["usage"]["output_tokens"].is_number(),
        "{got:#}"
    );
    assert_valid("run-get-reply.json", &got);
    let n = got["last_run_seq"].as_u64().unwrap();
    assert!(n > 1, "{got:#}");
    home.emit(10_001);
    let replayed = home.rlxd(&[
        "call",
        "run.events",
        &json!({"run_id": r, "from_run_seq": 2}).to_string(),
    ]);
    assert_eq!(replayed.code(), Some(0), "{replayed:?}");
    let events = replayed.lines();
    assert_eq!(events.len() as u64, n - 1, "{events:#?}");
    assert_eq!(events[0]["data"]["run_seq"], 2);
    assert_eq!(events[events.len() - 1]["data"]["run_seq"], n);
    assert_valid("run-event-list.json", &Value::Array(events));
    let unknown = home.rlxd(&[
        "call",
        "run.events",
        &json!({"run_id": run_id()}).to_string(),
    ]);
    assert_eq!(unknown.code(), Some(1));
}

#[test]
fn run_events_follows_a_live_run_until_it_ends() {
    let home = home_with("slow", &[("FAKE_CLAUDE_SLOW_MS", "1500")]);
    let since = seq(&home);
    let r = run_id();
    start(&home, &task("take your time"), &r);
    wait_until(&home, since, 20, "the run starts", |events| {
        !of(events, &r, Some("run.started")).is_empty()
    });
    let followed = home.rlxd(&["call", "run.events", &json!({"run_id": r}).to_string()]);
    assert_eq!(followed.code(), Some(0), "{followed:?}");
    let events = followed.lines();
    assert_eq!(events.last().unwrap()["type"], "run.done", "{events:#?}");
    let run_seqs: Vec<u64> = events
        .iter()
        .map(|e| e["data"]["run_seq"].as_u64().unwrap())
        .collect();
    assert_eq!(run_seqs, (1..=run_seqs.len() as u64).collect::<Vec<_>>());
}

#[test]
fn the_end_state_structured_output_and_finish_survive_a_restart() {
    let home = home("structured");
    let mut params = task("judge it");
    params["output_schema"] = json!({"type": "object", "required": ["verdict"]});
    let (r, _, _) = gated_start(&home, &params).wait();
    let before = call(&home, "run.get", &json!({"run_id": r}));
    home.stop();
    home.start_dev();
    let after = call(&home, "run.get", &json!({"run_id": r}));
    assert_eq!(after["state"], "done");
    assert!(after["finished_at"].is_string());
    assert_eq!(after["finished_at"], before["finished_at"]);
    assert_eq!(after["structured_output"]["verdict"], "ok");
    assert_eq!(after["structured_output"], before["structured_output"]);
    assert_eq!(after["last_event"], before["last_event"]);
    let listed = call(&home, "run.list", &json!({"states": ["done"]}));
    assert!(
        listed["runs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|run| run["run_id"] == r.as_str())
    );
}

#[test]
fn space_erase_removes_the_space_its_sessions_and_rlxds_records() {
    let home = home("persist");
    let p = call(&home, "space.ensure", &json!({"name": "default"}))["path"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut params = task("hi");
    params["persist_session"] = json!(true);
    let (r, _, _) = gated_start(&home, &params).wait();
    let config = home.root().join("claude-config").join("projects");
    let sessions = |id: &str| -> Vec<PathBuf> {
        std::fs::read_dir(&config)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.path().join(format!("{id}.jsonl")))
                    .filter(|path| path.exists())
                    .collect()
            })
            .unwrap_or_default()
    };
    assert_eq!(
        sessions(&r).len(),
        1,
        "the session file is where Claude Code keeps it"
    );
    let erased = call(&home, "space.erase", &json!({"name": "default"}));
    assert_valid("space-erase-reply.json", &erased);
    let removed: Vec<&str> = erased["removed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    assert!(removed.contains(&p.as_str()), "{erased:#}");
    assert!(sessions(&r).is_empty());
    assert!(!Path::new(&p).exists());
    assert!(!home.root().join("rlxd").join("runs").join(&r).exists());
    let listed = call(&home, "run.list", &json!({}));
    assert!(
        !listed["runs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|run| run["run_id"] == r.as_str())
    );
    assert_eq!(
        refused(&home, "run.get", &json!({"run_id": r})),
        "unknown_run"
    );
}

#[test]
fn space_erase_refuses_while_a_run_in_the_space_is_live() {
    let home = home("slow");
    let since = seq(&home);
    let r = run_id();
    start(&home, &task("take your time"), &r);
    wait_until(&home, since, 20, "the run starts", |events| {
        !of(events, &r, Some("run.started")).is_empty()
    });
    let run = home.rlxd(&["call", "space.erase", r#"{"name":"default"}"#]);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_eq!(run.error_code(), "runs_live");
    assert_eq!(run.json()["error"]["detail"]["run_ids"][0], r.as_str());
    assert!(
        home.root().join("spaces").join("default").exists(),
        "nothing was removed"
    );
    home.rlxd(&["call", "run.cancel", &json!({"run_id": r}).to_string()]);
    // Another space's erase is not held up by it.
    call(&home, "space.ensure", &json!({"name": "other"}));
    call(&home, "space.erase", &json!({"name": "other"}));
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
fn space_erase_follows_no_link() {
    let home = home("hello");
    let elsewhere = home.root().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("keep"), "x").unwrap();
    std::fs::create_dir_all(home.root().join("spaces")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, home.root().join("spaces").join("linked")).unwrap();
    assert_eq!(
        refused(&home, "space.erase", &json!({"name": "linked"})),
        "link_refused"
    );
    assert!(elsewhere.join("keep").exists());
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

#[cfg(unix)]
#[test]
fn a_fifo_in_the_runs_folder_wedges_nothing_and_the_signal_is_still_said() {
    // How Claude Code ended comes from its guard over a pipe: no file of the run's folder
    // is opened for it, so FIFOs where one might be (`<run>/1/exit`, as before) hold up
    // nothing.
    let home = home_with("abort", &[("FAKE_CLAUDE_ABORT_AFTER_MS", "1500")]);
    let since = seq(&home);
    let r = run_id();
    start(&home, &task("go"), &r);
    wait_until(&home, since, 20, "the run talks", |e| {
        !of(e, &r, Some("run.text")).is_empty()
    });
    let attempt = record_path(&home, &r).with_file_name("1");
    for name in ["exit", "status"] {
        let made = std::process::Command::new("mkfifo")
            .arg(attempt.join(name))
            .status()
            .unwrap();
        assert!(made.success(), "mkfifo {name}");
    }
    let events = wait_until(&home, since, 20, "the run ends", |e| ended(e, &r));
    let last = of(&events, &r, None).last().copied().unwrap().clone();
    assert_eq!(last["type"], "run.failed", "{events:#?}");
    let message = last["data"]["message"].as_str().unwrap();
    assert!(message.contains("signal: 6"), "{message}");
    let asked = Instant::now();
    assert_eq!(home.rlxd(&["call", "status"]).code(), Some(0));
    assert!(
        asked.elapsed() < Duration::from_secs(2),
        "{:?}",
        asked.elapsed()
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
    // A cancel of the slow run waits for that run, and holds up nothing else meanwhile.
    thread::scope(|scope| {
        let cancel =
            scope.spawn(|| home.rlxd(&["call", "run.cancel", &json!({"run_id": a}).to_string()]));
        thread::sleep(Duration::from_millis(300));
        quick("status during a cancel", &["call", "status"]);
        quick(
            "run.start during a cancel",
            &["call", "run.start", &with_id(&task("hi"), &run_id())],
        );
        assert_eq!(cancel.join().unwrap().code(), Some(0));
    });
    // A ends, cancelled, once its disk lets it.
    wait_until(&home, since, 30, "A ends", |e| ended(e, &a));
}

#[test]
fn a_place_handed_to_a_run_being_cancelled_on_a_slow_disk_stalls_nothing() {
    // Test builds only: state events of runs in "slowdisk" wait 4 s before they are made
    // durable, and a freed place goes to the next run 1.5 s late. Both of a pool's places
    // go to queued runs whose cancels hold their locks over the slow disk: the two
    // launches wait for those locks as blocking work, and no tokio worker waits with them.
    let home = home_with(
        "slow",
        &[
            ("RLXD_TEST_SLOW_PERSIST", "slowdisk:4000"),
            ("RLXD_TEST_LAUNCH_DELAY_MS", "1500"),
        ],
    );
    let since = seq(&home);
    let running = [run_id(), run_id()];
    for r in &running {
        start(&home, &task("take your time"), r);
    }
    wait_until(&home, since, 20, "both run", |e| {
        running
            .iter()
            .all(|r| !of(e, r, Some("run.started")).is_empty())
    });
    let mut slow = task("quick");
    slow["space"] = json!("slowdisk");
    let queued = [run_id(), run_id()];
    for r in &queued {
        start(&home, &slow, r);
    }
    wait_until(&home, since, 20, "both wait", |e| {
        queued
            .iter()
            .all(|r| !of(e, r, Some("run.queued")).is_empty())
    });
    let cancel = |r: &str| home.rlxd(&["call", "run.cancel", &json!({"run_id": r}).to_string()]);
    thread::scope(|scope| {
        // The places go to the queued runs, 1.5 s from now.
        for r in &running {
            scope.spawn(move || assert_eq!(cancel(r).code(), Some(0)));
        }
        common::eventually("the running runs end", || {
            running
                .iter()
                .all(|r| call(&home, "run.get", &json!({"run_id": r}))["state"] == "failed")
        });
        // Their cancels take their locks over 4 s of slow disk, before the launches come.
        let cancels: Vec<_> = queued
            .iter()
            .map(|r| scope.spawn(move || cancel(r)))
            .collect();
        thread::sleep(Duration::from_millis(2200));
        let began = Instant::now();
        let status = home.rlxd(&["call", "status"]);
        assert_eq!(status.code(), Some(0), "{status:?}");
        assert!(
            began.elapsed() < Duration::from_secs(1),
            "status waited {:?} behind the launches",
            began.elapsed()
        );
        for cancelled in cancels {
            assert_eq!(cancelled.join().unwrap().code(), Some(0));
        }
    });
    let events = wait_until(&home, since, 30, "the queued runs end", |e| {
        queued.iter().all(|r| ended(e, r))
    });
    for r in &queued {
        let last = of(&events, r, None).last().copied().unwrap().clone();
        assert_eq!(last["data"]["reason"], "cancelled", "{events:#?}");
        assert!(
            of(&events, r, Some("run.started")).is_empty(),
            "no Claude Code for a cancelled run"
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn a_run_recorded_in_another_boot_is_never_swept() {
    // After a reboot a recorded pid and group id may be anyone's: the start sweep leaves
    // the run's processes alone unless its record is of this boot. (macOS only: on Linux
    // Claude Code dies with its guard by its parent-death signal.)
    let home = home("slow");
    let since = seq(&home);
    let r = run_id();
    start(&home, &task("take your time"), &r);
    wait_until(&home, since, 20, "the run starts", |e| {
        !of(e, &r, Some("run.started")).is_empty()
    });
    let rlxd = home.status()["pid"].as_u64().unwrap();
    let guard = guard_of(&home, &r);
    let killed = std::process::Command::new("kill")
        .args(["-9", &rlxd.to_string(), &guard.to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    common::eventually("rlxd is gone", || {
        home.rlxd(&["call", "status"]).code() == Some(3)
    });
    let path = record_path(&home, &r);
    let mut record: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    record["boot"] = json!("00000000-0000-0000-0000-000000000000");
    std::fs::write(&path, record.to_string()).unwrap();
    home.start_dev();
    let got = call(&home, "run.get", &json!({"run_id": r}));
    assert_eq!(
        got["last_event"]["data"]["reason"], "engine_restarted",
        "{got:#}"
    );
    let left = processes_matching(&pattern(&home, "slow"));
    let _ = std::process::Command::new("pkill")
        .args(["-9", "-f", &pattern(&home, "slow")])
        .status();
    assert_eq!(
        left, 1,
        "the run's Claude Code, of \"another boot\", is left alone"
    );
}

/// The run's record, `<home>/rlxd/runs/<r>/run.json`.
fn record_path(home: &TestHome, r: &str) -> PathBuf {
    home.root()
        .join("rlxd")
        .join("runs")
        .join(r)
        .join("run.json")
}

/// The run's log, `<home>/rlxd/runs/<r>/events.jsonl`.
fn log_path(home: &TestHome, r: &str) -> PathBuf {
    home.root()
        .join("rlxd")
        .join("runs")
        .join(r)
        .join("events.jsonl")
}

/// Claude Code's project folder for the space `p` (its resolved path), in this home's
/// CLAUDE_CONFIG_DIR.
fn project_of(home: &TestHome, p: &str) -> PathBuf {
    let folder: String = p
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    home.root()
        .join("claude-config")
        .join("projects")
        .join(folder)
}

#[test]
fn a_needs_you_raised_before_a_restart_is_cleared_after_it() {
    let home = home("asks");
    let mut params = task("post the standup");
    params["needs_you_when"] =
        json!({"pointer": "/status", "equals": "needs_you", "question_pointer": "/question"});
    let (r, _, events) = gated_start(&home, &params).wait();
    let raised: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "needs_you.raised")
        .collect();
    let id = raised[0]["data"]["id"].clone();
    home.stop();
    home.start_dev();
    let s = seq(&home);
    call(&home, "run.resume", &json!({"run_id": r, "reply": "#dev"}));
    let after = home.rlxd(&[
        "events",
        "--topics",
        "run,needs_you",
        "--since",
        &s.to_string(),
        "--follow",
        "--until",
        "run.done",
        "--timeout",
        "30",
    ]);
    let cleared: Vec<Value> = after
        .lines()
        .into_iter()
        .filter(|e| e["type"] == "needs_you.cleared")
        .collect();
    assert_eq!(cleared.len(), 1, "{after:?}");
    assert_eq!(cleared[0]["data"]["id"], id);
}

#[test]
fn a_run_that_never_started_has_no_session_to_resume() {
    let home = home("slow");
    let since = seq(&home);
    let (t1, t2, t3) = (run_id(), run_id(), run_id());
    for t in [&t1, &t2, &t3] {
        start(&home, &task("take your time"), t);
    }
    wait_until(&home, since, 20, "t3 waits", |e| {
        !of(e, &t3, Some("run.queued")).is_empty()
    });
    call(&home, "run.cancel", &json!({"run_id": t3}));
    assert_eq!(
        refused(&home, "run.resume", &json!({"run_id": t3})),
        "no_session"
    );
    for t in [&t1, &t2] {
        call(&home, "run.cancel", &json!({"run_id": t}));
    }
}

#[test]
fn space_erase_touches_only_the_spaces_own_project_folder() {
    let home = home("persist");
    let p = call(&home, "space.ensure", &json!({"name": "default"}))["path"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut params = task("hi");
    params["persist_session"] = json!(true);
    let (r, _, _) = gated_start(&home, &params).wait();
    let ours = project_of(&home, &p).join(format!("{r}.jsonl"));
    assert!(ours.exists(), "{}", ours.display());
    // A session of the same id in an unrelated project is not the space's.
    let unrelated = home
        .root()
        .join("claude-config")
        .join("projects")
        .join("-Users-someone-Code-unrelated-project");
    std::fs::create_dir_all(&unrelated).unwrap();
    std::fs::write(unrelated.join(format!("{r}.jsonl")), "{}\n").unwrap();
    let erased = call(&home, "space.erase", &json!({"name": "default"}));
    assert!(!ours.exists());
    assert!(unrelated.join(format!("{r}.jsonl")).exists(), "{erased:#}");
    assert!(!erased.to_string().contains("unrelated"), "{erased:#}");
}

#[cfg(unix)]
#[test]
fn a_partial_erase_keeps_its_runs_and_erasing_again_finishes() {
    use std::os::unix::fs::PermissionsExt;
    let home = home("hello");
    let p = call(&home, "space.ensure", &json!({"name": "default"}))["path"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut params = task("hi");
    params["persist_session"] = json!(true);
    let (r, _, _) = gated_start(&home, &params).wait();
    let project = project_of(&home, &p);
    std::fs::set_permissions(&project, std::fs::Permissions::from_mode(0o555)).unwrap();
    let run = home.rlxd(&["call", "space.erase", r#"{"name":"default"}"#]);
    std::fs::set_permissions(&project, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(run.error_code(), "erase_incomplete", "{run:?}");
    let detail = run.json()["error"]["detail"].clone();
    assert!(
        detail["failed"][0]["path"]
            .as_str()
            .unwrap()
            .ends_with(&format!("{r}.jsonl")),
        "{detail}"
    );
    // The run is still known, its record kept; erasing again finishes the job.
    assert_eq!(
        call(&home, "run.get", &json!({"run_id": r}))["state"],
        "done"
    );
    let erased = call(&home, "space.erase", &json!({"name": "default"}));
    assert!(
        erased["removed"]
            .to_string()
            .contains(&format!("{r}.jsonl")),
        "{erased:#}"
    );
    assert_eq!(
        refused(&home, "run.get", &json!({"run_id": r})),
        "unknown_run"
    );
}

#[test]
fn an_end_logged_before_its_record_was_written_is_taken_not_doubled() {
    let home = home("hello");
    let (r, _, _) = gated_start(&home, &task("hi")).wait();
    home.stop();
    // As if rlxd died after the end reached the log and before its record did.
    let mut record: Value =
        serde_json::from_str(&std::fs::read_to_string(record_path(&home, &r)).unwrap()).unwrap();
    record["state"] = json!("running");
    record["last_event"] = Value::Null;
    std::fs::write(record_path(&home, &r), record.to_string()).unwrap();
    home.start_dev();
    let got = call(&home, "run.get", &json!({"run_id": r}));
    assert_eq!(got["state"], "done", "{got:#}");
    assert_eq!(got["last_event"]["type"], "run.done");
    let log = std::fs::read_to_string(log_path(&home, &r)).unwrap();
    assert!(!log.contains("engine_restarted"), "no second end: {log}");
}

#[test]
fn an_end_its_log_lost_is_put_back_from_its_record() {
    let home = home("hello");
    let (r, _, _) = gated_start(&home, &task("hi")).wait();
    home.stop();
    // As if rlxd died after the end's record was written and before its log line was.
    let log = std::fs::read_to_string(log_path(&home, &r)).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    let done: Value = serde_json::from_str(lines[lines.len() - 1]).unwrap();
    assert_eq!(done["type"], "run.done");
    std::fs::write(
        log_path(&home, &r),
        lines[..lines.len() - 1].join("\n") + "\n",
    )
    .unwrap();
    home.start_dev();
    let replayed = home.rlxd(&["call", "run.events", &json!({"run_id": r}).to_string()]);
    let events = replayed.lines();
    let last = events.last().unwrap();
    assert_eq!(last["type"], "run.done", "{events:#?}");
    assert_eq!(last["data"]["run_seq"], done["data"]["run_seq"]);
    assert_eq!(last["data"]["result"], done["data"]["result"]);
    assert_eq!(
        call(&home, "run.get", &json!({"run_id": r}))["state"],
        "done"
    );
}

#[cfg(unix)]
#[test]
fn space_erase_removes_links_inside_the_space_as_links_and_names_them() {
    let home = home("hello");
    let p = call(&home, "space.ensure", &json!({"name": "default"}))["path"]
        .as_str()
        .unwrap()
        .to_owned();
    let elsewhere = home.root().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("keep"), "x").unwrap();
    let nested = Path::new(&p).join("deep").join("er");
    std::fs::create_dir_all(&nested).unwrap();
    std::os::unix::fs::symlink(&elsewhere, nested.join("out")).unwrap();
    std::os::unix::fs::symlink(elsewhere.join("keep"), Path::new(&p).join("file-link")).unwrap();
    let erased = call(&home, "space.erase", &json!({"name": "default"}));
    assert_valid("space-erase-reply.json", &erased);
    let links = erased["links"].as_array().unwrap();
    assert_eq!(links.len(), 2, "{erased:#}");
    for link in links {
        // Named under the resolved space, as removed[] and space.ensure name it.
        assert!(link.as_str().unwrap().starts_with(&p), "{link} under {p}");
    }
    assert_eq!(erased["removed"][0], p.as_str());
    assert!(!Path::new(&p).exists());
    assert!(elsewhere.join("keep").exists(), "no link was followed");
    assert_eq!(
        std::fs::read_to_string(elsewhere.join("keep")).unwrap(),
        "x"
    );
}

#[cfg(unix)]
#[test]
fn space_erase_refuses_a_link_on_the_way_and_removes_nothing() {
    let home = home("hello");
    let p = call(&home, "space.ensure", &json!({"name": "default"}))["path"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut params = task("hi");
    params["persist_session"] = json!(true);
    let (r, _, _) = gated_start(&home, &params).wait();
    // The space's project folder, swapped for a link out of the config dir.
    let project = project_of(&home, &p);
    let elsewhere = home.root().join("elsewhere-project");
    std::fs::rename(&project, &elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &project).unwrap();
    let run = home.rlxd(&["call", "space.erase", r#"{"name":"default"}"#]);
    assert_eq!(run.error_code(), "link_refused", "{run:?}");
    let paths = run.json()["error"]["detail"]["paths"].to_string();
    assert!(
        paths.contains(project.file_name().unwrap().to_str().unwrap()),
        "{paths}"
    );
    assert!(Path::new(&p).exists(), "nothing was removed");
    assert!(elsewhere.join(format!("{r}.jsonl")).exists());
    assert_eq!(
        call(&home, "run.get", &json!({"run_id": r}))["state"],
        "done"
    );
    // <home>/spaces itself a link: refused too.
    let other = home_with("hello", &[]);
    let target = other.root().join("real-spaces");
    std::fs::create_dir_all(target.join("default")).unwrap();
    let _ = std::fs::remove_dir_all(other.root().join("spaces"));
    std::os::unix::fs::symlink(&target, other.root().join("spaces")).unwrap();
    let run = other.rlxd(&["call", "space.erase", r#"{"name":"default"}"#]);
    assert_eq!(run.error_code(), "link_refused", "{run:?}");
    assert!(target.join("default").exists());
}

#[cfg(windows)]
#[test]
fn space_erase_removes_a_junction_inside_the_space_as_a_link() {
    let home = home("hello");
    let p = call(&home, "space.ensure", &json!({"name": "default"}))["path"]
        .as_str()
        .unwrap()
        .to_owned();
    let elsewhere = home.root().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("keep"), "x").unwrap();
    let junction = Path::new(&p).join("deep").join("j");
    std::fs::create_dir_all(junction.parent().unwrap()).unwrap();
    let made = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(&junction)
        .arg(&elsewhere)
        .output()
        .unwrap();
    assert!(made.status.success(), "{made:?}");
    let erased = call(&home, "space.erase", &json!({"name": "default"}));
    assert_eq!(erased["links"].as_array().unwrap().len(), 1, "{erased:#}");
    assert!(!Path::new(&p).exists());
    assert!(
        elsewhere.join("keep").exists(),
        "the junction was not followed"
    );
}

#[test]
fn seventy_runs_at_once_get_one_seq_per_event_on_replay_and_on_follow() {
    // The tester's repro (#2525): 7 kinds x 2 x 5, partial text on.
    let home = home("hello");
    let since = seq(&home);
    let follower = home.spawn(&[
        "events",
        "--topics",
        "run,daemon",
        "--since",
        &since.to_string(),
        "--follow",
        "--until",
        "daemon.debug",
        "--timeout",
        "120",
    ]);
    let kinds = [
        "task",
        "send_executor",
        "widget_refresh",
        "watch",
        "check",
        "parse_instruction",
        "widget_author",
    ];
    let pinned = json!({"tool": "mcp__claude_ai_Slack__slack_send_message",
        "input_sha256": "cdab067e9f3beb32d1252cfd63e492592fecbf591b0d08cadb24bb17f3864246"});
    let mut starts = Vec::new();
    for kind in kinds {
        for _ in 0..10 {
            let mut params = json!({"kind": kind, "prompt": "hi",
                "mount": {"kind": "no_tools"}, "partial": true});
            if kind == "send_executor" || kind == "watch" {
                params["pinned_call"] = pinned.clone();
            }
            starts.push((run_id(), params));
        }
    }
    thread::scope(|scope| {
        for (id, params) in &starts {
            let home = &home;
            scope.spawn(move || start(home, params, id));
        }
    });
    let ids: Vec<&String> = starts.iter().map(|(id, _)| id).collect();
    let replayed = wait_until(&home, since, 120, "every run ends", |e| {
        ids.iter().all(|id| ended(e, id))
    });
    home.emit(1);
    let followed = finish(follower);
    assert_eq!(followed.code(), Some(0), "the follower: {followed:?}");
    let followed: Vec<Value> = followed
        .lines()
        .into_iter()
        .filter(|e| e["topic"] == "run")
        .collect();
    for (name, events) in [("replay", &replayed), ("follow", &followed)] {
        let seqs: Vec<u64> = events.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
        assert!(
            seqs.windows(2).all(|w| w[0] < w[1]),
            "{name}: seqs repeat or go back"
        );
    }
    assert_eq!(
        followed, replayed,
        "the follower got what the replay holds, once"
    );
    for id in &ids {
        let mine = of(&replayed, id, None);
        let run_seqs: Vec<u64> = mine
            .iter()
            .map(|e| e["data"]["run_seq"].as_u64().unwrap())
            .collect();
        assert_eq!(
            run_seqs,
            (1..=run_seqs.len() as u64).collect::<Vec<_>>(),
            "{id}"
        );
        // The run's log holds exactly the events subscribers got, the same seqs.
        let logged: Vec<Value> = std::fs::read_to_string(log_path(&home, id))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let seen: Vec<Value> = mine.into_iter().cloned().collect();
        assert_eq!(logged, seen, "{id}");
    }
}

/// The run's guard, as its record names it (macOS and Linux).
#[cfg(unix)]
fn guard_of(home: &TestHome, r: &str) -> u64 {
    let path = home
        .root()
        .join("rlxd")
        .join("runs")
        .join(r)
        .join("run.json");
    let record: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    record["process"]["pid"]
        .as_u64()
        .unwrap_or_else(|| panic!("no process: {record:#}"))
}

#[cfg(unix)]
#[test]
fn a_guard_killed_alone_takes_its_run_with_it_and_a_cancel_still_answers() {
    let home = home("slow");
    let since = seq(&home);
    let r = run_id();
    start(&home, &task("take your time"), &r);
    wait_until(&home, since, 20, "the run starts", |e| {
        !of(e, &r, Some("run.started")).is_empty()
    });
    common::kill_hard(guard_of(&home, &r));
    common::eventually("nothing of the run is left", || {
        processes_matching(&pattern(&home, "slow")) == 0
    });
    let events = wait_until(&home, since, 10, "the run ends", |e| ended(e, &r));
    let last = of(&events, &r, None).last().copied().unwrap().clone();
    assert_eq!(last["type"], "run.failed", "{events:#?}");
    assert!(
        last["data"]["message"].as_str().unwrap().contains("guard"),
        "{last:#}"
    );
    let began = Instant::now();
    let cancelled = home.rlxd(&["call", "run.cancel", &json!({"run_id": r}).to_string()]);
    assert_eq!(cancelled.code(), Some(0), "{cancelled:?}");
    assert!(
        began.elapsed() < Duration::from_secs(2),
        "{:?}",
        began.elapsed()
    );
    // Its place in the pool was given back.
    let next = run_id();
    start(&home, &task("quick"), &next);
    wait_until(&home, since, 20, "the next run ends", |e| ended(e, &next));
}

#[cfg(unix)]
#[test]
fn a_run_with_a_timeout_whose_guard_dies_leaves_nothing_running() {
    let home = home("slow");
    let since = seq(&home);
    let r = run_id();
    let mut params = task("take your time");
    params["timeout_s"] = json!(3);
    start(&home, &params, &r);
    wait_until(&home, since, 20, "the run starts", |e| {
        !of(e, &r, Some("run.started")).is_empty()
    });
    common::kill_hard(guard_of(&home, &r));
    wait_until(&home, since, 10, "the run ends", |e| ended(e, &r));
    common::eventually("nothing of the run is left", || {
        processes_matching(&pattern(&home, "slow")) == 0
    });
}

#[test]
fn rlxd_killed_alone_leaves_no_run_running() {
    let home = home("slow");
    let since = seq(&home);
    let r = run_id();
    start(&home, &task("take your time"), &r);
    wait_until(&home, since, 20, "the run starts", |e| {
        !of(e, &r, Some("run.started")).is_empty()
    });
    common::kill_hard(home.status()["pid"].as_u64().unwrap());
    common::eventually("nothing of the run is left", || {
        processes_matching(&pattern(&home, "slow")) == 0
    });
}

#[test]
fn a_structured_output_too_large_for_run_done_still_ends_the_run() {
    let home = home("huge");
    let mut params = task("go");
    params["output_schema"] = json!({"type": "object"});
    let (r, _, events) = gated_start(&home, &params).wait();
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

#[cfg(unix)]
#[test]
fn a_config_dir_that_is_a_link_is_taken_as_it_resolves() {
    // A dotfiles setup: ~/.claude is a link. It is a root, so it is followed, and only
    // links below it are refused.
    let home = home("persist");
    let p = call(&home, "space.ensure", &json!({"name": "default"}))["path"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut params = task("hi");
    params["persist_session"] = json!(true);
    let (r, _, _) = gated_start(&home, &params).wait();
    let dotfiles = home.root().join("dotfiles-claude");
    let folder: String = p
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let planted = dotfiles.join("projects").join(&folder);
    std::fs::create_dir_all(&planted).unwrap();
    std::fs::write(planted.join(format!("{r}.jsonl")), "{}\n").unwrap();
    std::os::unix::fs::symlink(&dotfiles, home.root().join("user-home").join(".claude")).unwrap();
    let erased = call(&home, "space.erase", &json!({"name": "default"}));
    assert!(!planted.join(format!("{r}.jsonl")).exists(), "{erased:#}");
    // An empty space erases too.
    call(&home, "space.ensure", &json!({"name": "empty"}));
    call(&home, "space.erase", &json!({"name": "empty"}));
}

#[test]
fn run_events_replays_a_log_past_four_mebibytes_from_any_run_seq() {
    let home = home("big");
    let (r, _, _) = gated_start(&home, &task("go")).wait();
    let n = call(&home, "run.get", &json!({"run_id": r}))["last_run_seq"]
        .as_u64()
        .unwrap();
    let size = std::fs::metadata(log_path(&home, &r)).unwrap().len();
    assert!(size > 4 << 20, "the log is {size} bytes");
    // From early, and from past the log's first 4 MiB (run_seq 6 to 8 of 8).
    assert_eq!(n, 8, "the started event, six answers and the end");
    for from in [2, 6, 7, 8] {
        let replayed = home.rlxd(&[
            "call",
            "run.events",
            &json!({"run_id": r, "from_run_seq": from}).to_string(),
        ]);
        assert_eq!(replayed.code(), Some(0), "from {from}: {replayed:?}");
        let run_seqs: Vec<u64> = replayed
            .lines()
            .iter()
            .map(|e| e["data"]["run_seq"].as_u64().unwrap())
            .collect();
        assert_eq!(run_seqs, (from..=n).collect::<Vec<_>>(), "from {from}");
    }
}

#[test]
fn run_events_follows_a_live_run_with_a_long_log_from_a_late_run_seq() {
    let home = home_with("big", &[("FAKE_CLAUDE_SLOW_MS", "2000")]);
    let since = seq(&home);
    let r = run_id();
    start(&home, &task("go"), &r);
    let events = wait_until(&home, since, 30, "six answers", |e| {
        of(e, &r, Some("run.text")).len() >= 6
    });
    let late = of(&events, &r, None).last().unwrap()["data"]["run_seq"]
        .as_u64()
        .unwrap();
    let followed = home.rlxd(&[
        "call",
        "run.events",
        &json!({"run_id": r, "from_run_seq": late}).to_string(),
    ]);
    assert_eq!(followed.code(), Some(0), "{followed:?}");
    let events = followed.lines();
    assert_eq!(events[0]["data"]["run_seq"], late);
    assert_eq!(events.last().unwrap()["type"], "run.done", "{events:#?}");
}

#[cfg(unix)]
#[test]
fn rlxd_and_its_guards_killed_together_leave_nothing_after_a_restart() {
    // `killall -9 rlxd` kills the guards too, which are rlxds.
    let home = home("slow");
    let since = seq(&home);
    let r = run_id();
    start(&home, &task("take your time"), &r);
    wait_until(&home, since, 20, "the run starts", |e| {
        !of(e, &r, Some("run.started")).is_empty()
    });
    let rlxd = home.status()["pid"].as_u64().unwrap();
    let guard = guard_of(&home, &r);
    let killed = std::process::Command::new("kill")
        .args(["-9", &rlxd.to_string(), &guard.to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    common::eventually("rlxd is gone", || {
        home.rlxd(&["call", "status"]).code() == Some(3)
    });
    home.start_dev();
    common::eventually("nothing of the run is left", || {
        processes_matching(&pattern(&home, "slow")) == 0
    });
    let got = call(&home, "run.get", &json!({"run_id": r}));
    assert_eq!(
        got["last_event"]["data"]["reason"], "engine_restarted",
        "{got:#}"
    );
}
