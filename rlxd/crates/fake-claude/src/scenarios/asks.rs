//! `asks`: its first attempt ends with the output `{"status":"needs_you","question":
//! "Which channel?"}`. Resumed (`--resume`), it reads the reply from its first user
//! message and ends `{"status":"done","channel":<reply>}`. The output is the result's
//! text, and also `structured_output` under `--json-schema`.

use serde_json::{Value, json};

use crate::Fake;

/// The question its first attempt asks.
pub const QUESTION: &str = "Which channel?";

pub fn run(fake: &mut Fake) -> i32 {
    let resumed = fake.resumed();
    let Some(message) = fake.begin() else {
        return super::NO_PROMPT;
    };
    let output = if resumed {
        let reply = Fake::text_of(&message);
        fake.say(&format!("Posting to {reply}."));
        json!({"status": "done", "channel": reply})
    } else {
        fake.say("I need one thing from you first.");
        json!({"status": "needs_you", "question": QUESTION})
    };
    let structured = fake.value("--json-schema").map(|_| output.clone());
    fake.succeed(
        &output.to_string(),
        structured.filter(|v: &Value| v.is_object()),
    );
    0
}
