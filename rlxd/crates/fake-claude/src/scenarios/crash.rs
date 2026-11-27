//! `crash`: starts, says part of its answer, then exits 1 with no result.

use crate::Fake;

/// The text it gets out before it crashes.
pub const PARTIAL: &str = "I have started on it and";

pub fn run(fake: &mut Fake) -> i32 {
    if fake.begin().is_none() {
        return super::NO_PROMPT;
    }
    fake.say(PARTIAL);
    fake.sleep_ms(200);
    eprintln!("fake-claude/crash: crashed on purpose, mid-stream");
    1
}
