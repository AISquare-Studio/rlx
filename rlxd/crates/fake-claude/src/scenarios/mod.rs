//! The scenarios, one module each, and the table that names them. A scenario plays one
//! start of Claude Code and returns the code it exits with.
//!
//! To add one: a module here, and a line in [`ALL`]. Nothing else changes.

use crate::Fake;

mod abort;
mod asks;
mod big;
mod connectors;
mod crash;
pub mod gate;
mod hello;
mod huge;
pub mod login;
mod orphan;
mod persist;
mod rate_limit;
mod slow;
mod structured;
mod tool_result;
pub mod watch_call;

pub use connectors::{ROUTE_ENV, SLACK, TRY_TOOL_ENV, tools as slack_tools};
pub use orphan::LINGER;

use connectors::Schedule;

/// One scenario: plays a start, and returns its exit code.
pub type Scenario = fn(&mut Fake) -> i32;

/// Every scenario, by the name a run starts it as.
pub const ALL: &[(&str, Scenario)] = &[
    ("abort", abort::run),
    ("asks", asks::run),
    ("big", big::run),
    ("connectors-late", |fake| {
        connectors::play(fake, Schedule::ConnectsAt(3000), ANSWER)
    }),
    ("connectors-later", |fake| {
        connectors::play(fake, Schedule::ConnectsAt(8000), ANSWER)
    }),
    ("connectors-later-noisy", |fake| {
        connectors::play_noisy(fake, Schedule::ConnectsAt(8000), ANSWER)
    }),
    ("connectors-needs-auth", |fake| {
        connectors::play(fake, Schedule::NeedsAuth, ANSWER)
    }),
    ("connectors-never", |fake| {
        connectors::play(fake, Schedule::Never, ANSWER)
    }),
    ("crash", crash::run),
    ("gate", gate::by_prompt),
    ("hello", hello::run),
    ("huge", huge::run),
    ("init-connectors", |fake| {
        connectors::play(fake, Schedule::ConnectsAt(0), ANSWER)
    }),
    ("init-needs-auth", |fake| {
        connectors::play(fake, Schedule::NeedsAuth, ANSWER)
    }),
    ("init-other", connectors::other),
    ("orphan", orphan::run),
    ("persist", persist::run),
    ("rate-limit", rate_limit::run),
    ("slow", slow::run),
    ("stdin-eof-empty", connectors::stdin_eof_empty),
    ("structured", structured::run),
    ("tool-result", tool_result::run),
    ("watch-call", watch_call::run),
];

/// What the connector scenarios answer.
const ANSWER: &str = "Nothing in Slack needs you.";

/// The scenario called `name`: one of [`ALL`], or one of the gate's ([`gate`]).
pub fn find(name: &str) -> Option<Scenario> {
    ALL.iter()
        .find(|(each, _)| *each == name)
        .map(|(_, scenario)| *scenario)
        .or_else(|| gate::named(name))
}

/// The code a scenario exits with when stdin closed before its first message: Claude
/// Code reads no prompt, and fails.
const NO_PROMPT: i32 = 1;
