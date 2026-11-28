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
pub mod mcp_accounts;
pub mod mcp_direct;
mod orphan;
mod persist;
mod print_env;
pub mod probe;
mod rate_limit;
mod slow;
mod structured;
mod tool_result;
pub mod watch_call;

pub use connectors::{ROUTE_ENV, SLACK, TRY_TOOL_ENV, tools as slack_tools};
pub use orphan::LINGER;

use connectors::Schedule;
use mcp_direct::Variant;

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
    ("connectors-slow-init", connectors::slow_init),
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
    ("mcp-accounts", |fake| mcp_accounts::play(fake, false)),
    ("mcp-accounts-old", |fake| mcp_accounts::play(fake, true)),
    ("mcp-direct", |fake| mcp_direct::play(fake, Variant::Gated)),
    ("mcp-direct-defiant", |fake| {
        mcp_direct::play(fake, Variant::Defiant)
    }),
    ("mcp-direct-garbled", |fake| {
        mcp_direct::play(fake, Variant::Garbled)
    }),
    ("mcp-direct-hang", |fake| {
        mcp_direct::play(fake, Variant::Hang)
    }),
    ("mcp-direct-huge", |fake| {
        mcp_direct::play(fake, Variant::Huge)
    }),
    ("mcp-direct-needs-auth", |fake| {
        mcp_direct::play(fake, Variant::NeedsAuth)
    }),
    ("mcp-direct-needs-auth-slow", |fake| {
        mcp_direct::play(fake, Variant::NeedsAuthSlow)
    }),
    ("mcp-direct-old", |fake| {
        mcp_direct::play(fake, Variant::Old)
    }),
    ("mcp-direct-orphan", |fake| {
        mcp_direct::play(fake, Variant::Orphan)
    }),
    ("mcp-direct-plant", |fake| {
        mcp_direct::play(fake, Variant::Plant)
    }),
    ("mcp-direct-respelled", |fake| {
        mcp_direct::play(fake, Variant::Respelled)
    }),
    ("mcp-direct-silent", |fake| {
        mcp_direct::play(fake, Variant::Silent)
    }),
    ("mcp-direct-twice", |fake| {
        mcp_direct::play(fake, Variant::Twice)
    }),
    ("mcp-direct-twice-other", |fake| {
        mcp_direct::play(fake, Variant::TwiceOther)
    }),
    ("mcp-direct-slow-ask", |fake| {
        mcp_direct::play(fake, Variant::SlowAsk)
    }),
    ("mcp-direct-slow-reply", |fake| {
        mcp_direct::play(fake, Variant::SlowReply)
    }),
    ("mcp-direct-turn", |fake| {
        mcp_direct::play(fake, Variant::Turn)
    }),
    ("mcp-direct-unasked", |fake| {
        mcp_direct::play(fake, Variant::Unasked)
    }),
    ("orphan", orphan::run),
    ("persist", persist::run),
    ("print-env", print_env::run),
    ("auth-status", hello::run),
    ("signed-out", hello::run),
    ("probe-hangs", hello::run),
    ("auth-env", hello::run),
    ("probe-slow", hello::run),
    ("probe-markers", hello::run),
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
