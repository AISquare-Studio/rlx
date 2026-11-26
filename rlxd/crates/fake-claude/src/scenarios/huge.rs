//! `huge`: more than `run.done` carries: a successful result of 1.5 MiB of text, or, under
//! `--json-schema`, a short result whose `structured_output` is 1.5 MiB of JSON. rlxd
//! ends the run `failed` with a named reason rather than lose its end.

use serde_json::json;

use crate::Fake;

pub fn run(fake: &mut Fake) -> i32 {
    if fake.begin().is_none() {
        return super::NO_PROMPT;
    }
    fake.say("Here comes a lot.");
    let lot = "x".repeat(3 << 19);
    if fake.value("--json-schema").is_some() {
        fake.succeed("ok", Some(json!({"verdict": lot})));
    } else {
        fake.succeed(&lot, None);
    }
    0
}
