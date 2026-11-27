//! Hooks over the socket (handoff 0.10.3, 1.5, and 7.3's hook bullet): `rlxd-hook <event>`
//! prints what `rlx hook <event>` prints, has its side effect, publishes one `hook.<event>`,
//! falls back to `rlx hook` with rlxd stopped, and turns on the brain's `accepted`: before
//! it, the hook runs as `rlx hook`; after it, never again. `pre-tool-use` always denies.
//! And 1.6's twenty hooks through one brain.
//!
//! Each test seeds a project as 1.5's check does, with the checkout's own rlx
//! ([`common::real_rlx`]); they skip where there is none, unless `RLXD_INTEROP_REQUIRED`
//! is set, as rlxd.yml sets it.

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::{TestHome, fake_brain, real_rlx, rlxd_hook, workspace};
use serde_json::{Value, json};

/// A home with rlx's brain, and a project onboarded with a fact to print at session start.
struct Seeded {
    home: TestHome,
    rlx: PathBuf,
    project: PathBuf,
}

impl Seeded {
    fn new() -> Option<Self> {
        let rlx = real_rlx()?;
        Some(Self::with(rlx.clone(), rlx))
    }

    /// A home whose rlxd runs `brain` as its brain, and whose hooks fall back to `rlx`.
    fn with(brain: PathBuf, rlx: PathBuf) -> Self {
        let home = TestHome::new().with_rlx(brain);
        let project = home.root().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let git = std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&project)
            .status()
            .expect("git runs");
        assert!(git.success());
        let seeded = Self { home, rlx, project };
        seeded.rlx(&["project", "onboard", &seeded.project.display().to_string()]);
        seeded.rlx(&[
            "context",
            "add",
            "--project",
            "seed fact for the hook check",
        ]);
        seeded
    }

    /// `rlx args…` in the project, which must succeed: its stdout.
    fn rlx(&self, args: &[&str]) -> Vec<u8> {
        let (code, stdout, stderr) =
            self.home
                .run_bytes(&self.rlx, args, &self.project, b"", &[("RLXD_RLX_BIN", "")]);
        assert_eq!(code, Some(0), "rlx {args:?}: {stderr}");
        stdout
    }

    /// The JSON Claude Code sends for `event`, for session t1 in the project.
    fn payload(&self, event: &str, fields: Value) -> Vec<u8> {
        let mut payload = json!({"session_id": "t1", "cwd": self.project.display().to_string()});
        payload["hook_event_name"] = Value::String(event.to_owned());
        for (key, value) in fields.as_object().unwrap() {
            payload[key] = value.clone();
        }
        let mut bytes = serde_json::to_vec(&payload).unwrap();
        bytes.push(b'\n');
        bytes
    }

    /// `rlxd-hook <event>` with `stdin`, whose fallback is the real rlx: its stdout.
    fn rlxd_hook(&self, event: &str, stdin: &[u8]) -> Vec<u8> {
        let rlx = self.rlx.display().to_string();
        let (code, stdout, stderr) = self.home.run_bytes(
            &rlxd_hook(),
            &[event],
            &self.project,
            stdin,
            &[("RLXD_RLX_BIN", &rlx)],
        );
        assert_eq!(code, Some(0), "rlxd-hook {event} exits 0: {stderr}");
        stdout
    }

    /// `rlxd-hook <event>` with `stdin` and `envs`, whose fallback is the real rlx: its
    /// stdout and stderr.
    fn rlxd_hook_with(
        &self,
        event: &str,
        stdin: &[u8],
        envs: &[(&str, &str)],
    ) -> (Vec<u8>, String) {
        let rlx = self.rlx.display().to_string();
        let mut all = vec![("RLXD_RLX_BIN", rlx.as_str())];
        all.extend_from_slice(envs);
        let (code, stdout, stderr) =
            self.home
                .run_bytes(&rlxd_hook(), &[event], &self.project, stdin, &all);
        assert_eq!(code, Some(0), "rlxd-hook {event} exits 0: {stderr}");
        (stdout, stderr)
    }

    /// `rlx hook <event>` with `stdin`: its stdout.
    fn rlx_hook(&self, event: &str, stdin: &[u8]) -> Vec<u8> {
        let (code, stdout, stderr) =
            self.home
                .run_bytes(&self.rlx, &["hook", event], &self.project, stdin, &[]);
        assert_eq!(code, Some(0), "rlx hook {event}: {stderr}");
        stdout
    }

    /// How many prompts `rlx --json log` holds with `text`.
    fn logged(&self, text: &str) -> usize {
        let log: Value = serde_json::from_slice(&self.rlx(&["--json", "log"])).unwrap();
        let printed = log.to_string();
        printed.matches(text).count()
    }

    /// The `hook` topic's events so far.
    fn hook_events(&self) -> Vec<Value> {
        let run = self
            .home
            .rlxd(&["events", "--topics", "hook", "--since", "0"]);
        assert_eq!(run.code(), Some(0), "{run:?}");
        run.lines()
    }
}

