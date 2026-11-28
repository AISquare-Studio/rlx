//! What a run's Claude Code is started with (handoff 0.10.3, 2.1 and 5.1): its command
//! line, and its own settings file with the gate's PreToolUse hook.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use rlxd_proto::{Mount, RunStartParams};
use serde_json::{Value, json};

/// The command line after the program, for `attempt` of the run `params` asks for, with
/// its settings file at `settings`:
///
/// ```text
/// -p --output-format stream-json --verbose --input-format stream-json
///   --session-id <run_id>          (--resume <run_id> from attempt 2)
///   [--no-session-persistence]     (a run whose session does not persist)
///   --tools "" [--strict-mcp-config]   (no_tools adds --strict-mcp-config)
///   [--json-schema <output_schema>] [--append-system-prompt <text>]
///   [--include-partial-messages]
///   --settings <settings> --setting-sources project --permission-mode dontAsk
/// ```
///
/// Never `--allowedTools`, which would let calls past the gate, nor `--bare`, which
/// would lose the claude.ai login and its connectors.
pub fn args(params: &RunStartParams, attempt: u64, settings: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--input-format",
        "stream-json",
    ]
    .map(OsString::from)
    .to_vec();
    let id = params.run_id.as_str();
    args.extend([
        if attempt > 1 {
            "--resume"
        } else {
            "--session-id"
        }
        .into(),
        id.into(),
    ]);
    if !params.persists() {
        args.push("--no-session-persistence".into());
    }
    args.extend(["--tools".into(), OsString::new()]);
    if params.mount == Mount::NoTools {
        args.push("--strict-mcp-config".into());
    }
    if let Some(schema) = &params.output_schema {
        args.extend([
            "--json-schema".into(),
            Value::Object(schema.clone()).to_string().into(),
        ]);
    }
    if let Some(text) = &params.append_system_prompt {
        args.extend(["--append-system-prompt".into(), text.into()]);
    }
    if params.partial {
        args.push("--include-partial-messages".into());
    }
    args.extend([
        "--settings".into(),
        settings.as_os_str().to_owned(),
        "--setting-sources".into(),
        "project".into(),
        "--permission-mode".into(),
        "dontAsk".into(),
    ]);
    args
}

/// The run's own settings for `attempt` (5.1): one PreToolUse hook, matching every tool,
/// that runs `rlxd-hook pre-tool-use --run <run_id> --attempt <n> --deadline-ms <ms>` by the
/// absolute path `hook` (its own timer is `deadline_ms` and 5 s), with a timeout of
/// `deadline_ms / 1000 + 10` seconds (rounded up), so Claude
/// Code never ends the hook before rlxd's own deadline.
pub fn settings(params: &RunStartParams, attempt: u64, hook: &Path) -> Value {
    settings_via(params, attempt, hook, None)
}

/// [`settings`], the hook run as `hook <via> pre-tool-use …`: test builds name rlxd itself
/// with `__test-hook` until rlxd-hook's own pre-tool-use is wired.
pub fn settings_via(
    params: &RunStartParams,
    attempt: u64,
    hook: &Path,
    via: Option<&str>,
) -> Value {
    let via = via.map(|via| format!(" {via}")).unwrap_or_default();
    let command = format!(
        "{}{via} pre-tool-use --run {} --attempt {attempt} --deadline-ms {}",
        quoted(hook),
        params.run_id,
        params.deadline_ms
    );
    let timeout = params.deadline_ms.div_ceil(1000) + 10;
    json!({"hooks": {"PreToolUse": [{"matcher": "*", "hooks": [
        {"type": "command", "command": command, "timeout": timeout}
    ]}]}})
}

/// The command line after the program of a `connectors.call` run (5.3; spike #5, route a):
/// a control channel with no session and its MCP servers loaded, on which every
/// permission prompt comes to rlxd as a `can_use_tool` request. No `--settings`, so no
/// hook of rlxd's is consulted: the `can_use_tool` alone carries the gate's decision.
///
/// ```text
/// -p --input-format stream-json --output-format stream-json --verbose
///   --tools "" --no-session-persistence --setting-sources project
///   --permission-mode manual --permission-prompts host --permission-prompt-tool stdio
/// ```
pub fn call_args() -> Vec<OsString> {
    [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--tools",
        "",
        "--no-session-persistence",
        "--setting-sources",
        "project",
        "--permission-mode",
        "manual",
        "--permission-prompts",
        "host",
        "--permission-prompt-tool",
        "stdio",
    ]
    .map(OsString::from)
    .to_vec()
}

