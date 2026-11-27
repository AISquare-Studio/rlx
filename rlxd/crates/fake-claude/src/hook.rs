//! The run's PreToolUse hook, run as Claude Code runs it (handoff 0.10.3, 5.1): the
//! command the run's `--settings` names, with the call as JSON on its stdin, killed after
//! the hook's `timeout`. Its stdout's `hookSpecificOutput.permissionDecision` decides: only
//! `allow` lets the tool run (under `--permission-mode dontAsk`, a hook that says nothing
//! leaves the call denied).
//!
//! The command is split into words here, double quotes kept together, and run with no
//! shell, as every fake-claude scenario runs with none: rlxd writes it plain, so the words
//! are the same ones a shell would make.
//!
//! Each answer is added to the start's `hook_replies` in the fixture log: its label, the
//! call, the decision and the reason. Never the hook's key.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::Fake;

/// The variable that holds the attempt's hook key, as rlxd names it.
pub const HOOK_KEY_ENV: &str = "RLXD_HOOK_KEY";

/// What the hook answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    /// `allow`, `deny`, `none` (no decision on stdout) or `timeout`.
    pub decision: String,
    /// The hook's `permissionDecisionReason`, or what went wrong.
    pub reason: String,
    /// What the hook printed, as JSON: `null` when it printed none.
    pub output: Value,
}

impl Answer {
    /// Whether the call may run: an explicit allow only.
    pub fn allowed(&self) -> bool {
        self.decision == "allow"
    }
}

/// A hook request made other than as Claude Code would make it, from inside the run.
#[derive(Clone, Debug)]
pub enum Tamper {
    /// As Claude Code makes it.
    None,
    /// With this key in place of the attempt's.
    Key(String),
    /// Naming this attempt in place of the run's.
    Attempt(u64),
    /// Naming this run in place of its own.
    Run(String),
}

impl Fake {
    /// Asks the run's hook about call `call_id` of `tool` with `input`, as Claude Code
    /// does before it runs a tool, and logs the answer under `label`.
    pub fn ask_hook(&mut self, label: &str, call_id: &str, tool: &str, input: &Value) -> Answer {
        self.ask_hook_as(label, call_id, tool, input, &Tamper::None)
    }

    /// [`Fake::ask_hook`], the request changed as `tamper` says.
    pub fn ask_hook_as(
        &mut self,
        label: &str,
        call_id: &str,
        tool: &str,
        input: &Value,
        tamper: &Tamper,
    ) -> Answer {
        let stdin = self.hook_input(call_id, tool, input);
        let answer = match self.hook_command() {
            Ok((words, timeout)) => run(&tampered(words, tamper), tamper, &stdin, timeout),
            Err(why) => Answer {
                decision: "none".to_owned(),
                reason: why,
                output: Value::Null,
            },
        };
        self.log_answer(label, call_id, tool, &answer);
        answer
    }

    /// Asks the hook about each of `calls` (`call_id`, tool, input) at once, a hook process
    /// each, as Claude Code asks about the calls of one message: the answers, in order.
    pub fn ask_hooks_together(
        &mut self,
        label: &str,
        calls: &[(&str, &str, Value)],
    ) -> Vec<Answer> {
        let asking: Vec<thread::JoinHandle<Answer>> = calls
            .iter()
            .map(|(call_id, tool, input)| {
                let stdin = self.hook_input(call_id, tool, input);
                let command = self.hook_command();
                thread::spawn(move || match command {
                    Ok((words, timeout)) => run(&words, &Tamper::None, &stdin, timeout),
                    Err(why) => Answer {
                        decision: "none".to_owned(),
                        reason: why,
                        output: Value::Null,
                    },
                })
            })
            .collect();
        let answers: Vec<Answer> = asking
            .into_iter()
            .map(|asked| {
                asked.join().unwrap_or_else(|_| Answer {
                    decision: "none".to_owned(),
                    reason: "the hook's thread panicked".to_owned(),
                    output: Value::Null,
                })
            })
            .collect();
        for ((call_id, tool, _), answer) in calls.iter().zip(&answers) {
            self.log_answer(label, call_id, tool, answer);
        }
        answers
    }

    /// The hook's stdin for a call: Claude Code's PreToolUse input.
    fn hook_input(&self, call_id: &str, tool: &str, input: &Value) -> Value {
        json!({
            "session_id": self.session_id(), "transcript_path": "", "cwd": self.cwd,
            "permission_mode": self.value("--permission-mode").unwrap_or("default"),
            "hook_event_name": "PreToolUse", "tool_name": tool, "tool_input": input,
            "tool_use_id": call_id,
        })
    }