/// Each fleet event's fields beyond the session and directory.
fn fleet_events() -> Vec<(&'static str, &'static str, Value)> {
    vec![
        (
            "session-start",
            "SessionStart",
            json!({"source": "startup"}),
        ),
        // Not ASCII, so the bytes each side reads and writes are its stdio's encoding's,
        // on Windows too (review #2562).
        (
            "user-prompt-submit",
            "UserPromptSubmit",
            json!({"prompt": "x — naïve 日本語 ✓"}),
        ),
        ("stop", "Stop", json!({"stop_hook_active": false})),
        (
            "notification",
            "Notification",
            json!({"message": "m — café ✓", "notification_type": "idle_prompt"}),
        ),
        (
            "stop-failure",
            "StopFailure",
            json!({"error": "overloaded"}),
        ),
        ("session-end", "SessionEnd", json!({"reason": "exit"})),
    ]
}

#[test]
fn every_hook_prints_what_rlx_hook_prints_and_publishes_one_event() {
    let Some(seeded) = Seeded::new() else { return };
    seeded.home.start();
    for (event, name, fields) in fleet_events() {
        let payload = seeded.payload(name, fields);
        let through_rlxd = seeded.rlxd_hook(event, &payload);
        assert_eq!(
            through_rlxd,
            seeded.rlx_hook(event, &payload),
            "{event}: {}",
            String::from_utf8_lossy(&through_rlxd)
        );
        if event == "session-start" {
            let printed = String::from_utf8_lossy(&through_rlxd);
            assert!(
                printed.contains("seed fact for the hook check"),
                "{printed}"
            );
        }
    }
    // The gate's hook: denied, never allowed, and its event too (7.3: all seven).
    let payload = seeded.payload(
        "PreToolUse",
        json!({"tool_name": "mcp__claude_ai_Slack__slack_send_message", "tool_input": {}}),
    );
    let denied = seeded.rlxd_hook("pre-tool-use", &payload);
    let reply: Value = serde_json::from_slice(&denied).unwrap();
    assert_eq!(reply["hookSpecificOutput"]["permissionDecision"], "deny");
    let events = seeded.hook_events();
    for (event, _, _) in fleet_events()
        .into_iter()
        .chain([("pre-tool-use", "", json!({}))])
    {
        let r#type = format!("hook.{}", event.replace('-', "_"));
        let matching: Vec<&Value> = events.iter().filter(|e| e["type"] == r#type).collect();
        assert_eq!(matching.len(), 1, "{type}: {events:?}");
        assert_eq!(matching[0]["data"]["session_id"], "t1");
        assert_eq!(
            matching[0]["data"]["cwd"],
            seeded.project.display().to_string()
        );
    }
    let everything = serde_json::to_string(&events).unwrap();
    assert!(!everything.contains("\"allow\""), "no allow: {everything}");
}

#[test]
fn the_prompt_is_recorded_once_through_the_brain() {
    let Some(seeded) = Seeded::new() else { return };
    seeded.home.start();
    let payload = seeded.payload("UserPromptSubmit", json!({"prompt": "hook-parity-probe"}));
    assert_eq!(
        seeded.rlxd_hook("user-prompt-submit", &payload),
        seeded.rlx_hook("user-prompt-submit", &payload)
    );
    // The brain's run and `rlx hook`'s: two, each once.
    assert_eq!(seeded.logged("hook-parity-probe"), 2);
}

#[test]
fn twenty_hooks_go_through_one_brain() {
    let Some(seeded) = Seeded::new() else { return };
    seeded.home.start();
    let first = seeded.home.status()["brain"]["pid"].as_u64().unwrap();
    let payload = seeded.payload("UserPromptSubmit", json!({"prompt": "x"}));
    for _ in 0..20 {
        seeded.rlxd_hook("user-prompt-submit", &payload);
    }
    let brain = &seeded.home.status()["brain"];
    assert_eq!(brain["pid"].as_u64(), Some(first), "{brain}");
    assert!(brain["handled"].as_u64().unwrap() >= 20, "{brain}");
    assert_eq!(brain["restarts"], 0, "{brain}");
}

#[test]
fn with_rlxd_stopped_a_hook_is_rlx_hooks() {
    let Some(seeded) = Seeded::new() else { return };
    let payload = seeded.payload("SessionStart", json!({"source": "startup"}));
    let printed = seeded.rlxd_hook("session-start", &payload);
    assert!(!printed.is_empty());
    assert_eq!(printed, seeded.rlx_hook("session-start", &payload));
    // The gate's hook never falls back: it denies.
    let denied = seeded.rlxd_hook("pre-tool-use", &seeded.payload("PreToolUse", json!({})));
    let reply: Value = serde_json::from_slice(&denied).unwrap();
    assert_eq!(reply["hookSpecificOutput"]["permissionDecision"], "deny");
}

#[test]
fn a_hook_the_brain_never_accepted_runs_as_rlx_hook_once() {
    let Some(rlx) = real_rlx() else { return };
    let seeded = Seeded::with(fake_brain("mute"), rlx);
    seeded.home.start();
    let payload = seeded.payload(
        "UserPromptSubmit",
        json!({"prompt": "never-accepted-probe"}),
    );
    let started = Instant::now();
    seeded.rlxd_hook("user-prompt-submit", &payload);
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "it waited for an accept"
    );
    assert_eq!(
        seeded.logged("never-accepted-probe"),
        1,
        "the fallback ran it, once"
    );
}

