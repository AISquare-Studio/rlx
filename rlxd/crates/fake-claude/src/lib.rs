//! fake-claude: a stand-in for Claude Code, so rlxd's runner checks run in CI on three
//! OSes with no model turn (handoff 0.10.3, "How to read the checks").
//!
//! # Which scenario
//!
//! One binary plays every scenario. It is picked by the name the binary is started as:
//! rlxd's tests link (or copy) the binary to `<dir>/fake-claude/<scenario>` (`.exe` on
//! Windows) and point `RLXD_CLAUDE_BIN` there. So the process's command line names its
//! scenario, as `pgrep -f fake-claude/slow` expects, and no shell, Python or batch file
//! stands in between on any OS. Started as `fake-claude` itself, the first argument
//! `--scenario=<name>` names it.
//!
//! Each scenario is one module under `scenarios/` and one line of
//! [`scenarios::ALL`]. A later item adds its own scenarios there without touching these.
//!
//! # What every scenario does, as Claude Code does
//!
//! - `--version` prints `2.1.314 (Claude Code)`, and `auth status --json` says it is
//!   signed in with claude.ai as `probe@example.com` on the `pro` plan.
//! - `-p --output-format stream-json` without `--verbose` exits 1, saying why (rlxapp
//!   architecture, E10).
//! - With `--input-format stream-json`, it waits for the first user message on stdin,
//!   then writes `system/init` and the scenario's stream-json, one JSON object a line.
//! - `--include-partial-messages` adds `stream_event` text deltas before each assistant
//!   message.
//! - Unless `--no-session-persistence`, it writes its session file where Claude Code
//!   does: `<config dir>/projects/<cwd, each character but a letter or digit as '-'>/
//!   <session id>.jsonl`, the config dir being `CLAUDE_CONFIG_DIR`, else `~/.claude`.
//!   The session id is `--session-id`'s, or `--resume`'s.
//! - Its stream also carries a type and fields Claude Code may add later, which rlxd
//!   must ignore.
//!
//! # The fixture log
//!
//! Each start adds an entry to `$AISQUARE_HOME/fake-claude.log.json`, in the handoff's
//! shape, and keeps it current as it goes:
//!
//! ```json
//! {"starts": [{"argv": ["…"], "env": {"CLAUDE_CONFIG_DIR": "…"},
//!              "stdin": [{"at": 812, "type": "user"}], "stdin_eof_at": null,
//!              "connected_at": null, "delivered": true, "hook_replies": [],
//!              "tool_runs": 0, "exit": 0}]}
//! ```
//!
//! - `argv` is the whole command line, the program as rlxd named it first.
//! - `env` holds `CLAUDE_CONFIG_DIR`, `CLAUDE_SECURESTORAGE_CONFIG_DIR` and `PATH`, those
//!   that are set.
//! - Times are milliseconds since the first start in this home, one clock across every
//!   start: its origin is kept in `$AISQUARE_HOME/fake-claude.clock`.
//! - `stdin_eof_at` is when stdin closed while the scenario still ran: an early close. A
//!   close after the scenario wrote its `result`, which is how a run ends, is not one.
//! - `delivered` is whether the scenario got to write its `result`; a start that was
//!   killed or crashed first stays `false`.
//! - `connected_at` belongs to the connector scenarios (5.5). `hook_replies` holds each
//!   answer the run's PreToolUse hook gave a gate scenario (5.1, [`hook`]), and
//!   `tool_runs` counts the tool calls a start ran: only on an allow.
//! - `exit` is the code it exited with, `null` while it runs or once it was killed.
//!
//! Every write takes `fake-claude.log.lock` (an OS file lock), reads the log, changes
//! this start's entry, and replaces the file whole, so concurrent starts never lose each
//! other's entries and a reader never sees half a file.

use std::ffi::OsString;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

pub mod hook;
mod log;
pub mod scenarios;

pub use log::Log;

/// The Claude Code version every scenario claims.
pub const VERSION: &str = "2.1.314";

/// The model every scenario claims.
pub const MODEL: &str = "claude-sonnet-4-5-20250929";