/// The command line after the program of the connectors' one-turn probe (5.3), with its
/// settings file at `settings`: a run's, with no session, its MCP servers loaded
/// (`--tools ""` and no `--strict-mcp-config`), and one turn at most.
///
/// ```text
/// -p --output-format stream-json --verbose --input-format stream-json
///   --no-session-persistence --tools "" --max-turns 1
///   --settings <settings> --setting-sources project --permission-mode dontAsk
/// ```
pub fn probe_args(settings: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--input-format",
        "stream-json",
        "--no-session-persistence",
        "--tools",
        "",
        "--max-turns",
        "1",
        "--settings",
    ]
    .map(OsString::from)
    .to_vec();
    args.push(settings.as_os_str().to_owned());
    args.extend(
        [
            "--setting-sources",
            "project",
            "--permission-mode",
            "dontAsk",
        ]
        .map(OsString::from),
    );
    args
}

/// The probe's own settings: one PreToolUse hook, matching every tool, that runs
/// `rlxd __probe-deny` by rlxd's absolute path `rlxd`. rlxd itself denies each call the
/// probe makes: the hook opens no connection, and no call reaches the app. Refused, saying
/// why, for a path a hook's shell would misread.
pub fn probe_settings(rlxd: &Path) -> Result<Value, String> {
    if !rlxd.is_absolute() {
        return Err(format!(
            "rlxd's own path, {}, is not absolute",
            rlxd.display()
        ));
    }
    if cfg!(windows) && rlxd.to_string_lossy().contains(WINDOWS_SHELL_CHARACTERS) {
        return Err(format!(
            "rlxd's own path, {}, holds a character a hook's shell would read",
            rlxd.display()
        ));
    }
    let command = format!("{} __probe-deny", quoted(rlxd));
    Ok(json!({"hooks": {"PreToolUse": [{"matcher": "*", "hooks": [
        {"type": "command", "command": command, "timeout": 10}
    ]}]}}))
}

/// What `rlxd __probe-deny` prints: Claude Code's PreToolUse deny, by rlxd.
pub fn probe_deny_output() -> String {
    json!({"hookSpecificOutput": {"hookEventName": "PreToolUse",
        "permissionDecision": "deny",
        "permissionDecisionReason": "Denied by rlxd: a connectors probe runs no tool."}})
    .to_string()
}

/// `path` as a shell word, for the shell Claude Code runs a hook's command through: as it
/// is when it holds only characters no shell treats specially, else in double quotes,
/// with `"`, `\`, `$` and `` ` `` escaped.
fn quoted(path: &Path) -> String {
    quoted_for(&path.to_string_lossy(), cfg!(windows))
}

