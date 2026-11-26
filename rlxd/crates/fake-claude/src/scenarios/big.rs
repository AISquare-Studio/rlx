//! `big`: six answers of 900 KiB of text each, so the run's log passes 4 MiB, then, after
//! `FAKE_CLAUDE_SLOW_MS` milliseconds (default none), its result: the long-log replay
//! checks of `run.events`.

use crate::Fake;

pub fn run(fake: &mut Fake) -> i32 {
    if fake.begin().is_none() {
        return super::NO_PROMPT;
    }
    for n in 0..6 {
        fake.say(&format!("{n}{}", "x".repeat(900 << 10)));
    }
    let ms = std::env::var("FAKE_CLAUDE_SLOW_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
        .unwrap_or(0);
    fake.sleep_ms(ms);
    fake.succeed("done", None);
    0
}