/// `fake-claude`, as its name or first argument says.
pub fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args_os()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let (name, args) = scenario_of(&argv);
    if args.iter().any(|arg| arg == scenarios::LINGER) {
        // The process `orphan` leaves behind: it lingers a minute.
        std::thread::sleep(std::time::Duration::from_secs(60));
        return ExitCode::SUCCESS;
    }
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["--version"] => {
            println!("{VERSION} (Claude Code)");
            return ExitCode::SUCCESS;
        }
        ["auth", "status", "--json"] => {
            println!(
                "{}",
                json!({"loggedIn": true, "authMethod": "claude.ai", "apiProvider": "firstParty",
                    "email": "probe@example.com", "orgId": "00000000-0000-4000-8000-000000000000",
                    "orgName": "Probe Org", "subscriptionType": "pro"})
            );
            return ExitCode::SUCCESS;
        }
        _ => {}
    }
    let Some(scenario) = scenarios::find(&name) else {
        eprintln!(
            "fake-claude: no scenario {name:?}; the scenarios are: {}",
            scenarios::ALL
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>()
                .join(", ")
        );
        return ExitCode::from(2);
    };
    let mut fake = Fake::start(argv, args);
    if fake.flag("--output-format") && fake.value("--output-format") == Some("stream-json") {
        if !fake.flag("--verbose") {
            eprintln!("Error: When using --print, --output-format=stream-json requires --verbose");
            return fake.exit(1);
        }
    } else {
        eprintln!("fake-claude/{name}: plays only -p --output-format stream-json");
        return fake.exit(2);
    }
    let code = scenario(&mut fake);
    fake.exit(code)
}

/// The scenario's name and the arguments after the program: the program's file name
/// without its extension, or `--scenario=<name>` for the plain binary.
fn scenario_of(argv: &[String]) -> (String, Vec<String>) {
    let program = argv.first().map(PathBuf::from).unwrap_or_default();
    let stem = program
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let args = argv.get(1..).unwrap_or_default().to_vec();
    if stem == "fake-claude"
        && let Some(name) = args.first().and_then(|a| a.strip_prefix("--scenario="))
    {
        return (name.to_owned(), args[1..].to_vec());
    }
    (stem, args)
}

/// One start of a scenario: its arguments, its stdin, its stdout and its log entry.
pub struct Fake {
    args: Vec<String>,
    log: Option<Log>,
    input: Receiver<Option<Value>>,
    finished: Arc<Mutex<bool>>,
    session_id: String,
    cwd: String,
    /// The session file's lines so far, when the session persists.
    session: Vec<Value>,
}

impl Fake {
    /// Starts reading stdin, and logs this start.
    fn start(argv: Vec<String>, args: Vec<String>) -> Self {
        let log = Log::open(&argv);
        let finished = Arc::new(Mutex::new(false));
        let (sender, input) = mpsc::channel();
        {
            let log = log.clone();
            let finished = Arc::clone(&finished);
            thread::spawn(move || read_stdin(&sender, log.as_ref(), &finished));
        }
        let cwd = std::env::current_dir()
            .map(|dir| dir.display().to_string())
            .unwrap_or_default();
        let mut fake = Self {
            args,
            log,
            input,
            finished,
            session_id: String::new(),
            cwd,
            session: Vec::new(),
        };
        fake.session_id = fake
            .value("--resume")
            .or_else(|| fake.value("--session-id"))
            .unwrap_or("00000000-0000-4000-8000-000000000000")
            .to_owned();
        fake
    }

    /// Whether the command line has `name`.
    pub fn flag(&self, name: &str) -> bool {
        self.args.iter().any(|arg| arg == name)
    }

    /// The argument after `name`, if the command line has it.
    pub fn value(&self, name: &str) -> Option<&str> {
        let at = self.args.iter().position(|arg| arg == name)?;
        self.args.get(at + 1).map(String::as_str)
    }

    /// Whether this start resumes a session (`--resume`).
    pub fn resumed(&self) -> bool {
        self.flag("--resume")
    }

    /// The session id: `--session-id`'s, or `--resume`'s.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The fixture log, when `AISQUARE_HOME` names a home.
    pub fn log(&self) -> Option<&Log> {
        self.log.as_ref()
    }

