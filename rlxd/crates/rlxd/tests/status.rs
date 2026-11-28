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

/// A home whose rlxd runs fake-claude `scenario` as Claude Code, with `HOME`,
/// `USERPROFILE` and `CLAUDE_CONFIG_DIR` inside it, started in gate dev mode.
fn home_with(scenario: &str) -> TestHome {
    let home = TestHome::new().with_own_user();
    let claude = fake(home.root(), scenario);
    let home = home.with_claude(claude);
    home.start_dev();
    home
}

/// `env.set change` from the app, which must succeed.
fn app_sets(home: &TestHome, change: &str) {
    let run = home.rlxd(&["call", "--as-app", "env.set", change]);
    assert_eq!(run.code(), Some(0), "{run:?}");
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

#[test]
fn an_app_saying_hello_again_with_the_same_values_keeps_the_answer_and_its_as_of() {
    // Review #3345's major: every app hello carries the app's env. One with the values
    // rlxd has, as on each reconnect, must not drop a good answer; probe-slow takes longer
    // than status waits, so a dropped one would read signed out, with no as_of.
    let home = home_with("probe-slow");
    let env = json!({"CLAUDE_CONFIG_DIR": "cc-same"}).to_string();
    app_sets(&home, &env);
    let first = claude_until(&home, "probed", probed);
    assert_eq!(first["signed_in"], true, "{first}");
    assert_eq!(first["account"], "probe@example.com", "{first}");

    // The app says hello again with the same env, and asks status on that connection.
    let run = home.rlxd(&["call", "--as-app", "--env", &env, "status"]);
    let claude = run.data()["claude"].clone();
    assert_eq!(claude["signed_in"], true, "{claude}");
    assert_eq!(claude["as_of"], first["as_of"], "the same answer: {claude}");
    // And sets them again.
    app_sets(&home, &env);
    let claude = home.status()["claude"].clone();
    assert_eq!(claude["signed_in"], true, "{claude}");
    assert_eq!(claude["as_of"], first["as_of"], "the same answer: {claude}");

    // Other values do drop it: still checking (no as_of), then the new answer.
    app_sets(&home, r#"{"CLAUDE_CONFIG_DIR":"cc-other"}"#);
    let claude = home.status()["claude"].clone();
    assert!(claude.get("as_of").is_none(), "{claude}");
    let claude = claude_until(&home, "probed again", probed);
    assert_ne!(claude["as_of"], first["as_of"], "{claude}");
    assert_eq!(claude["signed_in"], true, "{claude}");
}

#[test]
fn status_right_after_a_change_that_starts_a_hanging_probe_answers_within_a_second() {
    // Test #3351 (2): the probe a change starts hangs, and status, asked at once, waits
    // for it at most 500 ms. rlxd's own start does not count: serve --detach waits it out.
    let home = home_with("probe-hangs");
    app_sets(&home, r#"{"CLAUDE_CONFIG_DIR":"cc-a"}"#);
    let asked = Instant::now();
    let claude = home.status()["claude"].clone();
    let took = asked.elapsed();
    assert!(
        took < Duration::from_secs(1),
        "status took {took:?}: {claude}"
    );
    assert!(claude.get("as_of").is_none(), "still checking: {claude}");
    assert_eq!(claude["signed_in"], false, "{claude}");
    // A hello whose env changes the values, then status on its connection.
    let asked = Instant::now();
    let run = home.rlxd(&[
        "call",
        "--as-app",
        "--env",
        r#"{"CLAUDE_CONFIG_DIR":"cc-b"}"#,
        "status",
    ]);
    let took = asked.elapsed();
    let claude = run.data()["claude"].clone();
    assert!(
        took < Duration::from_secs(1),
        "hello and status took {took:?}: {claude}"
    );
    assert!(claude.get("as_of").is_none(), "still checking: {claude}");
}

#[test]
fn nothing_rlxd_writes_holds_what_the_probe_printed() {
    // Manager decision #2553, test #3351 (4): the probe's output is never logged.
    // fake-claude/probe-markers prints its marker in every field of auth status, in
    // --version's line, and on both commands' stderr.
    const MARKER: &str = "probe-output-marker-5d1f";
    let home = home_with("probe-markers");
    let claude = claude_until(&home, "probed", probed);
    // Its marker is the one this test looks for: the account status shows carries it.
    assert_eq!(
        claude["account"],
        format!("probe+{MARKER}@example.com"),
        "{claude}"
    );
    assert_eq!(claude["version"], "2.1.314", "{claude}");
    app_sets(&home, r#"{"CLAUDE_CONFIG_DIR":"cc-again"}"#);
    claude_until(&home, "probed again", probed);
    home.stop();
    // Everything under the home, but the fake-claude it runs (bin/), which holds the
    // marker as its own code.
    let mut folders = vec![home.root().to_path_buf()];
    let mut read = 0;
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(&folder).unwrap().flatten() {
            let path = entry.path();
            if path == home.root().join("bin") {
                continue;
            }
            if path.is_dir() {
                folders.push(path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                read += 1;
                let text = String::from_utf8_lossy(&bytes);
                assert!(
                    !text.contains(MARKER),
                    "{} holds the probe's output",
                    path.display()
                );
            }
        }
    }
    assert!(
        read > 0 && home.log().contains("listening"),
        "the log was read"
    );
}
