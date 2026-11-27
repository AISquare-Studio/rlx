//! What `rlxd-hook` answers before it does anything else (handoff 0.10.3, 1.1), and what it
//! does with no rlxd to ask (1.5): a fleet hook is `rlx hook`'s, with the same stdin, and
//! `pre-tool-use` is denied. The hooks with rlxd running are `crates/rlxd/tests/hooks.rs`.

use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rlxd-hook"))
        .args(args)
        .output()
        .expect("rlxd-hook runs")
}

#[test]
fn version_prints_the_name_and_rlxs_version() {
    for flag in ["--version", "-V"] {
        let output = run(&[flag]);
        assert!(output.status.success(), "{flag}: {output:?}");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("rlxd-hook {}\n", env!("CARGO_PKG_VERSION"))
        );
    }
}

#[test]
fn help_prints_the_usage() {
    let output = run(&["--help"]);
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .starts_with("usage: rlxd-hook")
    );
}

/// A usage error says why and exits 0, as every hook does: Claude Code reads exit 2 as a
/// blocking error (review #2562).
#[test]
fn anything_else_is_a_usage_error() {
    for args in [&[][..], &["serve"], &["--version", "--help"]] {
        let output = run(args);
        assert_eq!(output.status.code(), Some(0), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .starts_with("usage: rlxd-hook")
        );
    }
}

/// A fresh home with no rlxd, and an `rlx` that says what it was run with: its arguments,
/// then its stdin.
struct NoRlxd {
    root: std::path::PathBuf,
    rlx: std::path::PathBuf,
}

impl NoRlxd {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("rlxd-hook-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let rlx = if cfg!(windows) {
            let rlx = root.join("rlx.cmd");
            std::fs::write(&rlx, "@echo off\r\necho fake rlx: %*\r\nfindstr \"^\"\r\n").unwrap();
            rlx
        } else {
            let rlx = root.join("rlx");
            std::fs::write(&rlx, "#!/bin/sh\necho \"fake rlx: $*\"\ncat\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&rlx, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            rlx
        };
        Self { root, rlx }
    }

    fn run(&self, args: &[&str], stdin: &[u8]) -> Output {
        self.run_with(args, stdin, &[])
    }

    /// [`NoRlxd::run`], with `envs` set too.
    fn run_with(&self, args: &[&str], stdin: &[u8], envs: &[(&str, &str)]) -> Output {
        use std::io::Write;
        let mut child = Command::new(env!("CARGO_BIN_EXE_rlxd-hook"))
            .args(args)
            .envs(envs.iter().copied())
            .env("AISQUARE_HOME", self.root.join("home"))
            .env_remove("RLX_HOME")
            .env("RLXD_RLX_BIN", &self.rlx)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("rlxd-hook runs");
        let mut pipe = child.stdin.take().unwrap();
        let writing = std::thread::spawn({
            let stdin = stdin.to_vec();
            move || {
                let _ = pipe.write_all(&stdin);
            }
        });
        let output = child.wait_with_output().unwrap();
        writing.join().unwrap();
        output
    }
}