    /// The next user message on stdin, waiting for it: `None` once stdin has closed.
    pub fn next_user_message(&mut self) -> Option<Value> {
        self.input.recv().ok().flatten()
    }

    /// The text of a stream-json user message: its content string, or its text blocks.
    pub fn text_of(message: &Value) -> String {
        match &message["message"]["content"] {
            Value::String(text) => text.clone(),
            Value::Array(blocks) => blocks
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join(""),
            _ => String::new(),
        }
    }

    /// Writes one stream-json line and flushes it.
    pub fn emit(&mut self, line: &Value) {
        let mut out = io::stdout().lock();
        // A closed stdout ends the run anyway; the scenario goes on to its end.
        let _ = writeln!(out, "{line}").and_then(|()| out.flush());
    }

    /// Waits until the run's first user message, then writes `system/init`: that
    /// message, or `None` when stdin closed with none.
    pub fn begin(&mut self) -> Option<Value> {
        self.begin_with(json!([]), json!([]))
    }

    /// [`Fake::begin`], `system/init` listing `tools` and `mcp_servers`.
    pub fn begin_with(&mut self, tools: Value, mcp_servers: Value) -> Option<Value> {
        let message = self.next_user_message()?;
        self.remember(&message);
        let strict = self.flag("--strict-mcp-config");
        let init = json!({
            "type": "system", "subtype": "init", "cwd": self.cwd,
            "session_id": self.session_id, "tools": tools, "mcp_servers": mcp_servers,
            "model": MODEL, "permissionMode": self.value("--permission-mode").unwrap_or("default"),
            "slash_commands": [], "apiKeySource": "none", "claude_code_version": VERSION,
            "output_style": "default", "agents": [], "skills": [], "plugins": [],
            "uuid": uuid(), "fake_strict_mcp_config": strict,
        });
        self.emit(&init);
        // Something a later Claude Code may add, which rlxd ignores.
        self.emit(
            &json!({"type": "fake_future_event", "subtype": "probe", "uuid": uuid(),
            "session_id": self.session_id}),
        );
        Some(message)
    }

    /// Writes an assistant text message, with its text deltas first under
    /// `--include-partial-messages`.
    pub fn say(&mut self, text: &str) {
        if self.flag("--include-partial-messages") {
            self.emit(
                &json!({"type": "stream_event", "event": {"type": "message_start"},
                "session_id": self.session_id, "parent_tool_use_id": null, "uuid": uuid()}),
            );
            let chars: Vec<char> = text.chars().collect();
            for chunk in chars.chunks(chars.len().div_ceil(3).max(1)) {
                let delta: String = chunk.iter().collect();
                self.emit(&json!({"type": "stream_event", "event": {
                    "type": "content_block_delta", "index": 0,
                    "delta": {"type": "text_delta", "text": delta}},
                    "session_id": self.session_id, "parent_tool_use_id": null,
                    "uuid": uuid()}));
            }
        }
        let message = json!({"type": "assistant", "message": {
            "id": format!("msg_{}", &uuid()[..8]), "type": "message", "role": "assistant",
            "model": MODEL, "content": [{"type": "text", "text": text}],
            "stop_reason": null, "usage": {"input_tokens": 12, "output_tokens": 7},
            "fake_new_field": true},
            "parent_tool_use_id": null, "session_id": self.session_id, "uuid": uuid()});
        self.remember(&message);
        self.emit(&message);
    }

    /// Writes the run's `result`, a success, after marking this start delivered. With
    /// `structured`, the result carries it as `structured_output`.
    pub fn succeed(&mut self, result: &str, structured: Option<Value>) {
        let mut line = json!({
            "type": "result", "subtype": "success", "is_error": false,
            "duration_ms": 1234, "duration_api_ms": 1000, "num_turns": 1,
            "result": result, "session_id": self.session_id, "total_cost_usd": 0.0123,
            "usage": {"input_tokens": 12, "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0, "output_tokens": 7,
                "server_tool_use": {"web_search_requests": 0}},
            "modelUsage": {}, "permission_denials": [], "uuid": uuid(),
        });
        if let Some(structured) = structured {
            line["structured_output"] = structured;
        }
        self.persist();
        self.mark_finished();
        self.emit(&line);
    }

