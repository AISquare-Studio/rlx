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
    // The tests' RLXD_CLAUDE_BIN wins over any PATH, and status says so.
    assert_eq!(home.status()["claude"]["path_source"], "rlxd_claude_bin");
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
        // Refused by the gate, or not the app: not_gate before any look at the keys (7.2).
        &[
            "call",
            "--role",
            "app",
            "--gate-credential",
            "made-up",
            "--env",
            r#"{"HOME":"x"}"#,
            "status",
        ],
        &[
            "call",
            "--role",
            "tui",
            "--env",
            r#"{"HOME":"x"}"#,
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
    // run would start. With RLXD_CLAUDE_BIN unset, so rlxd finds the stub on its own PATH
    // until the app sets one, and a user of its own.
    let home = TestHome::new().with_own_user();
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
    // The app has set no PATH yet: rlxd looks on its own, and says so.
    assert_eq!(claude(&home)["path_source"], "rlxd");

    let (bin, bin1) = (root.join("bin"), root.join("bin1"));
    let found = place(&program, &bin, name);
    std::fs::create_dir_all(&bin1).unwrap();
    let path = std::env::join_paths([&bin1, &bin]).unwrap();
    let set = serde_json::json!({"PATH": path.to_str().unwrap()}).to_string();
    app_sets(&home, &set);
    let seen = claude(&home);
    assert_eq!(seen["installed"], true, "{seen}");
    assert_eq!(seen["path_source"], "app", "{seen}");
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
    assert_eq!(seen["path_source"], "app", "the PATH searched: {seen}");

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

#[test]
fn no_test_can_start_a_real_claude() {
    // A real claude would read the developer's sign-in. To the tests' guard, a real one is
    // a `claude` in a folder outside the test's home: here, another home's.
    let home = TestHome::new().with_own_user();
    let elsewhere = TestHome::new();
    let real_bin = elsewhere.root().join("bin");
    std::fs::create_dir_all(&real_bin).unwrap();
    let real = real_bin.join(if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    });
    std::fs::write(&real, "").unwrap();

    // rlxd's own PATH: the stub's folder first, the real one's left off.
    let base = std::env::join_paths([real_bin.clone(), home.root().join("bin")]).unwrap();
    let path: Vec<PathBuf> = std::env::split_paths(&home.rlxd_path(&base)).collect();
    assert_eq!(
        path,
        [
            common::stub_claude_dir().to_path_buf(),
            home.root().join("bin")
        ]
    );

    // RLXD_CLAUDE_BIN unset and no PATH from the app: rlxd finds the stub on its own.
    let mut serve = home.command(&["serve", "--detach", "--gate-dev-mode"]);
    serve.env_remove("RLXD_CLAUDE_BIN").stdin(Stdio::null());
    let run = common::run(serve, None);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let stub = common::stub_claude_dir().join(if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    });
    let seen = claude(&home);
    assert_eq!(seen["path_source"], "rlxd", "{seen}");
    assert_eq!(seen["path"], stub.display().to_string(), "{seen}");

    // What would let rlxd start the real one fails the test before anything starts.
    let fails =
        |what: &dyn Fn()| std::panic::catch_unwind(std::panic::AssertUnwindSafe(what)).is_err();
    let set = serde_json::json!({"PATH": real_bin.to_str().unwrap()}).to_string();
    assert!(fails(&|| drop(
        home.command(&["call", "--as-app", "env.set", &set])
    )));
    assert!(fails(&|| drop(
        home.command(&["call", "--as-app", "--env", &set, "status"])
    )));
    let raw = format!(r#"{{"id":1,"method":"env.set","params":{set}}}"#);
    assert!(fails(&|| drop(
        home.rlxd_with_stdin(&["call", "--raw"], Some(&raw))
    )));
    let stored = home.root().join("rlxd").join("env.json");
    let before = std::fs::read(&stored).ok();
    std::fs::write(&stored, &set).unwrap();
    assert!(fails(&|| drop(home.command(&["serve", "--detach"]))));
    match before {
        Some(text) => std::fs::write(&stored, text).unwrap(),
        None => std::fs::remove_file(&stored).unwrap(),
    }
    let named = TestHome::new().with_claude(real.clone());
    assert!(fails(&|| drop(named.command(&["status"]))));
    // The home's own folders, and the stub's, are the tests'.
    let own = serde_json::json!({"PATH": home.root().join("bin").to_str().unwrap()}).to_string();
    drop(home.command(&["call", "--as-app", "env.set", &own]));
}

/// 4.3's Accept line, exactly as the handoff writes it (rev 7): it must exit 0. It writes
/// `tree.txt` where it runs.
#[cfg(unix)]
const CRATE_BAN: &str = "cargo tree --manifest-path rlxd/Cargo.toml --target all -e normal > tree.txt && test -s tree.txt && ! grep -qE 'security-framework|keyring' tree.txt";

