//! The scenarios, one module each, and the table that names them. A scenario plays one
//! start of Claude Code and returns the code it exits with.
//!
//! To add one: a module here, and a line in [`ALL`]. Nothing else changes.

use crate::Fake;

mod abort;
mod asks;
mod big;
mod crash;
pub mod gate;
mod hello;
mod huge;
mod orphan;
mod persist;
mod print_env;
mod rate_limit;
mod slow;
mod structured;
mod tool_result;
pub mod watch_call;

pub use orphan::LINGER;

/// One scenario: plays a start, and returns its exit code.
pub type Scenario = fn(&mut Fake) -> i32;

/// Every scenario, by the name a run starts it as.
pub const ALL: &[(&str, Scenario)] = &[
    ("abort", abort::run),
    ("asks", asks::run),
    ("big", big::run),
    ("crash", crash::run),
    ("gate", gate::by_prompt),
    ("hello", hello::run),
    ("huge", huge::run),
    ("orphan", orphan::run),
    ("persist", persist::run),
    ("print-env", print_env::run),
    ("rate-limit", rate_limit::run),
    ("slow", slow::run),
    ("structured", structured::run),
    ("tool-result", tool_result::run),
    ("watch-call", watch_call::run),
];

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
