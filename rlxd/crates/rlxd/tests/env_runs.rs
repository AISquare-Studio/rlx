//! 4.3's Accept lines that start a run (handoff 0.10.3): the app's environment reaches
//! Claude Code raw, and every run starts the absolute `claude` rlxd resolved when the app
//! set `PATH`, never one looked up when the run starts. `run.start` refuses
//! `claude_not_found` when the app's `PATH` has none. fake-claude's `print-env` stands
//! in, on every OS: linked as `bin/claude` on macOS and Linux, as the Accept does, and
//! copied as `bin\claude.exe` on Windows, with `FAKE_CLAUDE_SCENARIO` naming the scenario.
//!
//! Runs pass `persist_session: false`, and rlxd's own `CLAUDE_CONFIG_DIR` is in the home,
//! so nothing touches the real `~/.claude`.

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use common::TestHome;
use serde_json::{Value, json};

/// fake-claude, placed at `path`: linked on macOS and Linux, copied on Windows.
fn place(path: &Path) -> PathBuf {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let binary = Path::new(env!("CARGO_BIN_EXE_fake-claude"));
    if std::fs::hard_link(binary, path).is_err() {
        std::fs::copy(binary, path).unwrap();
    }
    path.to_path_buf()
}

/// `name` in `dir` as a link to `target` on macOS and Linux, a copy of fake-claude on
/// Windows (`.exe`).
fn claude_in(dir: &Path, target: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    #[cfg(unix)]
    {
        let link = dir.join("claude");
        std::os::unix::fs::symlink(target, &link).unwrap();
        link
    }
    #[cfg(windows)]
    {
        let _ = target;
        place(&dir.join("claude.exe"))
    }
}

/// A new run id: a lower-case UUID.
fn run_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let hex = format!("{:032x}", nanos ^ (u128::from(n) << 64));
    format!(
        "{}-{}-4{}-8{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    )
}

fn task(id: &str) -> String {
    json!({"run_id": id, "kind": "task", "prompt": "x", "mount": {"kind": "no_tools"},
        "space": "default", "persist_session": false})
    .to_string()
}

