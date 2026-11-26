//! `huge`: a successful result of 1.5 MiB of text, more than `run.done` carries: rlxd
//! ends the run `failed` with a named reason rather than lose its end.

use crate::Fake;

pub fn run(fake: &mut Fake) -> i32 {
    if fake.begin().is_none() {
        return super::NO_PROMPT;
    }
    fake.say("Here comes a lot.");
    let result = "x".repeat(3 << 19);
    fake.succeed(&result, None);
    0
}