impl Drop for NoRlxd {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn with_no_rlxd_a_fleet_hook_is_rlx_hooks_with_the_same_stdin_and_says_nothing() {
    let here = NoRlxd::new("fallback");
    let stdin = b"{\"session_id\":\"t1\",\"hook_event_name\":\"Stop\"}\n";
    for event in ["session-start", "stop", "session-end"] {
        let output = here.run(&[event], stdin);
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        let printed = String::from_utf8(output.stdout.clone()).unwrap();
        assert!(
            printed.starts_with(&format!("fake rlx: hook {event}")),
            "{printed}"
        );
        if cfg!(unix) {
            let mut expected = format!("fake rlx: hook {event}\n").into_bytes();
            expected.extend_from_slice(stdin);
            assert_eq!(
                printed.as_bytes(),
                expected,
                "the same stdin, byte for byte"
            );
        }
        assert!(
            output.stderr.is_empty(),
            "rlxd not running is today's path: {output:?}"
        );
    }
}

#[test]
fn with_no_rlxd_pre_tool_use_denies_and_never_falls_back() {
    let here = NoRlxd::new("deny");
    for args in [
        &["pre-tool-use"][..],
        &["pre-tool-use", "--run", "x", "--attempt", "1"],
    ] {
        let output = here.run(args, b"{\"tool_name\":\"t\",\"tool_input\":{}}");
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        let reply: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(reply["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(reply["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert!(!String::from_utf8_lossy(&output.stdout).contains("fake rlx"));
    }
}

#[test]
fn ping_with_no_rlxd_exits_3_and_says_why() {
    let here = NoRlxd::new("ping");
    let output = here.run(&["ping"], b"");
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("rlxd is not running"));
}

#[test]
fn an_event_it_does_not_take_is_a_usage_error() {
    for args in [&["codex"][..], &["hook", "stop"], &["stop", "extra"]] {
        let output = run(args);
        assert_eq!(output.status.code(), Some(0), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(String::from_utf8_lossy(&output.stderr).starts_with("usage: rlxd-hook"));
    }
    // The gate's hook run wrongly still denies: an exit 0 with nothing printed would let
    // Claude Code ask no one.
    for args in [
        &["pre-tool-use", "--run", "x"][..],
        &["pre-tool-use", "--attempt", "0", "--run", "x"],
    ] {
        let output = run(args);
        assert_eq!(output.status.code(), Some(0), "{args:?}: {output:?}");
        let reply: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(reply["hookSpecificOutput"]["permissionDecision"], "deny");
    }
}

/// A stand-in for rlxd in `here`'s home: it says ok to the hello and answers every `hook`
/// as it is told, as a wrong rlxd or brain could, counting them. It stops listening when
/// dropped.
#[cfg(unix)]
struct FakeRlxd {
    socket: std::path::PathBuf,
    hooks: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(unix)]
impl FakeRlxd {
    /// Answers each `hook` ok, with `stdout`.
    fn answering(here: &NoRlxd, stdout: &[u8]) -> Self {
        use rlxd_client::proto::b64;
        let data = serde_json::json!({"stdout_b64": b64::encode(stdout), "stderr_b64": "",
            "exit_code": 0});
        Self::replying(here, serde_json::json!({"ok": true, "data": data}))
    }

    /// Answers each `hook` with `reply`'s `ok` and `data` or `error`.
    fn replying(here: &NoRlxd, reply: serde_json::Value) -> Self {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixListener;

        use std::sync::atomic::Ordering;

        use rlxd_client::Home;
        use serde_json::{Value, json};

        let layout = Home::at(here.root.join("home"));
        std::fs::create_dir_all(layout.run_dir()).unwrap();
        std::fs::create_dir_all(layout.rlxd_dir()).unwrap();
        std::fs::write(layout.token(), "t").unwrap();
        let socket = layout.socket();
        let dir = socket.parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        let hooks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = std::sync::Arc::clone(&hooks);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let told = reply.clone();
                let counted = std::sync::Arc::clone(&counted);
                std::thread::spawn(move || {
                    let mut writer = &stream;
                    for line in BufReader::new(&stream).lines() {
                        let Ok(line) = line else { return };
                        let request: Value = serde_json::from_str(&line).unwrap_or_default();
                        let mut reply = match request["method"].as_str() {
                            Some("hello") => json!({"ok": true, "data": {"protocol_version": 1,
                                "protocol_versions": [1], "rlxd_version": "0.10.3",
                                "capabilities": []}}),
                            _ => {
                                counted.fetch_add(1, Ordering::SeqCst);
                                told.clone()
                            }
                        };
                        reply["schema_version"] = json!(1);
                        reply["id"] = request["id"].clone();
                        if writeln!(writer, "{reply}").is_err() {
                            return;
                        }
                    }
                });
            }
        });
        Self { socket, hooks }
    }

    /// How many `hook` requests reached it.
    fn hooks(&self) -> usize {
        self.hooks.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(unix)]
impl Drop for FakeRlxd {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Until the gate decides tool calls (5.1), rlxd-hook prints rlxd's `pre-tool-use` reply
/// only when it is a deny: an empty reply (on which Claude Code would ask no one), text,
/// `{}` or an allow is denied as unreached, and never goes to `rlx hook`.
#[cfg(unix)]
#[test]
fn pre_tool_use_prints_only_a_deny_whatever_rlxd_answers() {
    const UNREACHED: &str = "rlxd could not be asked about this tool call, so it is denied";
    let allow = r#"{"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "allow", "permissionDecisionReason": "fine"}}"#;
    let deny = rlxd_client::proto::deny_output("rlxd's own reason");
    for (case, stdout, passes) in [
        ("empty", &b""[..], false),
        ("text", b"just some text\n", false),
        ("object", b"{}", false),
        ("allow", allow.as_bytes(), false),
        ("deny", deny.as_bytes(), true),
    ] {
        let here = NoRlxd::new(&format!("gate-{case}"));
        let _rlxd = FakeRlxd::answering(&here, stdout);
        let output = here.run(
            &["pre-tool-use"],
            b"{\"tool_name\":\"t\",\"tool_input\":{}}",
        );
        assert_eq!(output.status.code(), Some(0), "{case}: {output:?}");
        assert!(output.stderr.is_empty(), "{case}: {output:?}");
        if passes {
            assert_eq!(
                output.stdout, stdout,
                "{case}: a deny goes through as it is"
            );
            continue;
        }
        let reply: serde_json::Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|e| panic!("{case}: not one JSON reply ({e}): {output:?}"));
        let said = &reply["hookSpecificOutput"];
        assert_eq!(said["permissionDecision"], "deny", "{case}");
        assert_eq!(said["permissionDecisionReason"], UNREACHED, "{case}");
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains("fake rlx"),
            "{case}"
        );
    }
}