/// Starts a task run, and returns its start's entry in the fixture log once it exited.
fn run_to_its_end(home: &TestHome) -> (String, Value) {
    let id = run_id();
    let run = home.rlxd(&["call", "run.start", &task(&id)]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let log = home.root().join("fake-claude.log.json");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let start = std::fs::read_to_string(&log)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|log| {
                log["starts"]
                    .as_array()?
                    .iter()
                    .rev()
                    .find(|s| {
                        s["argv"]
                            .as_array()
                            .is_some_and(|argv| argv.iter().any(|a| a == &id[..]))
                            && !s["exit"].is_null()
                    })
                    .cloned()
            });
        if let Some(start) = start {
            return (id, start);
        }
        assert!(Instant::now() < deadline, "the run never ended: {log:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A home whose rlxd runs with `RLXD_CLAUDE_BIN` unset, so `claude` is resolved on the
/// app's `PATH`, in gate dev mode, with `CLAUDE_CONFIG_DIR` in the home.
fn home_without_claude_bin() -> TestHome {
    let home = TestHome::new();
    let config = home.root().join("claude-config");
    std::fs::create_dir_all(&config).unwrap();
    let mut home = home.with_env("CLAUDE_CONFIG_DIR", config.to_str().unwrap());
    if cfg!(windows) {
        home = home.with_env("FAKE_CLAUDE_SCENARIO", "print-env");
    }
    let mut serve = home.command(&["serve", "--detach", "--gate-dev-mode"]);
    serve.env_remove("RLXD_CLAUDE_BIN").stdin(Stdio::null());
    let run = common::run(serve, None);
    assert_eq!(run.code(), Some(0), "{run:?}");
    home
}

#[test]
fn the_apps_claude_config_dir_reaches_claude_code_raw() {
    // rlxd call --as-app env.set '{"CLAUDE_CONFIG_DIR":"cc-test"}', then a run with
    // fake-claude/print-env: .starts[-1].env.CLAUDE_CONFIG_DIR == "cc-test".
    let home = TestHome::new();
    let program = place(
        &home
            .root()
            .join("bin")
            .join("fake-claude")
            .join(if cfg!(windows) {
                "print-env.exe"
            } else {
                "print-env"
            }),
    );
    let home = home.with_claude(program);
    home.start_dev();
    let run = home.rlxd(&[
        "call",
        "--as-app",
        "env.set",
        r#"{"CLAUDE_CONFIG_DIR":"cc-test"}"#,
    ]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let (_, start) = run_to_its_end(&home);
    assert_eq!(start["env"]["CLAUDE_CONFIG_DIR"], "cc-test", "{start}");
    assert_eq!(start["exit"], 0, "{start}");
}

#[test]
fn a_run_starts_the_claude_resolved_when_the_app_set_path() {
    // The Accept's "Absolute paths": only bin has claude when the app sets PATH; bin1's,
    // added after, comes first on PATH, and a lookup at the run's start would find it.
    let home = home_without_claude_bin();
    let root = home.root().to_path_buf();
    let f = place(
        &root.join("bin").join("fake-claude").join(if cfg!(windows) {
            "print-env.exe"
        } else {
            "print-env"
        }),
    );
    let (bin, bin1) = (root.join("bin"), root.join("bin1"));
    let resolved = claude_in(&bin, &f);
    std::fs::create_dir_all(&bin1).unwrap();
    let mut dirs = vec![bin1.clone(), bin.clone()];
    dirs.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(dirs).unwrap();
    let set = json!({"PATH": path.to_str().unwrap()}).to_string();
    let run = home.rlxd(&["call", "--as-app", "env.set", &set]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    claude_in(&bin1, &f);

    let (id, start) = run_to_its_end(&home);
    assert_eq!(
        start["argv"][0],
        resolved.display().to_string(),
        "bin's claude, as resolved when the app set PATH: {start}"
    );
    assert_eq!(
        start["env"]["PATH"],
        path.to_str().unwrap(),
        "the app's PATH, raw"
    );

    // The hook command: rlxd-hook by its absolute path (5.1).
    let settings = root
        .join("rlxd")
        .join("runs")
        .join(&id)
        .join("1")
        .join("settings.json");
    let settings: Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    let command = settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .to_owned();
    let program = command
        .split(" pre-tool-use ")
        .next()
        .unwrap()
        .trim_matches('"');
    assert!(
        command.contains(" pre-tool-use ")
            && Path::new(program).is_absolute()
            && (program.ends_with("/rlxd-hook") || program.ends_with("/rlxd-hook.exe")),
        "{command}"
    );
}

#[test]
fn run_start_refuses_when_the_apps_path_has_no_claude() {
    let home = home_without_claude_bin();
    let empty = home.root().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let set = json!({"PATH": empty.to_str().unwrap()}).to_string();
    let run = home.rlxd(&["call", "--as-app", "env.set", &set]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let run = home.rlxd(&["call", "run.start", &task(&run_id())]);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_eq!(run.error_code(), "claude_not_found");
    let message = run.json()["error"]["message"].as_str().unwrap().to_owned();
    assert!(message.contains("on the app's PATH"), "{message}");

    // Windows: npm's claude.cmd shim, found first, is not run.
    #[cfg(windows)]
    {
        let shims = home.root().join("shims");
        std::fs::create_dir_all(&shims).unwrap();
        std::fs::write(shims.join("claude.cmd"), "@echo off\r\n").unwrap();
        let set = json!({"PATH": shims.to_str().unwrap()}).to_string();
        let run = home.rlxd(&["call", "--as-app", "env.set", &set]);
        assert_eq!(run.code(), Some(0), "{run:?}");
        let run = home.rlxd(&["call", "run.start", &task(&run_id())]);
        assert_eq!(run.error_code(), "claude_not_runnable", "{run:?}");
    }
}
