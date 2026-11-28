//! What the checks that run fake-claude share (handoff 0.10.3, "How to read the checks"):
//! a home whose rlxd runs a scenario, a run id, the handoff's `gated_start`, and the
//! fixture log. `runner.rs` keeps its own copies until the scheduler's branch, which edits
//! it, has merged; then it moves onto these.

use std::path::{Path, PathBuf};
use std::process::Child;

use serde_json::Value;

use super::{TestHome, eventually, finish};

/// fake-claude, linked (or copied) to `<home>/bin/fake-claude/<scenario>`, `.exe` on
/// Windows: the program `RLXD_CLAUDE_BIN` names, so its command line names its scenario.
pub fn link(root: &Path, scenario: &str) -> PathBuf {
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
/// with the test client paired as the app. `HOME`, `USERPROFILE` and `CLAUDE_CONFIG_DIR`
/// are folders of the home's own, so nothing reads or writes a real `~/.claude`.
pub fn home_with(scenario: &str, envs: &[(&str, &str)]) -> TestHome {
    home_in(TestHome::new(), scenario, envs)
}

/// [`home_with`] `first`, whose fake-claude plays the scenario `<home>/scenario` names at
/// each start (`FAKE_CLAUDE_SCENARIO_FILE`): write another name there to switch.
pub fn home_with_scenario_file(first: &str) -> TestHome {
    home_with_scenario_file_and(first, &[])
}

/// [`home_with_scenario_file`], with `envs` set too.
pub fn home_with_scenario_file_and(first: &str, envs: &[(&str, &str)]) -> TestHome {
    home_with_scenario_file_in(TestHome::new(), first, envs)
}

/// [`home_with_scenario_file_and`], in `home`, which may hold files its rlxd reads at
/// its start.
pub fn home_with_scenario_file_in(home: TestHome, first: &str, envs: &[(&str, &str)]) -> TestHome {
    let file = home.root().join("scenario");
    std::fs::write(&file, first).unwrap();
    let file = file.to_str().unwrap().to_owned();
    let mut all = vec![("FAKE_CLAUDE_SCENARIO_FILE", file.as_str())];
    all.extend_from_slice(envs);
    home_in(home, first, &all)
}

/// `run.resume {run_id: id, reply}` as the app, captured as [`gated_start`] captures a
/// start: the events from the resume to the run's next end.
pub fn resumed(home: &TestHome, id: &str, reply: &str, seconds: u64) -> Vec<Value> {
    let since = seq(home);
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
        &seconds.to_string(),
    ]);
    eventually("the capture holds the app role", || home.app_clients() >= 1);
    let params = serde_json::json!({"run_id": id, "reply": reply});
    let run = home.rlxd(&["call", "--as-app", "run.resume", &params.to_string()]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let run = finish(capture);
    assert_eq!(run.code(), Some(0), "the capture: {run:?}");
    run.lines()
}

/// Run `id`'s session file, where Claude Code keeps it under the home's
/// `CLAUDE_CONFIG_DIR`: `None` when there is none.
pub fn session_file(home: &TestHome, id: &str) -> Option<PathBuf> {
    let projects = home.root().join("claude-config").join("projects");
    let found: Vec<PathBuf> = std::fs::read_dir(projects)
        .into_iter()
        .flatten()
        .filter_map(|entry| Some(entry.ok()?.path().join(format!("{id}.jsonl"))))
        .filter(|path| path.is_file())
        .collect();
    assert!(found.len() <= 1, "{found:?}");
    found.into_iter().next()
}

/// Plays `scenario` in `home`, as [`home_with`] says.
fn home_in(home: TestHome, scenario: &str, envs: &[(&str, &str)]) -> TestHome {
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
    home
}

/// A new run id, as `uuidgen | tr A-Z a-z` makes one.
pub fn run_id() -> String {
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
pub fn seq(home: &TestHome) -> u64 {
    home.status()["seq"].as_u64().unwrap()
}

/// `rlxd call run.start params` with `run_id` `id`, which must succeed.
pub fn start(home: &TestHome, params: &Value, id: &str) {
    let mut params = params.clone();
    params["run_id"] = Value::String(id.to_owned());
    let run = home.rlxd(&["call", "run.start", &params.to_string()]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_eq!(run.data()["run_id"], id, "{run:?}");
}

/// The handoff's `gated_start`: an app-role capture of the run and needs_you topics from
/// the current seq, until the first run.done, run.failed or run.needs_you, then the run.
pub struct Gated {
    pub id: String,
    pub since: u64,
    capture: Child,
}

/// `T=<seconds> gated_start params`.
pub fn gated_start(home: &TestHome, params: &Value, seconds: u64) -> Gated {
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
        &seconds.to_string(),
    ]);
    eventually("the capture holds the app role", || home.app_clients() >= 1);
    start(home, params, &id);
    Gated { id, since, capture }
}

impl Gated {
    /// `wait "$cap"`: the run's id and the capture's events, `ev.jsonl`.
    pub fn wait(self) -> (String, Vec<Value>) {
        let run = finish(self.capture);
        assert_eq!(run.code(), Some(0), "the capture: {run:?}");
        (self.id, run.lines())
    }
}

/// `$AISQUARE_HOME/fake-claude.log.json`.
pub fn fixture_log(home: &TestHome) -> Value {
    let path = home.root().join("fake-claude.log.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap()
}
