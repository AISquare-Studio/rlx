//! `orphan`: starts a process of its own that would outlive it (as an MCP server or a
//! tool's background job can), answers, and exits with that process still running. Its
//! command line names this scenario, so `pgrep -f fake-claude/orphan` finds it; the run's
//! end must take it too.

use std::process::{Command, Stdio};

use crate::Fake;

/// The argument the left-behind process is started with.
pub const LINGER: &str = "--fake-claude-linger";

pub fn run(fake: &mut Fake) -> i32 {
    if fake.begin().is_none() {
        return super::NO_PROMPT;
    }
    let me = std::env::args_os().next().unwrap_or_default();
    let started = Command::new(me)
        .arg(LINGER)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Err(e) = started {
        eprintln!("fake-claude/orphan: the lingering process did not start: {e}");
        return 1;
    }
    fake.say("Started something that lingers.");
    fake.succeed("Started something that lingers.", None);
    0
}