#[test]
fn a_hook_the_brain_accepted_never_runs_again() {
    let Some(rlx) = real_rlx() else { return };
    let seeded = Seeded::with(fake_brain("accepts-then-dies"), rlx);
    seeded.home.start();
    let payload = seeded.payload("UserPromptSubmit", json!({"prompt": "accepted-probe"}));
    let printed = seeded.rlxd_hook("user-prompt-submit", &payload);
    assert!(printed.is_empty(), "{}", String::from_utf8_lossy(&printed));
    assert_eq!(
        seeded.logged("accepted-probe"),
        0,
        "the brain accepted it: no fallback ran it"
    );
}

/// Python, from the checkout's environment, as `real_rlx` finds rlx: `script` with `args`.
fn python(script: &str, args: &[&str]) -> std::process::Command {
    let mut command = std::process::Command::new("uv");
    command
        .args(["run", "--quiet", "python", "-c", script])
        .args(args)
        .current_dir(workspace().parent().unwrap());
    command
}

/// A session's end through the brain outlasts the store's wait for a busy database (5 s),
/// as `rlx hook` does: under a 4 s `BEGIN EXCLUSIVE` it waits, ends the session's row, and
/// never times out (review #2562: a 3 s deadline lost it).
#[test]
fn a_session_end_waits_out_a_busy_store_as_rlx_hook_does() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;

    let Some(seeded) = Seeded::new() else { return };
    seeded.home.start();
    let role = [("AISQUARE_ROLE", "coder")];
    let start = seeded.payload("SessionStart", json!({"source": "startup"}));
    seeded.rlxd_hook_with("session-start", &start, &role);
    let store = seeded.home.root().join("context.db").display().to_string();
    let ended_at = || {
        let output = python(
            "import sqlite3, sys\n\
             row = sqlite3.connect(sys.argv[1]).execute(\n\
                 'SELECT ended_at FROM team_session WHERE id = ?', (sys.argv[2],)).fetchone()\n\
             print('none' if row is None else (row[0] or ''))",
            &[&store, "t1"],
        )
        .output()
        .expect("python runs");
        assert!(output.status.success(), "{output:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };
    assert_eq!(ended_at(), "", "session-start made t1's row, not ended");
    let mut holder = python(
        "import sqlite3, sys, time\n\
         db = sqlite3.connect(sys.argv[1], isolation_level=None, timeout=30)\n\
         db.execute('BEGIN EXCLUSIVE')\n\
         print('held', flush=True)\n\
         time.sleep(4)\n\
         db.execute('COMMIT')",
        &[&store],
    )
    .stdout(Stdio::piped())
    .spawn()
    .expect("python runs");
    let mut said = String::new();
    BufReader::new(holder.stdout.take().unwrap())
        .read_line(&mut said)
        .unwrap();
    assert_eq!(said.trim(), "held");
    let started = Instant::now();
    let end = seeded.payload("SessionEnd", json!({"reason": "exit"}));
    let (_, stderr) = seeded.rlxd_hook_with("session-end", &end, &role);
    let took = started.elapsed();
    assert!(holder.wait().unwrap().success());
    assert!(!stderr.contains("not run again"), "{stderr}");
    assert!(
        took >= Duration::from_secs(3),
        "it waited for the store: {took:?}"
    );
    assert!(
        took < Duration::from_secs(8),
        "inside rlxd-hook's budget: {took:?}"
    );
    assert_ne!(ended_at(), "", "the session's row is ended");
}

/// A hook the brain took and stalled on before rlxd read that it had (its `accepted` still
/// in the pipe when the accept wait ends) may have run: rlxd says `hook_timeout`, and
/// rlxd-hook runs nothing more, so the prompt is not logged a second time by `rlx hook`
/// (review #2555 M1, tester #2645). With refuse()'s drain removed, rlxd says
/// `hook_not_delivered` and rlxd-hook runs `rlx hook`: logged once.
#[test]
fn a_hook_the_brain_took_unread_never_runs_again() {
    let Some(rlx) = real_rlx() else { return };
    let seeded = Seeded::with(fake_brain("accepts-unterminated"), rlx);
    seeded.home.start();
    let payload = seeded.payload(
        "UserPromptSubmit",
        json!({"prompt": "accepted-unread-probe"}),
    );
    let (printed, stderr) = seeded.rlxd_hook_with("user-prompt-submit", &payload, &[]);
    assert!(printed.is_empty(), "{}", String::from_utf8_lossy(&printed));
    assert!(stderr.contains("hook_timeout"), "{stderr}");
    assert!(stderr.contains("stalled"), "the message says how: {stderr}");
    assert_eq!(
        seeded.logged("accepted-unread-probe"),
        0,
        "the brain took it: no fallback ran it"
    );
}

#[test]
fn rlxd_hook_ping_is_quick() {
    let Some(seeded) = Seeded::new() else { return };
    seeded.home.start();
    let hook = rlxd_hook();
    let mut times = Vec::new();
    for _ in 0..21 {
        let started = Instant::now();
        let (code, _, stderr) =
            seeded
                .home
                .run_bytes(&hook, &["ping"], seeded.home.root(), b"", &[]);
        times.push(started.elapsed());
        assert_eq!(code, Some(0), "{stderr}");
    }
    times.sort();
    let median = times[times.len() / 2];
    eprintln!("rlxd-hook ping: median {median:?}, slowest {:?}", times[20]);
    // 1.5's check is on macOS and Linux; Windows starts processes slower.
    if !cfg!(windows) {
        assert!(median < Duration::from_millis(30), "median {median:?}");
    }
}

/// `hook`'s params for `event`, with `env`.
fn hook_params(home: &TestHome, event: &str, env: Value) -> String {
    json!({
        "event": event,
        "stdin_b64": "eyJzZXNzaW9uX2lkIjoidDEifQ==",
        "cwd": home.root().display().to_string(),
        "env": env,
    })
    .to_string()
}

#[test]
fn a_hook_of_another_home_is_not_delivered_and_publishes_nothing() {
    let home = TestHome::new();
    home.start();
    let own = home.root().display().to_string();
    let elsewhere = home.root().join("elsewhere").display().to_string();
    for env in [
        json!({"AISQUARE_HOME": elsewhere}),
        json!({"RLX_HOME": elsewhere, "AISQUARE_HOME": own}),
        // rlx before and after 0.10.2 would disagree on this one's home.
        json!({"RLX_HOME": own, "AISQUARE_HOME": elsewhere}),
    ] {
        let params = hook_params(&home, "stop", env.clone());
        let refused = home.rlxd(&["call", "hook", &params]);
        assert_eq!(
            refused.error_code(),
            "hook_not_delivered",
            "{env}: {refused:?}"
        );
    }
    // The same home, written another way (a relative path from the hook's directory),
    // is this rlxd's: it gets as far as the brain, which this home has none of.
    let relative = json!({"AISQUARE_HOME": "."});
    let params = hook_params(&home, "stop", relative);
    let reached = home.rlxd(&["call", "hook", &params]);
    assert_eq!(reached.error_code(), "hook_not_delivered", "{reached:?}");
    let message = reached.json()["error"]["message"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(message.contains("brain never took"), "{message}");
    let events = home.rlxd(&["events", "--topics", "hook", "--since", "0"]);
    let published: Vec<Value> = events.lines();
    assert_eq!(
        published.len(),
        1,
        "only the hook of this home: {published:?}"
    );
}

#[test]
fn hook_is_for_command_line_clients_only() {
    let home = TestHome::new();
    home.start();
    let own = home.root().display().to_string();
    let params = hook_params(&home, "stop", json!({"AISQUARE_HOME": own}));
    for role in ["tui", "remote"] {
        let refused = home.rlxd(&["call", "--role", role, "--client", "probe", "hook", &params]);
        assert_eq!(refused.error_code(), "role_not_allowed", "{refused:?}");
        assert_eq!(refused.json()["error"]["detail"]["method"], "hook");
        assert_eq!(refused.json()["error"]["detail"]["role"], role);
    }
    // A raw call naming a variable off the list, a case variant included, is refused, not
    // passed on (tester #2645): rlxd-hook never sends one.
    for env in [
        json!({"AISQUARE_HOME": own, "aisquare_role": "coder"}),
        json!({"AISQUARE_HOME": own, "ANTHROPIC_API_KEY": "k"}),
    ] {
        let params = hook_params(&home, "stop", env.clone());
        let refused = home.rlxd(&["call", "hook", &params]);
        assert_eq!(refused.error_code(), "invalid_params", "{env}: {refused:?}");
    }
    let events = home.rlxd(&["events", "--topics", "hook", "--since", "0"]);
    assert!(
        events.lines().is_empty(),
        "a refused hook publishes nothing"
    );
}
