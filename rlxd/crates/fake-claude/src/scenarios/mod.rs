//! The scenarios, one module each, and the table that names them. A scenario plays one
//! start of Claude Code and returns the code it exits with.
//!
//! To add one: a module here, and a line in [`ALL`]. Nothing else changes.

use crate::Fake;

mod asks;
mod crash;
mod hello;
mod persist;
mod rate_limit;
mod slow;
mod structured;
mod tool_result;

/// One scenario: plays a start, and returns its exit code.
pub type Scenario = fn(&mut Fake) -> i32;

/// Every scenario, by the name a run starts it as.
pub const ALL: &[(&str, Scenario)] = &[
    ("asks", asks::run),
    ("crash", crash::run),
    ("hello", hello::run),
    ("persist", persist::run),
    ("rate-limit", rate_limit::run),
    ("slow", slow::run),
    ("structured", structured::run),
    ("tool-result", tool_result::run),
];

/// The scenario called `name`.
pub fn find(name: &str) -> Option<Scenario> {
    ALL.iter()
        .find(|(each, _)| *each == name)
        .map(|(_, scenario)| *scenario)
}

/// The code a scenario exits with when stdin closed before its first message: Claude
/// Code reads no prompt, and fails.
const NO_PROMPT: i32 = 1;
