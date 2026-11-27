//! `structured`: with `--json-schema`, its result carries `structured_output`, the
//! recorded `structured/output.json`: the widget data path and rlxapp's `Check`.

use serde_json::Value;

use crate::Fake;

/// The recorded output, which the checks compare with.
pub const OUTPUT: &str =
    include_str!("../../../../tests/fixtures/fake-claude/structured/output.json");

pub fn run(fake: &mut Fake) -> i32 {
    if fake.begin().is_none() {
        return super::NO_PROMPT;
    }
    let output: Value = serde_json::from_str(OUTPUT).expect("output.json is JSON");
    fake.say("Here is the verdict.");
    let schema = fake
        .value("--json-schema")
        .and_then(|text| serde_json::from_str::<Value>(text).ok());
    let structured = schema.map(|_| output.clone());
    fake.succeed(&output.to_string(), structured);
    0
}
