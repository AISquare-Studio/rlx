//! `status` (handoff 0.10.3, 7.4): its three Accept lines, the background probe of
//! Claude Code, and `runs` with runs in flight, on every OS. fake-claude stands in for
//! Claude Code: the probe scenarios answer `claude --version` and `claude auth status
//! --json` as Claude Code does, with `probe@example.com`.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::TestHome;
use serde_json::{Value, json};

/// fake-claude as `<home>/bin/fake-claude/<scenario>` (`.exe` on Windows): the path
/// `RLXD_CLAUDE_BIN` names, without the extension, as Windows finds it.
fn fake(root: &Path, scenario: &str) -> PathBuf {
    let dir = root.join("bin").join("fake-claude");
    std::fs::create_dir_all(&dir).unwrap();
    let name = if cfg!(windows) {
        format!("{scenario}.exe")
    } else {
        scenario.to_owned()
    };
    let binary = Path::new(env!("CARGO_BIN_EXE_fake-claude"));
    if std::fs::hard_link(binary, dir.join(&name)).is_err() {
        std::fs::copy(binary, dir.join(&name)).unwrap();
    }
    dir.join(scenario)
}

/// A home whose rlxd runs fake-claude `scenario` as Claude Code, with
/// `CLAUDE_CONFIG_DIR` inside it, started in gate dev mode.
fn home_with(scenario: &str) -> TestHome {
    let home = TestHome::new();
    let claude = fake(home.root(), scenario);
    let config = home.root().join("claude-config");
    std::fs::create_dir_all(&config).unwrap();
    let home = home
        .with_claude(claude)
        .with_env("CLAUDE_CONFIG_DIR", config.to_str().unwrap());
    home.start_dev();
    home
}

/// `status.claude`, asked until `done` holds, for up to 20 s.
fn claude_until(home: &TestHome, what: &str, done: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let claude = home.status()["claude"].clone();
        if done(&claude) {
            return claude;
        }
        assert!(Instant::now() < deadline, "never {what}: {claude}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn probed(claude: &Value) -> bool {
    claude.get("as_of").is_some()
}

#[test]
fn accept_installed_and_signed_in_are_booleans() {
    // rlxd call status | jq -e '.data.claude | (.installed | type == "boolean") and
    // (.signed_in | type == "boolean")'
    for home in [TestHome::new(), home_with("auth-status")] {
        if home.root().join("bin").exists() {
            claude_until(&home, "probed", probed);
        } else {
            home.start();
        }
        let claude = home.status()["claude"].clone();
        assert!(claude["installed"].is_boolean(), "{claude}");
        assert!(claude["signed_in"].is_boolean(), "{claude}");
    }
}

#[test]
fn accept_rlxd_claude_bin_at_nothing_is_not_installed() {
    // With RLXD_CLAUDE_BIN pointing at nothing: .data.claude.installed == false. The
    // test home's default points it at <home>/no-claude.
    let home = TestHome::new();
    home.start();
    let claude = claude_until(&home, "probed", probed);
    assert_eq!(claude["installed"], false, "{claude}");
    assert_eq!(claude["signed_in"], false, "{claude}");
    for absent in ["version", "path", "account", "plan", "auth_method", "error"] {
        assert!(claude.get(absent).is_none(), "{absent}: {claude}");
    }
    assert_eq!(claude["path_source"], "rlxd_claude_bin", "{claude}");
}

#[test]
fn accept_auth_status_is_signed_in_as_probe_on_pro() {
    // With fake-claude/auth-status: .data.claude.signed_in == true and .account ==
    // "probe@example.com" and .plan == "pro".
    let home = home_with("auth-status");
    let claude = claude_until(&home, "probed", probed);
    assert_eq!(claude["signed_in"], true, "{claude}");
    assert_eq!(claude["account"], "probe@example.com", "{claude}");
    assert_eq!(claude["plan"], "pro", "{claude}");
    assert_eq!(claude["auth_method"], "claude.ai", "{claude}");
    assert_eq!(claude["version"], "2.1.314", "{claude}");
    assert_eq!(claude["installed"], true, "{claude}");
    for kept_out in ["orgId", "orgName", "org", "error"] {
        assert!(claude.get(kept_out).is_none(), "{kept_out}: {claude}");
    }
}

#[test]
fn a_signed_out_claude_code_is_signed_out_with_no_error() {
    let home = home_with("signed-out");
    let claude = claude_until(&home, "probed", probed);
    assert_eq!(claude["installed"], true, "{claude}");
    assert_eq!(claude["signed_in"], false, "{claude}");
    for absent in ["account", "plan", "auth_method", "error"] {
        assert!(claude.get(absent).is_none(), "{absent}: {claude}");
    }
}

#[test]
fn a_hanging_probe_leaves_status_answering_within_a_second() {
    let home = home_with("probe-hangs");
    for _ in 0..3 {
        let asked = Instant::now();
        let claude = home.status()["claude"].clone();
        let took = asked.elapsed();
        assert!(
            took < Duration::from_secs(1),
            "status took {took:?}: {claude}"
        );
        assert_eq!(claude["signed_in"], false, "{claude}");
    }
    let claude = claude_until(&home, "a probe that ended", probed);
    let error = claude["error"].as_str().unwrap_or_default();
    assert!(error.contains("did not answer within"), "{claude}");
}

#[test]
fn an_env_set_probes_again_with_the_new_values() {
    let home = home_with("auth-env");
    claude_until(&home, "probed", probed);
    let run = home.rlxd(&[
        "call",
        "--as-app",
        "env.set",
        r#"{"CLAUDE_CONFIG_DIR":"cc-b"}"#,
    ]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let claude = claude_until(&home, "the new account", |c| {
        c["account"] == "probe+cc-b@example.com"
    });
    assert_eq!(claude["signed_in"], true, "{claude}");
}

#[test]
fn runs_counts_the_live_and_the_queued() {
    // Tasks run two at a time: three slow ones are two live and one queued.
    let home = home_with("slow");
    let ids: Vec<String> = (1..=3)
        .map(|n| format!("00000000-0000-4000-8000-{n:012}"))
        .collect();
    for id in &ids {
        let params = json!({"run_id": id, "kind": "task", "prompt": "x",
            "mount": {"kind": "no_tools"}, "space": "default", "persist_session": false});
        let run = home.rlxd(&["call", "run.start", &params.to_string()]);
        assert_eq!(run.code(), Some(0), "{run:?}");
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let runs = home.status()["runs"].clone();
        if runs == json!({"live": 2, "queued": 1}) {
            break;
        }
        assert!(Instant::now() < deadline, "{runs}");
        std::thread::sleep(Duration::from_millis(100));
    }
    for id in &ids {
        let params = json!({"run_id": id}).to_string();
        let run = home.rlxd(&["call", "run.cancel", &params]);
        assert_eq!(run.code(), Some(0), "{run:?}");
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let runs = home.status()["runs"].clone();
        if runs == json!({"live": 0, "queued": 0}) {
            break;
        }
        assert!(Instant::now() < deadline, "{runs}");
        std::thread::sleep(Duration::from_millis(100));
    }
}