/// [`quoted`], for Windows when `windows`: there the path is written with forward slashes,
/// which Windows takes, always in double quotes, and with nothing escaped:
/// [`hook_beside`] refuses a path holding a character that Git Bash or `cmd` would read in
/// double quotes, so the command reads the same in either.
fn quoted_for(path: &str, windows: bool) -> String {
    let text = if windows {
        path.replace('\\', "/")
    } else {
        path.to_owned()
    };
    let plain = !windows
        && !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:@%,=".contains(c));
    if plain {
        return text;
    }
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for c in text.chars() {
        if !windows && matches!(c, '"' | '\\' | '$' | '`') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

/// The characters a Windows hook path may not hold: what Git Bash or `cmd` reads inside
/// double quotes.
const WINDOWS_SHELL_CHARACTERS: &[char] = &['"', '$', '`', '%', '!'];

/// `rlxd-hook` beside this rlxd, `rlxd` being this rlxd's own path: its absolute path,
/// whether or not it is there yet. Never a bare name looked up on `PATH` (rev 7, 4.3 and
/// 5.1): when rlxd's own path is unknown or not absolute, why not.
pub fn hook_beside(rlxd: Option<&Path>) -> Result<PathBuf, String> {
    let name = if cfg!(windows) {
        "rlxd-hook.exe"
    } else {
        "rlxd-hook"
    };
    match rlxd.and_then(Path::parent) {
        Some(dir) if cfg!(windows) && dir.to_string_lossy().contains(WINDOWS_SHELL_CHARACTERS) => {
            Err(format!(
                "rlxd's own folder, {}, holds a character a hook's shell would read ({}), so \
                 the rlxd-hook beside it can't be named in a hook command: install rlx in a \
                 folder without one",
                dir.display(),
                WINDOWS_SHELL_CHARACTERS.iter().collect::<String>()
            ))
        }
        Some(dir) if dir.is_absolute() => Ok(dir.join(name)),
        Some(dir) => Err(format!(
            "rlxd's own folder, {}, is not an absolute path, so the path of the rlxd-hook \
             beside it can't be named",
            dir.display()
        )),
        None => Err(
            "rlxd could not find its own path, so the path of the rlxd-hook beside \
                     it can't be named"
                .to_owned(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use rlxd_proto::{RunId, RunKind};

    use super::*;

    const ID: &str = "0f8e2d6c-3b1a-4c5d-9e7f-1a2b3c4d5e6f";

    fn params(kind: RunKind) -> RunStartParams {
        RunStartParams::new(RunId::new(ID).unwrap(), kind, "hi")
    }

    fn text(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        let at = args.iter().position(|a| a == flag)?;
        args.get(at + 1).map(String::as_str)
    }

    #[test]
    fn the_probe_loads_mcp_servers_for_one_turn_and_rlxd_denies_its_calls() {
        let argv = text(&probe_args(Path::new("/h/p.json")));
        assert_eq!(after(&argv, "--tools"), Some(""));
        assert_eq!(after(&argv, "--max-turns"), Some("1"));
        assert_eq!(after(&argv, "--settings"), Some("/h/p.json"));
        assert_eq!(after(&argv, "--setting-sources"), Some("project"));
        for present in ["-p", "--verbose", "--no-session-persistence"] {
            assert!(argv.iter().any(|a| a == present), "{present}: {argv:?}");
        }
        for absent in [
            "--strict-mcp-config",
            "--allowedTools",
            "--bare",
            "--session-id",
        ] {
            assert!(!argv.iter().any(|a| a == absent), "{absent}: {argv:?}");
        }
        let rlxd = if cfg!(windows) {
            r"C:\rlx\rlxd.exe"
        } else {
            "/opt/rlx/rlxd"
        };
        let settings = probe_settings(Path::new(rlxd)).unwrap();
        let hook = &settings["hooks"]["PreToolUse"][0];
        assert_eq!(hook["matcher"], "*");
        let command = hook["hooks"][0]["command"].as_str().unwrap();
        assert!(command.ends_with(" __probe-deny"), "{command}");
        assert!(!command.contains("rlxd-hook"), "{command}");
        assert!(probe_settings(Path::new("rlxd")).is_err());
        let deny: Value = serde_json::from_str(&probe_deny_output()).unwrap();
        assert_eq!(deny["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(
            deny["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .unwrap()
                .contains("rlxd")
        );
    }

    #[test]
    fn a_task_persists_and_mounts_no_tools_strictly() {
        let argv = text(&args(&params(RunKind::Task), 1, Path::new("/h/s.json")));
        assert_eq!(after(&argv, "--session-id"), Some(ID));
        assert_eq!(after(&argv, "--tools"), Some(""));
        assert_eq!(after(&argv, "--setting-sources"), Some("project"));
        assert_eq!(after(&argv, "--permission-mode"), Some("dontAsk"));
        assert_eq!(after(&argv, "--settings"), Some("/h/s.json"));
        assert_eq!(after(&argv, "--output-format"), Some("stream-json"));
        assert_eq!(after(&argv, "--input-format"), Some("stream-json"));
        for present in ["-p", "--verbose", "--strict-mcp-config"] {
            assert!(argv.iter().any(|a| a == present), "{present}: {argv:?}");
        }
        for absent in [
            "--no-session-persistence",
            "--allowedTools",
            "--bare",
            "--resume",
            "--include-partial-messages",
            "--json-schema",
            "--append-system-prompt",
        ] {
            assert!(!argv.iter().any(|a| a == absent), "{absent}: {argv:?}");
        }
    }

    #[test]
    fn other_kinds_are_session_less_unless_asked_and_a_resume_resumes() {
        let mut refresh = params(RunKind::WidgetRefresh);
        let first = text(&args(&refresh, 1, Path::new("s")));
        assert!(first.iter().any(|a| a == "--no-session-persistence"));
        refresh.persist_session = Some(true);
        let second = text(&args(&refresh, 2, Path::new("s")));
        assert!(!second.iter().any(|a| a == "--no-session-persistence"));
        assert_eq!(after(&second, "--resume"), Some(ID));
        assert!(!second.iter().any(|a| a == "--session-id"));
    }

    #[test]
    fn connectors_drop_built_in_tools_only_and_options_pass_through() {
        let mut task = params(RunKind::Task);
        task.mount = Mount::Connectors {
            connectors: vec!["claude.ai Slack".into()],
        };
        task.partial = true;
        task.append_system_prompt = Some("You are Nova.".into());
        task.output_schema =
            serde_json::from_str(r#"{"type":"object","required":["verdict"]}"#).ok();
        let args = text(&args(&task, 1, Path::new("s")));
        assert!(!args.iter().any(|a| a == "--strict-mcp-config"));
        assert_eq!(after(&args, "--tools"), Some(""));
        assert!(args.iter().any(|a| a == "--include-partial-messages"));
        assert_eq!(
            after(&args, "--append-system-prompt"),
            Some("You are Nova.")
        );
        let schema: Value = serde_json::from_str(after(&args, "--json-schema").unwrap()).unwrap();
        assert_eq!(schema, json!({"type": "object", "required": ["verdict"]}));
    }

    #[test]
    fn the_hook_is_absolute_quoted_when_it_must_be_and_waits_past_the_deadline() {
        let mut task = params(RunKind::Task);
        task.deadline_ms = 1000;
        let plain = settings(&task, 1, Path::new("/opt/rlx/bin/rlxd-hook"));
        let hook = &plain["hooks"]["PreToolUse"][0];
        assert_eq!(hook["matcher"], "*");
        let command = hook["hooks"][0]["command"].as_str().unwrap();
        if cfg!(windows) {
            assert!(command.starts_with('"'), "{command}");
        } else {
            assert_eq!(
                command,
                format!(
                    "/opt/rlx/bin/rlxd-hook pre-tool-use --run {ID} --attempt 1 --deadline-ms 1000"
                )
            );
        }
        assert_eq!(hook["hooks"][0]["timeout"], 11);
        task.deadline_ms = 1500;
        let spaced = settings(&task, 2, Path::new("/Users/a b/bin/rlxd-hook"));
        let command = spaced["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert_eq!(
            command,
            format!(
                "\"/Users/a b/bin/rlxd-hook\" pre-tool-use --run {ID} --attempt 2 --deadline-ms 1500"
            )
        );
        assert_eq!(spaced["hooks"]["PreToolUse"][0]["hooks"][0]["timeout"], 12);
    }

    #[test]
    fn a_path_a_shell_would_read_is_escaped() {
        assert_eq!(quoted_for("/a/$x`y\"z", false), "\"/a/\\$x\\`y\\\"z\"");
        assert_eq!(
            quoted_for("/opt/bin/rlxd-hook", false),
            "/opt/bin/rlxd-hook"
        );
        // Windows: forward slashes, always quoted, nothing escaped (hook_beside refuses a
        // path a shell would read).
        assert_eq!(
            quoted_for(r"C:\Users\a b\rlxd-hook.exe", true),
            r#""C:/Users/a b/rlxd-hook.exe""#
        );
        assert_eq!(
            quoted_for(r"C:\bin\rlxd-hook.exe", true),
            r#""C:/bin/rlxd-hook.exe""#
        );
    }

    #[test]
    fn the_hook_is_never_a_bare_name() {
        let rlxd = std::env::temp_dir().join("rlx").join("bin").join("rlxd");
        let hook = hook_beside(Some(&rlxd)).unwrap();
        assert!(hook.ends_with("rlxd-hook") || hook.ends_with("rlxd-hook.exe"));
        assert!(hook.is_absolute(), "{}", hook.display());
        assert_eq!(hook.parent(), rlxd.parent());
        assert!(hook_beside(None).is_err());
        assert!(hook_beside(Some(Path::new("rlxd"))).is_err());
        assert!(hook_beside(Some(Path::new("bin/rlxd"))).is_err());
    }
}
