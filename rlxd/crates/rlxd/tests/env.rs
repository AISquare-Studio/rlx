//! The app's environment for Claude Code (handoff 0.10.3, 4.3): `env.set`, `env.get` and
//! `hello`'s `env`, kept in `<home>/rlxd/env.json`, and `claude` resolved on the app's
//! `PATH`. Each test is a line of 4.3's Accept, but for those that start a run, which come
//! with the runner. The test client, `rlxd call --as-app`, is the gate only in gate dev
//! mode.

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;

use common::{TestHome, assert_valid};
use serde_json::Value;

/// `rlxd call env.get`'s data, which must be `ok`.
fn env_get(home: &TestHome) -> Value {
    let run = home.rlxd(&["call", "env.get"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_valid("env.json", &run.data());
    run.data()
}

/// `<home>/rlxd/env.json`'s bytes, or none.
fn env_file(home: &TestHome) -> Option<Vec<u8>> {
    std::fs::read(home.root().join("rlxd").join("env.json")).ok()
}

/// `rlxd call --as-app env.set <params>`, which must succeed: its data.
fn app_sets(home: &TestHome, params: &str) -> Value {
    let run = home.rlxd(&["call", "--as-app", "env.set", params]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_valid("env.json", &run.data());
    run.data()
}

#[test]
fn the_app_sets_a_value_and_an_rlxd_started_again_has_it() {
    let home = TestHome::new();
    home.start_dev();
    let data = app_sets(&home, r#"{"CLAUDE_CONFIG_DIR":"cc-test"}"#);
    assert_eq!(data, serde_json::json!({"CLAUDE_CONFIG_DIR": "cc-test"}));
    assert_eq!(env_get(&home)["CLAUDE_CONFIG_DIR"], "cc-test");
    // rlxd stop && rlxd serve --detach && rlxd call env.get
    home.stop();
    home.start();
    assert_eq!(
        env_get(&home)["CLAUDE_CONFIG_DIR"],
        "cc-test",
        "kept as sent"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let file = home.root().join("rlxd").join("env.json");
        let mode = std::fs::metadata(file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[test]
fn the_apps_hello_sets_them_too_and_an_empty_value_removes_one() {
    let home = TestHome::new();
    home.start_dev();
    let run = home.rlxd(&[
        "call",
        "--as-app",
        "--env",
        r#"{"CLAUDE_CONFIG_DIR":"cc-hello"}"#,
        "status",
    ]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_eq!(env_get(&home)["CLAUDE_CONFIG_DIR"], "cc-hello");
    app_sets(&home, r#"{"PATH":"/usr/bin"}"#);
    let data = app_sets(&home, r#"{"CLAUDE_CONFIG_DIR":""}"#);
    assert_eq!(
        data,
        serde_json::json!({"PATH": "/usr/bin"}),
        "kept, and removed"
    );
}

#[test]
fn any_other_key_is_bad_env_key_and_changes_nothing() {
    let home = TestHome::new();
    home.start_dev();
    app_sets(&home, r#"{"CLAUDE_CONFIG_DIR":"kept"}"#);
    let (before, file) = (env_get(&home), env_file(&home));
    for params in [
        r#"{"HOME":"x"}"#,
        r#"{"PATH":"/tmp","HOME":"x"}"#,
        r#"{"CLAUDE_CONFIG_DIR":"changed","bogus":"1"}"#,
    ] {
        let run = home.rlxd(&["call", "--as-app", "env.set", params]);
        assert_eq!(run.code(), Some(1), "{params}: {run:?}");
        assert_eq!(run.error_code(), "bad_env_key", "{params}");
        assert_valid("envelope.json", &run.json());
    }
    let run = home.rlxd(&["call", "--as-app", "--env", r#"{"HOME":"x"}"#, "status"]);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_eq!(run.error_code(), "bad_env_key");
    for params in [r#"{"PATH":1}"#, r#"{"PATH":"a\u0000b"}"#] {
        let run = home.rlxd(&["call", "--as-app", "env.set", params]);
        assert_eq!(run.error_code(), "invalid_params", "{params}: {run:?}");
    }
    assert_eq!(env_get(&home), before);
    assert_eq!(env_file(&home), file, "the file byte for byte");
}

#[test]
fn only_the_app_sets_them() {
    let home = TestHome::new();
    home.start_dev();
    app_sets(&home, r#"{"PATH":"/usr/bin"}"#);
    let run = home.rlxd(&["call", "env.get"]);
    let (before, file) = (run.stdout.clone(), env_file(&home));
    for args in [
        &["call", "env.set", r#"{"PATH":"/tmp"}"#][..],
        &[
            "call",
            "--role",
            "tui",
            "--client",
            "rlx-tui",
            "env.set",
            r#"{"PATH":"/tmp"}"#,
        ],
        &["call", "--env", r#"{"PATH":"/tmp"}"#, "status"],
        &[
            "call",
            "--role",
            "tui",
            "--env",
            r#"{"PATH":"/tmp"}"#,
            "status",
        ],
        &[
            "call",
            "--role",
            "app",
            "--gate-credential",
            "made-up",
            "--env",
            r#"{"PATH":"/tmp"}"#,
            "status",
        ],
    ] {
        let run = home.rlxd(args);
        assert_eq!(run.code(), Some(1), "{args:?}: {run:?}");
        assert_eq!(run.error_code(), "not_gate", "{args:?}");
        assert_valid("envelope.json", &run.json());
    }
    let run = home.rlxd(&["call", "env.get"]);
    assert_eq!(run.stdout, before, "env.get, byte for byte");
    assert_eq!(env_file(&home), file);
}

#[test]
fn an_unreadable_env_json_is_set_aside_and_reported() {
    let home = TestHome::new();
    let dir = home.root().join("rlxd");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("env.json"), "{nope").unwrap();
    home.start_dev();
    assert_eq!(env_get(&home), serde_json::json!({}));
    let aside: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("env.json.unreadable-"))
        .collect();
    assert_eq!(aside.len(), 1, "{aside:?}");
    assert_eq!(
        std::fs::read_to_string(dir.join(&aside[0])).unwrap(),
        "{nope"
    );
    let log = home.log();
    assert!(log.contains("env.json cannot be read"), "{log}");
    app_sets(&home, r#"{"PATH":"/usr/bin"}"#);
}

/// A program named `name` in `dir` that rlxd may start as Claude Code: a link to
/// `target` on macOS and Linux, as the Accept makes it; a copy on Windows.
fn place(target: &Path, dir: &Path, name: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let placed = dir.join(name);
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, &placed).unwrap();
    #[cfg(windows)]
    std::fs::copy(target, &placed).unwrap();
    placed
}

/// `status`'s `claude`.
fn claude(home: &TestHome) -> Value {
    home.status()["claude"].clone()
}

#[test]
fn claude_is_resolved_on_the_apps_path_when_it_is_set_and_kept_as_found() {
    // The Accept's "Absolute paths", up to the run: `status.claude.path` is the program a
    // run would start. With RLXD_CLAUDE_BIN unset.
    let home = TestHome::new();
    let root = home.root().to_path_buf();
    let (program, name) = if cfg!(windows) {
        // A program that runs: this rlxd, which prints no version a probe reads.
        (PathBuf::from(env!("CARGO_BIN_EXE_rlxd")), "claude.exe")
    } else {
        let script = root.join("fake-claude");
        std::fs::write(&script, "#!/bin/sh\necho 2.1.0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        (script, "claude")
    };
    let mut serve = home.command(&["serve", "--detach", "--gate-dev-mode"]);
    serve.env_remove("RLXD_CLAUDE_BIN").stdin(Stdio::null());
    let run = common::run(serve, None);
    assert_eq!(run.code(), Some(0), "{run:?}");

    let (bin, bin1) = (root.join("bin"), root.join("bin1"));
    let found = place(&program, &bin, name);
    std::fs::create_dir_all(&bin1).unwrap();
    let path = std::env::join_paths([&bin1, &bin]).unwrap();
    let set = serde_json::json!({"PATH": path.to_str().unwrap()}).to_string();
    app_sets(&home, &set);
    let seen = claude(&home);
    assert_eq!(seen["installed"], true, "{seen}");
    assert_eq!(
        seen["path"],
        found.display().to_string(),
        "as found: the link, not what it points to"
    );

    // Added after, ahead on PATH: rlxd resolved when the app set PATH, so it keeps bin's.
    let ahead = place(&program, &bin1, name);
    assert_eq!(claude(&home)["path"], found.display().to_string());
    app_sets(&home, &set);
    assert_eq!(
        claude(&home)["path"],
        ahead.display().to_string(),
        "again on a set"
    );

    // No claude on the app's PATH: not installed, whatever rlxd's own PATH holds.
    let empty = root.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    app_sets(
        &home,
        &serde_json::json!({"PATH": empty.to_str().unwrap()}).to_string(),
    );
    let seen = claude(&home);
    assert_eq!(seen["installed"], false, "{seen}");
    assert!(seen.get("path").is_none(), "{seen}");

    // Windows: npm's claude.cmd shim is found, and not run.
    #[cfg(windows)]
    {
        let shims = root.join("shims");
        std::fs::create_dir_all(&shims).unwrap();
        std::fs::write(shims.join("claude.cmd"), "@echo 2.1.0\r\n").unwrap();
        app_sets(
            &home,
            &serde_json::json!({"PATH": shims.to_str().unwrap()}).to_string(),
        );
        let seen = claude(&home);
        assert_eq!(seen["installed"], false, "a batch file is not run: {seen}");
    }
}
