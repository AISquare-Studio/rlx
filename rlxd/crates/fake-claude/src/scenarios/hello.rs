//! `hello`: one short answer, then a successful result.

use crate::Fake;

/// What it says, and its result.
pub const TEXT: &str = "Hello! How can I help today?";

pub fn run(fake: &mut Fake) -> i32 {
    if fake.begin().is_none() {
        return super::NO_PROMPT;
    }
    fake.say(TEXT);
    fake.succeed(TEXT, None);
    0
}
