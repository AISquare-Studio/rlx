//! `persist`: `hello`, which also writes its session file where Claude Code does, unless
//! the run passes `--no-session-persistence` (every scenario does; this one is named for
//! the erase check, 2.5).

use crate::Fake;

pub fn run(fake: &mut Fake) -> i32 {
    super::hello::run(fake)
}
