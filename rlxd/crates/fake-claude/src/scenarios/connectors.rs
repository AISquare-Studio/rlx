//! What the connector scenarios share (handoff 0.10.3, 5.5 and 7.7): one connector,
//! `claude.ai Slack`, whose status follows the fixture clock from the run's first start,
//! as an account's connectors warming up would. It doesn't start again with each try.
//!
//! As Claude Code does, a start writes `system/init` after the first user message, with
//! each server's status at that moment; a connected server's tools are mounted, others'
//! are not. With `RLXD_TEST_CONNECTOR_ROUTE=1`, the Claude Code it plays reports states
//! before the first message too (route 1): a `system/init` as it starts and another at
//! each change, until the message comes.
//!
//! `connected_at`, in the fixture log, is when this start saw Slack connected: the time
//! it connected, or this start's own start when it was connected already.
//!
//! `stdin-eof-empty` is Claude Code as E9 found it: when stdin is at EOF by the time it
//! writes `system/init`, it lists no servers and mounts no connector tools; held open, it
//! lists Slack connected.

use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use serde_json::{Value, json};

use crate::Fake;

/// The one connector, as the handoff's checks name it.
pub const SLACK: &str = "claude.ai Slack";

/// The variable that makes the scenarios play route-1 Claude Code, as rlxd's test build
/// reads it to pick the route.
pub const ROUTE_ENV: &str = "RLXD_TEST_CONNECTOR_ROUTE";

/// A variable naming a tool the scenarios call once Slack is connected, before they
/// answer: its PreToolUse hooks decide it ([`Fake::pre_tool_use`]).
pub const TRY_TOOL_ENV: &str = "FAKE_CLAUDE_TRY_TOOL";

/// Slack's tools, made up, as `init-connectors/expected-tools.json` lists them.
pub fn tools() -> Vec<String> {
    serde_json::from_str(include_str!(
        "../../../../tests/fixtures/fake-claude/init-connectors/expected-tools.json"
    ))
    .expect("expected-tools.json is a list of names")
}

/// How Slack's status goes, on the fixture clock.
#[derive(Clone, Copy, Debug)]
pub enum Schedule {
    /// `pending`, then `connected` from this many milliseconds on.
    ConnectsAt(u64),
    /// `pending` throughout.
    Never,
    /// `needs-auth` throughout: its sign-in has lapsed.
    NeedsAuth,
}

impl Schedule {
    /// Slack's status at `now`.
    fn status_at(self, now: u64) -> &'static str {
        match self {
            Self::ConnectsAt(at) if now >= at => "connected",
            Self::ConnectsAt(_) | Self::Never => "pending",
            Self::NeedsAuth => "needs-auth",
        }
    }

    /// When the status next changes, after `now`.
    fn next_change(self, now: u64) -> Option<u64> {
        match self {
            Self::ConnectsAt(at) if now < at => Some(at),
            _ => None,
        }
    }
}

/// Whether this start plays route-1 Claude Code.
fn route_1() -> bool {
    std::env::var(ROUTE_ENV).is_ok_and(|route| route == "1")
}

/// The fixture clock now: this start's log's, or 0 with no log.
fn now(fake: &Fake) -> u64 {
    fake.log().map_or(0, crate::Log::now)
}

/// Writes `system/init` with Slack in `status`, its tools mounted when connected, and
/// logs `connected_at` the first time this start sees it connected.
fn init(fake: &mut Fake, schedule: Schedule, status: &str) {
    let tools = if status == "connected" {
        tools()
    } else {
        Vec::new()
    };
    let servers = json!([{"name": SLACK, "status": status, "source": "claudeai"}]);
    fake.init(json!(tools), servers);
    let started_at = fake.started_at();
    if status == "connected"
        && let Some(log) = fake.log()
    {
        let at = match schedule {
            Schedule::ConnectsAt(at) => at.max(started_at),
            Schedule::Never | Schedule::NeedsAuth => log.now(),
        };
        log.update(|start| {
            if start["connected_at"].is_null() {
                start["connected_at"] = Value::from(at);
            }
        });
    }
}

/// Plays a start of a connector scenario: the states before the first message on route
/// 1, then the first message, `system/init`, and an answer. On a miss (Slack not
/// connected), it answers after a moment, as Claude Code does without the connector;
/// rlxd has ended the start by then.
pub fn play(fake: &mut Fake, schedule: Schedule, answer: &str) -> i32 {
    let message = if route_1() {
        let mut said = schedule.status_at(now(fake));
        init(fake, schedule, said);
        loop {
            let wait = schedule
                .next_change(now(fake))
                .map_or(Duration::from_secs(3600), |at| {
                    Duration::from_millis(at.saturating_sub(now(fake)))
                });
            match fake.next_user_message_within(wait) {
                Ok(Some(message)) => break message,
                Ok(None) => return super::NO_PROMPT,
                Err(RecvTimeoutError::Timeout) => {
                    let status = schedule.status_at(now(fake));
                    if status != said {
                        said = status;
                        init(fake, schedule, status);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return super::NO_PROMPT,
            }
        }
    } else {
        match fake.next_user_message() {
            Some(message) => message,
            None => return super::NO_PROMPT,
        }
    };
    fake.remember(&message);
    let status = schedule.status_at(now(fake));
    init(fake, schedule, status);
    if status != "connected" {
        fake.sleep_ms(1500);
    } else if let Ok(tool) = std::env::var(TRY_TOOL_ENV) {
        fake.pre_tool_use(&tool, &json!({"channel": "probe", "text": "probe"}));
    }
    fake.say(answer);
    fake.succeed(answer, None);
    0
}

/// `stdin-eof-empty`: Slack is connected while stdin stays open; with stdin at EOF by
/// `system/init`, none is listed (E9).
pub fn stdin_eof_empty(fake: &mut Fake) -> i32 {
    let schedule = Schedule::ConnectsAt(0);
    if route_1() {
        init(fake, schedule, "connected");
    }
    let Some(message) = fake.next_user_message() else {
        return super::NO_PROMPT;
    };
    fake.remember(&message);
    if fake.stdin_closed_within(300) {
        fake.init(json!([]), json!([]));
    } else {
        init(fake, schedule, "connected");
    }
    fake.say(ANSWER_EOF);
    fake.succeed(ANSWER_EOF, None);
    0
}

/// What `stdin-eof-empty` answers.
const ANSWER_EOF: &str = "Nothing in Slack needs you.";