/// [`CRATE_BAN`] through `sh`, in a fresh folder whose `rlxd` is a link to the
/// repository's, with `first` ahead on `PATH`: its exit code, and what it printed.
#[cfg(unix)]
fn run_crate_ban(first: &Path) -> (Option<i32>, String) {
    let cwd = TestHome::new();
    std::os::unix::fs::symlink(common::workspace(), cwd.root().join("rlxd")).unwrap();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::iter::once(first.to_path_buf()).chain(std::env::split_paths(&path));
    let output = std::process::Command::new("sh")
        .args(["-c", CRATE_BAN])
        .current_dir(cwd.root())
        .env("PATH", std::env::join_paths(path).unwrap())
        .stdin(Stdio::null())
        .output()
        .expect("sh runs");
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.code(), printed)
}

/// The ban, on a `cargo tree` of rlxd: `Err` with the lines the Accept's `grep -E
/// 'security-framework|keyring'` matches (its exit 0), or when the output is no tree of
/// rlxd at all, so a `cargo tree` that printed nothing can't pass.
fn crate_ban(tree: &str) -> Result<(), String> {
    if !tree.lines().any(|line| line.starts_with("rlxd v")) {
        return Err(format!("not a cargo tree of rlxd: {tree:?}"));
    }
    let found: Vec<&str> = tree
        .lines()
        .filter(|line| line.contains("security-framework") || line.contains("keyring"))
        .collect();
    if found.is_empty() {
        Ok(())
    } else {
        Err(format!("a Keychain crate in rlxd: {found:?}"))
    }
}

/// `cargo tree --manifest-path rlxd/Cargo.toml`, then `more`, from the repository's root,
/// where the Accept runs it, by the cargo running the tests: its output.
fn cargo_tree(more: &[&str]) -> String {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = std::process::Command::new(cargo)
        .args(["tree", "--manifest-path", "rlxd/Cargo.toml"])
        .args(more)
        .current_dir(common::workspace().parent().unwrap())
        .stdin(Stdio::null())
        .output()
        .expect("cargo runs");
    assert!(
        output.status.success(),
        "cargo tree {more:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// rlx keeps `/usr/bin/security` the only Keychain reader, so rlxd links no Keychain
/// crate (4.3). The Accept's line itself through `sh` where there is one, and on every OS
/// its two halves: the `cargo tree`, and its `grep`'s match ([`crate_ban`]), for the host
/// and for every target's normal dependencies (`--target all -e normal`), which
/// `deny.toml`'s bans hold too.
#[test]
fn no_keychain_crate_in_rlxd() {
    #[cfg(unix)]
    {
        // The line's `cargo` is the one running the tests.
        let cargo = PathBuf::from(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
        let first = cargo.parent().unwrap_or(Path::new("")).to_path_buf();
        let (code, printed) = run_crate_ban(&first);
        assert_eq!(code, Some(0), "{CRATE_BAN}: {printed}");
    }
    for more in [&[][..], &["--target", "all", "-e", "normal"][..]] {
        if let Err(why) = crate_ban(&cargo_tree(more)) {
            panic!("cargo tree {more:?}: {why}");
        }
    }
}

/// The ban can't pass by finding nothing: a Keychain crate planted in a tree fails it, as
/// does an output that is no tree of rlxd. The Accept's line too, given a `cargo` that
/// prints such a tree, prints nothing, or fails.
#[test]
fn the_crate_ban_fails_on_a_planted_keychain_crate() {
    let tree = "rlxd v0.10.1 (/src/rlx/rlxd/crates/rlxd)\n\
                ├── anyhow v1.0.98\n\
                └── tokio v1.47.1\n";
    assert_eq!(crate_ban(tree), Ok(()));
    for planted in [
        "│   └── security-framework v2.11.1",
        "├── security-framework-sys v2.14.0",
        "└── keyring v3.6.2",
    ] {
        let why = crate_ban(&format!("{tree}{planted}\n")).unwrap_err();
        assert!(why.contains(planted), "{why}");
    }
    assert!(crate_ban("").is_err());
    assert!(crate_ban("error: could not find `Cargo.toml`\n").is_err());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let fakes = TestHome::new();
        let bin = fakes.root().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        // A `cargo` that prints `out` and exits `code`, whatever it is asked.
        let fake_cargo = |out: &str, code: i32| {
            let printed = fakes.root().join("printed");
            std::fs::write(&printed, out).unwrap();
            let script = format!("#!/bin/sh\ncat '{}'\nexit {code}\n", printed.display());
            let cargo = bin.join("cargo");
            std::fs::write(&cargo, script).unwrap();
            std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        fake_cargo(tree, 0);
        let (code, printed) = run_crate_ban(&bin);
        assert_eq!(code, Some(0), "a clean tree: {printed}");
        for planted in ["│   └── security-framework v2.11.1", "└── keyring v3.6.2"] {
            fake_cargo(&format!("{tree}{planted}\n"), 0);
            let (code, printed) = run_crate_ban(&bin);
            assert_eq!(code, Some(1), "{planted}: {printed}");
        }
        fake_cargo("", 0);
        let (code, printed) = run_crate_ban(&bin);
        assert_eq!(code, Some(1), "a cargo tree that prints nothing: {printed}");
        fake_cargo("error: failed to parse manifest\n", 101);
        let (code, printed) = run_crate_ban(&bin);
        assert_eq!(code, Some(101), "a cargo tree that fails: {printed}");
    }
}
