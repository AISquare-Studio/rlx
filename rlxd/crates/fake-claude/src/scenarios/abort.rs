//! `abort`: starts, says part of its answer, then dies of a signal (`SIGABRT` on macOS
//! and Linux), as a crash does: rlxd says so in `run.failed`.

use crate::Fake;

pub fn run(fake: &mut Fake) -> i32 {
    if fake.begin().is_none() {
        return super::NO_PROMPT;
    }
    fake.say("About to fall over.");
    std::process::abort()
}
