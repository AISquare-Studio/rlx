//! `print-env`: answers with the app's environment it was started with (handoff 0.10.3,
//! 4.3): `CLAUDE_CONFIG_DIR`, `CLAUDE_SECURESTORAGE_CONFIG_DIR` and `PATH`, those set, as
//! a JSON object, then succeeds. Its log entry holds them too (`env`), as every start's
//! does, with the program rlxd named (`argv[0]`).

use crate::Fake;

pub fn run(fake: &mut Fake) -> i32 {
    if fake.begin().is_none() {
        return super::NO_PROMPT;
    }
    let env = serde_json::Value::Object(crate::recorded_env()).to_string();
    fake.say(&env);
    fake.succeed(&env, None);
    0
}