    /// Adds the hook's `answer` about a call to `hook_replies`: what the hook printed
    /// (`hookSpecificOutput`), with the call and the decision read from it.
    fn log_answer(&self, label: &str, call_id: &str, tool: &str, answer: &Answer) {
        if let Some(log) = self.log() {
            let at = log.now();
            let mut reply = match &answer.output {
                Value::Object(output) => Value::Object(output.clone()),
                _ => json!({}),
            };
            for (name, value) in [
                ("at", json!(at)),
                ("label", json!(label)),
                ("call_id", json!(call_id)),
                ("tool", json!(tool)),
                ("decision", json!(answer.decision)),
                ("reason", json!(answer.reason)),
            ] {
                reply[name] = value;
            }
            log.update(|start| {
                if let Some(replies) = start["hook_replies"].as_array_mut() {
                    replies.push(reply);
                }
            });
        }
    }

    /// Counts one tool run in the fixture log.
    pub fn ran_tool(&self) {
        if let Some(log) = self.log() {
            log.update(|start| {
                let runs = start["tool_runs"].as_u64().unwrap_or(0);
                start["tool_runs"] = Value::from(runs + 1);
            });
        }
    }

    /// The PreToolUse hook's command, as words, and its timeout: from `--settings`.
    fn hook_command(&self) -> Result<(Vec<String>, Duration), String> {
        let path = self
            .value("--settings")
            .ok_or_else(|| "no --settings".to_owned())?;
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        let settings: Value = serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
        let hook = &settings["hooks"]["PreToolUse"][0]["hooks"][0];
        let command = hook["command"]
            .as_str()
            .ok_or_else(|| "no PreToolUse hook command".to_owned())?;
        let timeout = Duration::from_secs(hook["timeout"].as_u64().unwrap_or(60));
        Ok((words(command), timeout))
    }
}

/// `command` split as a shell splits it: at spaces, outside double quotes, in which `\`
/// escapes the next character (macOS and Linux) or nothing (Windows).
fn words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut started = false;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '\\' if quoted && !cfg!(windows) => {
                if let Some(next) = chars.next() {
                    word.push(next);
                }
            }
            ' ' if !quoted => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}

/// Starts `command` as Claude Code starts a hook (Node's `detached: true`): on macOS and
/// Linux in a session and process group of its own (`setsid`), on Windows detached from
/// the console in a process group of its own, so only the hook's ancestry ties it to the
/// run.
#[cfg(unix)]
#[allow(unsafe_code)]
fn detached(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid(2) is async-signal-safe, and the child runs nothing else before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(windows)]
fn detached(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

/// `words`, with the run or the attempt changed as `tamper` says.
fn tampered(mut words: Vec<String>, tamper: &Tamper) -> Vec<String> {
    let (flag, value) = match tamper {
        Tamper::Attempt(attempt) => ("--attempt", attempt.to_string()),
        Tamper::Run(run) => ("--run", run.clone()),
        Tamper::None | Tamper::Key(_) => return words,
    };
    if let Some(at) = words.iter().position(|word| word == flag)
        && let Some(slot) = words.get_mut(at + 1)
    {
        *slot = value;
    }
    words
}

/// Runs the hook's `words` with `stdin`, for at most `timeout`, and reads its answer.
fn run(words: &[String], tamper: &Tamper, stdin: &Value, timeout: Duration) -> Answer {
    let none = |reason: String| Answer {
        decision: "none".to_owned(),
        reason,
        output: Value::Null,
    };
    let Some((program, args)) = words.split_first() else {
        return none("an empty hook command".to_owned());
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Tamper::Key(key) = tamper {
        command.env(HOOK_KEY_ENV, key);
    }
    detached(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return none(format!("the hook did not start: {e}")),
    };
    if let Some(mut input) = child.stdin.take() {
        let _ = input.write_all(stdin.to_string().as_bytes());
    }
    let mut stdout = child.stdout.take();
    let reading = thread::spawn(move || {
        let mut text = String::new();
        if let Some(stdout) = stdout.as_mut() {
            let _ = stdout.read_to_string(&mut text);
        }
        text
    });
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Answer {
                    decision: "timeout".to_owned(),
                    reason: format!("the hook took more than {}s", timeout.as_secs()),
                    output: Value::Null,
                };
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(e) => return none(format!("the hook could not be waited on: {e}")),
        }
    }
    let text = reading.join().unwrap_or_default();
    let output = serde_json::from_str::<Value>(text.trim()).unwrap_or(Value::Null);
    let decided = &output["hookSpecificOutput"];
    match decided["permissionDecision"].as_str() {
        Some(decision) => Answer {
            decision: decision.to_owned(),
            reason: decided["permissionDecisionReason"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            output: output.clone(),
        },
        None => none(format!("the hook decided nothing: {:?}", text.trim())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_splits_as_a_shell_splits_it() {
        assert_eq!(
            words("/a/rlxd __test-hook pre-tool-use --run R --attempt 2"),
            [
                "/a/rlxd",
                "__test-hook",
                "pre-tool-use",
                "--run",
                "R",
                "--attempt",
                "2"
            ]
        );
        assert_eq!(
            words("\"/a b/rlxd-hook\" pre-tool-use"),
            ["/a b/rlxd-hook", "pre-tool-use"]
        );
        let tampered = tampered(words("h --run R --attempt 1"), &Tamper::Attempt(7));
        assert_eq!(tampered, ["h", "--run", "R", "--attempt", "7"]);
    }
}
