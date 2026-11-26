//! `rate-limit`: a `rate_limit_event` warning that the five-hour limit is near, an hour
//! before it resets, then an answer.

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
    let event = json!({"type": "rate_limit_event", "rate_limit_info": {
        "status": "allowed_warning", "resetsAt": now + 3600, "rateLimitType": "five_hour",
        "utilization": 0.91, "isUsingOverage": false},
        "uuid": uuid(), "session_id": fake.session_id()});
    fake.emit(&event);
    fake.say("Still within the limit.");
    fake.succeed("Still within the limit.", None);
    0
}
