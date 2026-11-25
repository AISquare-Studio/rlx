//! What the types enforce, so that they and the schemas hold every document to the same
//! rules: the error a rule raises, the rules themselves, and two serde and schemars helpers
//! for optional fields.

use std::fmt;

use schemars::Schema;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// A document, or a value about to become one, that breaks a rule of its schema.
///
/// [`Invalid::field`] names the field, as a dotted path such as `error.detail` or
/// `data.run_seq`, and [`Invalid::problem`] says what is wrong with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invalid {
    field: String,
    problem: String,
}

impl Invalid {
    pub(crate) fn new(field: impl Into<String>, problem: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            problem: problem.into(),
        }
    }

    /// The field that breaks the rule, as a dotted path.
    pub fn field(&self) -> &str {
        &self.field
    }

    /// What is wrong with it.
    pub fn problem(&self) -> &str {
        &self.problem
    }

    /// This error, for a field inside `outer`: `error` and `code` give `error.code`.
    pub(crate) fn within(self, outer: &str) -> Self {
        Self::new(format!("{outer}.{}", self.field), self.problem)
    }
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.field, self.problem)
    }
}

impl std::error::Error for Invalid {}

pub(crate) fn non_empty(field: &str, value: &str) -> Result<(), Invalid> {
    if value.is_empty() {
        return Err(Invalid::new(field, "must not be empty"));
    }
    Ok(())
}

pub(crate) fn at_least(field: &str, value: u64, min: u64) -> Result<(), Invalid> {
    if value < min {
        return Err(Invalid::new(
            field,
            format!("must be at least {min}, not {value}"),
        ));
    }
    Ok(())
}

/// One snake_case name: a lowercase letter, then lowercase letters, digits and `_`.
pub(crate) const SNAKE: &str = "[a-z][a-z0-9_]*";

fn is_snake(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_lowercase())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// An event's topic: one snake_case name, such as `run` or `needs_you`.
pub(crate) fn topic(field: &str, value: &str) -> Result<(), Invalid> {
    if is_snake(value) {
        return Ok(());
    }
    Err(Invalid::new(
        field,
        format!("{value:?} is not a snake_case topic, such as run or needs_you"),
    ))
}

/// An event's type: `<topic>.<event>`, the event one or more snake_case names joined by
/// `.`, such as `run.text` or `connector.login.open_url`.
pub(crate) fn event_type(field: &str, value: &str, topic: &str) -> Result<(), Invalid> {
    let event = value
        .strip_prefix(topic)
        .and_then(|rest| rest.strip_prefix('.'));
    match event {
        Some(event) if event.split('.').all(is_snake) => Ok(()),
        _ => Err(Invalid::new(
            field,
            format!(
                "{value:?} is not {topic}.<event>, with the event in snake_case, such as \
                 {topic}.started"
            ),
        )),
    }
}

/// RFC 3339's `date-time`, as JSON Schema's `format: date-time` takes it:
/// `2026-11-25T12:00:00.000Z`, or with an offset such as `+00:00`. Second `60`, a leap
/// second, is refused: check-jsonschema refuses every one and the jsonschema crate
/// accepts some, so `59` is the bound both agree on.
pub(crate) fn date_time(field: &str, value: &str) -> Result<(), Invalid> {
    if is_date_time(value.as_bytes()) {
        Ok(())
    } else {
        Err(Invalid::new(
            field,
            format!("{value:?} is not an RFC 3339 date-time, such as 2026-11-25T12:00:00.000Z"),
        ))
    }
}

fn is_date_time(b: &[u8]) -> bool {
    let num = |at: usize, len: usize| -> Option<u32> {
        let digits = b.get(at..at + len)?;
        digits.iter().try_fold(0, |n, &d| {
            d.is_ascii_digit().then(|| n * 10 + u32::from(d - b'0'))
        })
    };
    let at = |i: usize, c: &[u8]| b.get(i).is_some_and(|x| c.contains(x));
    let (Some(year), Some(month), Some(day)) = (num(0, 4), num(5, 2), num(8, 2)) else {
        return false;
    };
    let (Some(hour), Some(minute), Some(second)) = (num(11, 2), num(14, 2), num(17, 2)) else {
        return false;
    };
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    if !(at(4, b"-") && at(7, b"-") && at(10, b"Tt") && at(13, b":") && at(16, b":"))
        || !(1..=days).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return false;
    }
    let mut i = 19;
    if at(i, b".") {
        let start = i + 1;
        i = start;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    match b.get(i..) {
        Some(b"Z" | b"z") => true,
        Some(offset) if offset.len() == 6 && matches!(offset[0], b'+' | b'-') => {
            offset[3] == b':'
                && num(i + 1, 2).is_some_and(|h| h <= 23)
                && num(i + 4, 2).is_some_and(|m| m <= 59)
        }
        _ => false,
    }
}

/// For an optional field: absent is `None` (with `#[serde(default)]`), and `null` is
/// refused, as the schema refuses it.
pub(crate) fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

/// For an optional field's schema: the field may be absent, but not `null`. Takes
/// `null` out of the schema schemars derives for an `Option`, so the schema still comes
/// from the field's real type.
pub(crate) fn drop_null(schema: &mut Schema) {
    if let Some(Value::Array(types)) = schema.get_mut("type") {
        types.retain(|t| t != "null");
        if let [only] = types.as_slice() {
            let only = only.clone();
            schema.insert("type".to_owned(), only);
        }
    }
}