    /// Records that the scenario has written its last line: from here, stdin closing is
    /// the run's end, not an early close.
    pub fn mark_finished(&mut self) {
        *self
            .finished
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        if let Some(log) = &self.log {
            log.update(|start| start["delivered"] = Value::Bool(true));
        }
    }

    /// Sleeps `ms` milliseconds. stdin closing meanwhile does not cut it short: like a
    /// turn in progress, it goes on.
    pub fn sleep_ms(&self, ms: u64) {
        thread::sleep(Duration::from_millis(ms));
    }

    /// Keeps `line` for the session file.
    pub fn remember(&mut self, line: &Value) {
        self.session.push(line.clone());
    }

    /// Writes the session file, unless `--no-session-persistence`.
    fn persist(&mut self) {
        if self.flag("--no-session-persistence") {
            return;
        }
        let Some(dir) = config_dir() else {
            return;
        };
        let project = dir.join("projects").join(encode_project(&self.cwd));
        let path = project.join(format!("{}.jsonl", self.session_id));
        let mut text = String::new();
        for line in &self.session {
            text.push_str(&line.to_string());
            text.push('\n');
        }
        let written = std::fs::create_dir_all(&project).and_then(|()| {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            file.write_all(text.as_bytes())
        });
        if let Err(e) = written {
            eprintln!("fake-claude: cannot write {}: {e}", path.display());
        }
        self.session.clear();
    }

    /// Records the exit code, and exits with it.
    fn exit(&self, code: i32) -> ExitCode {
        if let Some(log) = &self.log {
            log.update(|start| start["exit"] = Value::from(code));
        }
        ExitCode::from(u8::try_from(code).unwrap_or(1))
    }
}

/// Claude Code's config directory: `CLAUDE_CONFIG_DIR`, else `~/.claude`.
fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    std::env::home_dir().map(|home| home.join(".claude"))
}

/// The project folder name Claude Code gives a working directory: each character that is
/// not an ASCII letter or digit becomes `-`.
pub fn encode_project(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Reads stdin's stream-json lines, logging each, and hands the user messages over.
fn read_stdin(sender: &mpsc::Sender<Option<Value>>, log: Option<&Log>, finished: &Mutex<bool>) {
    let stdin = io::stdin();
    let mut lines = stdin.lock();
    let mut line = String::new();
    loop {
        line.clear();
        match lines.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let Ok(message) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let kind = message["type"].as_str().unwrap_or("").to_owned();
        if let Some(log) = log {
            let at = log.now();
            log.update(|start| {
                if let Some(stdin) = start["stdin"].as_array_mut() {
                    stdin.push(json!({"at": at, "type": kind}));
                }
            });
        }
        if kind == "user" && sender.send(Some(message)).is_err() {
            return;
        }
    }
    let done = *finished
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !done && let Some(log) = log {
        let at = log.now();
        log.update(|start| start["stdin_eof_at"] = Value::from(at));
    }
    let _ = sender.send(None);
}

/// A random-enough version-4 UUID for a line's `uuid`: nothing reads it.
pub fn uuid() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let mix = nanos ^ (u128::from(std::process::id()) << 64);
    let hex = format!("{mix:032x}");
    format!(
        "{}-{}-4{}-8{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    )
}

/// What `argv` looks like to the log: each argument as text.
fn printable(argv: &[String]) -> Vec<Value> {
    argv.iter().map(|arg| Value::String(arg.clone())).collect()
}

/// The variables the log records, those that are set.
fn recorded_env() -> serde_json::Map<String, Value> {
    [
        "CLAUDE_CONFIG_DIR",
        "CLAUDE_SECURESTORAGE_CONFIG_DIR",
        "PATH",
    ]
    .into_iter()
    .filter_map(|name| {
        let value: OsString = std::env::var_os(name)?;
        Some((
            name.to_owned(),
            Value::String(value.to_string_lossy().into_owned()),
        ))
    })
    .collect()
}
