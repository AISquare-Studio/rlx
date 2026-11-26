//! `abort`: starts, says part of its answer, waits `FAKE_CLAUDE_ABORT_AFTER_MS`
//! milliseconds (default 0), then dies of a signal (`SIGABRT` on macOS and Linux), as a
//! crash does: rlxd says so in `run.failed`.

use crate::Fake;

pub fn run(fake: &mut Fake) -> i32 {
    if fake.begin().is_none() {
        return super::NO_PROMPT;
    }
    fake.say("About to fall over.");
    let ms = std::env::var("FAKE_CLAUDE_ABORT_AFTER_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
        .unwrap_or(0);
    fake.sleep_ms(ms);
    std::process::abort()
}
