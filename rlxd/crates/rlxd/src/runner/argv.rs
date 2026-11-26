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
/// that runs `rlxd-hook pre-tool-use --run <run_id> --attempt <n>` by the absolute path
/// `hook`, with a timeout of `deadline_ms / 1000 + 10` seconds (rounded up), so Claude
/// Code never ends the hook before rlxd's own deadline.
pub fn settings(params: &RunStartParams, attempt: u64, hook: &Path) -> Value {
    let command = format!(
        "{} pre-tool-use --run {} --attempt {attempt}",
        quoted(hook),
        params.run_id
    );
    let timeout = params.deadline_ms.div_ceil(1000) + 10;
    json!({"hooks": {"PreToolUse": [{"matcher": "*", "hooks": [
        {"type": "command", "command": command, "timeout": timeout}
    ]}]}})
}

/// `path` as a shell word: as it is when it holds only characters no shell treats
/// specially, else in double quotes, with `"`, `\`, `$` and `` ` `` escaped. Claude Code
/// runs a hook's command through a shell. On Windows it is always quoted.
fn quoted(path: &Path) -> String {
    let text = path.to_string_lossy();
    let plain = !cfg!(windows)
        && !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:@%,=".contains(c));
    if plain {
        return text.into_owned();
    }
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for c in text.chars() {
        if matches!(c, '"' | '\\' | '$' | '`') && !(cfg!(windows) && c == '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

/// `rlxd-hook` beside this rlxd: its absolute path, whether or not it is there yet.
pub fn hook_beside(rlxd: Option<&Path>) -> PathBuf {
    let name = if cfg!(windows) {
        "rlxd-hook.exe"
    } else {
        "rlxd-hook"
    };
    match rlxd.and_then(Path::parent) {
        Some(dir) => dir.join(name),
        None => PathBuf::from(name),
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
                format!("/opt/rlx/bin/rlxd-hook pre-tool-use --run {ID} --attempt 1")
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
            format!("\"/Users/a b/bin/rlxd-hook\" pre-tool-use --run {ID} --attempt 2")
        );
        assert_eq!(spaced["hooks"]["PreToolUse"][0]["hooks"][0]["timeout"], 12);
    }

    #[test]
    fn a_path_a_shell_would_read_is_escaped() {
        if cfg!(windows) {
            return;
        }
        assert_eq!(quoted(Path::new("/a/$x`y\"z")), "\"/a/\\$x\\`y\\\"z\"");
    }
}