/// A hook rlxd-hook cannot send as it is runs as `rlx hook`, with the same stdin, before
/// anything reaches rlxd: a request longer than rlxd reads in one frame (its stdin, or its
/// stdin with its variables), or a variable longer than rlxd-proto allows (review #2562).
#[cfg(unix)]
#[test]
fn a_hook_too_long_for_one_frame_runs_as_rlx_hook_and_never_reaches_rlxd() {
    use rlxd_client::proto::{HOOK_ENV_VALUE_MOST, HOOK_FRAME_MOST};

    let stdin_alone = vec![b'a'; HOOK_FRAME_MOST / 4 * 3 + 64];
    // In base64 it fits a frame alone (768 KiB), and not with ten long variables.
    let with_vars = vec![b'b'; HOOK_FRAME_MOST / 16 * 9];
    let near = "v".repeat(HOOK_ENV_VALUE_MOST - 1);
    let over = "w".repeat(HOOK_ENV_VALUE_MOST + 1);
    let many: Vec<(&str, &str)> = [
        "AISQUARE_BOARD_CONTEXT",
        "AISQUARE_BROKER",
        "AISQUARE_PERSONA",
        "AISQUARE_TEAM",
        "AISQUARE_TEAM_DELTA",
        "AISQUARE_PIPELINE_ID",
        "AISQUARE_LAUNCH_ID",
        "AISQUARE_FLEET_AGENT",
        "CLAUDE_EFFORT",
        "TMUX_PANE",
    ]
    .into_iter()
    .map(|name| (name, near.as_str()))
    .collect();
    type Case<'a> = (&'a str, &'a [u8], Vec<(&'a str, &'a str)>);
    let cases: [Case; 3] = [
        ("stdin", &stdin_alone, vec![]),
        ("stdin-and-vars", &with_vars, many),
        (
            "a-long-var",
            b"{}",
            vec![("AISQUARE_PERSONA", over.as_str())],
        ),
    ];
    for (case, stdin, envs) in cases {
        let here = NoRlxd::new(&format!("frame-{case}"));
        let rlxd = FakeRlxd::answering(&here, b"from rlxd\n");
        let output = here.run_with(&["user-prompt-submit"], stdin, &envs);
        assert_eq!(output.status.code(), Some(0), "{case}");
        let mut expected = b"fake rlx: hook user-prompt-submit\n".to_vec();
        expected.extend_from_slice(stdin);
        assert!(
            output.stdout == expected,
            "{case}: rlx hook, with the same stdin"
        );
        assert_eq!(rlxd.hooks(), 0, "{case}: nothing reached rlxd");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("running `rlx hook"),
            "{case}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// rlxd's refusal of the request itself (it handed nothing to the brain) runs the hook as
/// `rlx hook`; an error once the brain may have it never does (review #2562).
#[cfg(unix)]
#[test]
fn a_request_rlxd_refused_runs_as_rlx_hook_and_a_failed_hook_never_does() {
    for (code, falls_back) in [
        ("invalid_request", true),
        ("invalid_params", true),
        ("unknown_method", true),
        ("role_not_allowed", true),
        ("hook_not_delivered", true),
        ("hook_failed", false),
        ("hook_timeout", false),
        ("internal", false),
    ] {
        let here = NoRlxd::new(&format!("refused-{code}"));
        let rlxd = FakeRlxd::replying(
            &here,
            serde_json::json!({"ok": false, "error": {"code": code, "message": "m"}}),
        );
        let output = here.run(&["stop"], b"{\"session_id\":\"t1\"}\n");
        assert_eq!(output.status.code(), Some(0), "{code}");
        assert_eq!(rlxd.hooks(), 1, "{code}");
        let ran = String::from_utf8_lossy(&output.stdout).contains("fake rlx: hook stop");
        assert_eq!(ran, falls_back, "{code}: {output:?}");
    }
}

/// A directory or variable JSON cannot carry unchanged is not sent lossily: the hook runs
/// as `rlx hook`, which reads them as they are (review #2562's nit).
#[cfg(unix)]
#[test]
fn a_hook_whose_variable_is_not_utf8_runs_as_rlx_hook() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::process::Stdio;

    let here = NoRlxd::new("not-utf8");
    let rlxd = FakeRlxd::answering(&here, b"from rlxd\n");
    let output = Command::new(env!("CARGO_BIN_EXE_rlxd-hook"))
        .arg("stop")
        .env("AISQUARE_HOME", here.root.join("home"))
        .env_remove("RLX_HOME")
        .env("RLXD_RLX_BIN", &here.rlx)
        .env("AISQUARE_PERSONA", OsStr::from_bytes(b"caf\xe9"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(rlxd.hooks(), 0);
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("fake rlx: hook stop"));
}
