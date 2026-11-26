//! `status.claude` (handoff 0.10.3, 7.4): probed in the background with the app's
//! environment, so `status` never waits on Claude Code for long, and probed again when the
//! app's environment changes. Stand-ins are shell scripts here, on macOS and Linux; the
//! fake-claude scenarios cover every OS with the runner.
#![cfg(unix)]

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::TestHome;
use serde_json::Value;

/// A stand-in for Claude Code in `home`: a shell script of `body`.
fn stand_in(home: &TestHome, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let program = home.root().join("claude-stand-in");
    std::fs::write(&program, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    program
}

/// `status.claude`, asked until `done` holds, for up to 15 s.
fn claude_until(home: &TestHome, what: &str, done: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let claude = home.status()["claude"].clone();
        if done(&claude) {
            return claude;
        }
        assert!(Instant::now() < deadline, "never {what}: {claude}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_hanging_probe_leaves_status_answering_within_a_second() {
    let home = TestHome::new();
    let program = stand_in(&home, "exec sleep 30");
    let home = home.with_claude(program);
    home.start();
    for _ in 0..3 {
        let asked = Instant::now();
        let claude = home.status()["claude"].clone();
        let took = asked.elapsed();
        assert!(took < Duration::from_secs(1), "status took {took:?}");
        assert_eq!(claude["installed"], true, "{claude}");
        assert_eq!(claude["signed_in"], false, "{claude}");
    }
    // The probe gives up after its time, and says why.
    let claude = claude_until(&home, "a probe that ended", |c| c.get("as_of").is_some());
    assert_eq!(claude["signed_in"], false, "{claude}");
    let error = claude["error"].as_str().unwrap_or_default();
    assert!(error.contains("did not answer within"), "{claude}");
}

#[test]
fn an_env_change_probes_again_with_the_apps_environment() {
    // Signed in as an account named after CLAUDE_CONFIG_DIR, on a plan named after PATH:
    // builtins only, as the app's PATH may hold nothing.
    let home = TestHome::new();
    let program = stand_in(
        &home,
        r#"case "$1" in
  --version) echo "2.1.332 (Claude Code)";;
  auth) printf '{"loggedIn":true,"authMethod":"claude.ai","email":"probe+%s@example.com","subscriptionType":"pro:%s"}' "$CLAUDE_CONFIG_DIR" "$PATH";;
esac"#,
    );
    let home = home.with_claude(program);
    home.start_dev();
    claude_until(&home, "probed", |c| c.get("as_of").is_some());

    let run = home.rlxd(&[
        "call",
        "--as-app",
        "env.set",
        r#"{"CLAUDE_CONFIG_DIR":"cc-a"}"#,
    ]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let claude = claude_until(&home, "the new CLAUDE_CONFIG_DIR", |c| {
        c["account"] == "probe+cc-a@example.com"
    });
    assert_eq!(claude["signed_in"], true, "{claude}");
    assert_eq!(claude["version"], "2.1.332");

    let run = home.rlxd(&[
        "call",
        "--as-app",
        "--env",
        r#"{"PATH":"/no-such-folder"}"#,
        "status",
    ]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let claude = claude_until(&home, "the new PATH", |c| {
        c["plan"] == "pro:/no-such-folder"
    });
    assert_eq!(claude["account"], "probe+cc-a@example.com", "{claude}");
}
