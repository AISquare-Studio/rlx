//! `rate-limit`: a `rate_limit_event` saying the five-hour limit is hit (`rejected`), five
//! minutes before it resets (as 7.6's rate-limit check has it), then an answer.
//! `FAKE_CLAUDE_RATE_STATUS` gives another status, such as `allowed_warning`: a limit
//! only near, which holds no job.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

use crate::{Fake, uuid};

pub fn run(fake: &mut Fake) -> i32 {
    if fake.begin().is_none() {
        return super::NO_PROMPT;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let status = std::env::var("FAKE_CLAUDE_RATE_STATUS")
        .ok()
        .filter(|status| !status.is_empty())
        .unwrap_or_else(|| "rejected".to_owned());
    let event = json!({"type": "rate_limit_event", "rate_limit_info": {
        "status": status, "resetsAt": now + 300, "rateLimitType": "five_hour",
        "utilization": 0.91, "isUsingOverage": false},
        "uuid": uuid(), "session_id": fake.session_id()});
    fake.emit(&event);
    fake.say("Still within the limit.");
    fake.succeed("Still within the limit.", None);
    0
}
