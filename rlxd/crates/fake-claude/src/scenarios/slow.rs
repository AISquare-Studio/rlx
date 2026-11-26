//! `slow`: starts, says it is working, then takes `FAKE_CLAUDE_SLOW_MS` milliseconds
//! (default 60,000) before it answers. stdin closing does not cut that short, so only
//! rlxd ending its process tree does: the cancel, timeout and engine-restart checks.

use crate::Fake;

/// How long it takes by default, in milliseconds.
pub const DEFAULT_MS: u64 = 60_000;

pub fn run(fake: &mut Fake) -> i32 {
    if fake.begin().is_none() {
        return super::NO_PROMPT;
    }
    fake.say("Working on it.");
    let ms = std::env::var("FAKE_CLAUDE_SLOW_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
        .unwrap_or(DEFAULT_MS);
    fake.sleep_ms(ms);
    fake.say("Done at last.");
    fake.succeed("Done at last.", None);
    0
}
