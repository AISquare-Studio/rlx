//! `events.subscribe`, the request that opens the event stream (handoff 0.10.3, 7.3).

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::Value;

use crate::Data;
use crate::check::{self, Invalid, drop_null, present};

/// `events.subscribe`'s params: which topics to stream, and from where.
///
/// ```json
/// {"topics": ["run", "needs_you"], "since": 41, "follow": true}
/// ```
///
/// The stream that follows carries, in order:
///
/// 1. with `since`, every event rlxd still holds after it, oldest first. When `since` is
///    older than the oldest event rlxd holds, a `daemon.gap {from, to}` comes first,
///    whatever the topics: the events numbered `from` to `to` are gone, so the client
///    re-reads what it shows;
/// 2. with `follow`, which is the default, every later event as it happens.
///
/// Event frames carry no `id`: a connection carries at most one stream. The request's
/// reply comes last and ends the stream, `{"id": …, "ok": true, "data": {"seq": …}}`,
/// where `seq` is the last one the stream covered, a `since` to resume from. Without
/// `follow`, it comes once the replay is done; with it, when rlxd stops. A subscribe rlxd
/// refuses gets its error reply and no events.
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(title = "SubscribeParams")]
#[non_exhaustive]
pub struct SubscribeParams {
    /// The topics to stream, such as `run` and `needs_you`. Absent means every topic. A
    /// topic this rlxd does not carry gets the error `unknown_topic`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        transform = drop_null,
        length(min = 1),
        inner(regex(pattern = r"^[a-z][a-z0-9_]*$"))
    )]
    pub topics: Option<Vec<String>>,
    /// Replay the events after this `seq` first, `0` for every event rlxd holds. Absent
    /// means live events only. A `seq` this rlxd has not reached yet gets the error
    /// `invalid_params`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub since: Option<u64>,
    /// Go on with live events after the replay, until rlxd stops. `false` ends the stream
    /// once the replay is done.
    #[serde(default = "follow_by_default")]
    pub follow: bool,
}

/// The serde code of `SubscribeParams`'s fields, unchecked. It is private, so the only way
/// in or out of the type is its own `Serialize` and `Deserialize`, which run `validate()`.
/// (`remote = "Self"` would make these unchecked functions public, as
/// `SubscribeParams::serialize` and `SubscribeParams::deserialize`.)
#[derive(Serialize, Deserialize)]
#[serde(remote = "SubscribeParams")]
struct UncheckedSubscribe {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    topics: Option<Vec<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    since: Option<u64>,
    #[serde(default = "follow_by_default")]
    follow: bool,
}

fn follow_by_default() -> bool {
    true
}

impl SubscribeParams {
    /// Every topic, live events only, until rlxd stops.
    pub fn new() -> Self {
        Self {
            topics: None,
            since: None,
            follow: true,
        }
    }

    /// These params, streaming only `topics`. Refuses an empty list, and a topic that is
    /// not snake_case.
    pub fn with_topics<I, T>(mut self, topics: I) -> Result<Self, Invalid>
    where
        I: IntoIterator<Item = T>,
        T: Into<String>,
    {
        self.topics = Some(topics.into_iter().map(Into::into).collect());
        self.validate()?;
        Ok(self)
    }

    /// These params, replaying every event rlxd holds after `since` first.
    #[must_use]
    pub fn with_since(mut self, since: u64) -> Self {
        self.since = Some(since);
        self
    }

    /// These params, going on with live events after the replay (`true`), or ending the
    /// stream once it is done (`false`).
    #[must_use]
    pub fn with_follow(mut self, follow: bool) -> Self {
        self.follow = follow;
        self
    }

    /// Whether these params keep every rule of `events-subscribe.json`. Whether rlxd
    /// carries each topic is rlxd's to say.
    pub fn validate(&self) -> Result<(), Invalid> {
        let Some(topics) = &self.topics else {
            return Ok(());
        };
        if topics.is_empty() {
            return Err(Invalid::new(
                "topics",
                "must not be empty: leave it out for every topic",
            ));
        }
        for topic in topics {
            check::topic("topics", topic)?;
        }
        Ok(())
    }
}

impl Default for SubscribeParams {
    fn default() -> Self {
        Self::new()
    }
}

impl Serialize for SubscribeParams {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(serde::ser::Error::custom)?;
        UncheckedSubscribe::serialize(self, serializer)
    }
}

/// Reads an object only. Every field is optional, so serde would read a struct from an
/// array too, as `[]` or `[["run"], 41]`, which the schema refuses.
impl<'de> Deserialize<'de> for SubscribeParams {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let object = Data::deserialize(deserializer)?;
        let params =
            UncheckedSubscribe::deserialize(Value::Object(object)).map_err(de::Error::custom)?;
        params.validate().map_err(de::Error::custom)?;
        Ok(params)
    }
}
