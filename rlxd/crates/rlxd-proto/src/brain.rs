//! The brain protocol: what rlxd and its resident Python brain, `rlx brain --stdio`, say
//! to each other over the brain's stdin and stdout (handoff 0.10.3, 1.6).
//!
//! It is internal to one rlx release: no client of rlxd's socket sees it. It is still
//! versioned, so that an rlx and an rlxd of different releases fail loudly with a named
//! error, rather than misread each other's frames:
//!
//! 1. rlxd writes [`BrainInput::Hello`], with the [`BRAIN_PROTOCOL`] it speaks.
//! 2. The brain answers [`BrainOutput::Hello`] with the protocol it agrees on, or
//!    [`BrainOutput::Refused`] and exits: `incompatible_protocol`, with the protocols it
//!    speaks, or `env_allowlist_differs`, when rlxd's hook variables are not the ones it
//!    sets. rlxd likewise refuses a brain whose hello names a protocol rlxd does not
//!    speak, and its `status` then says `brain_incompatible`.
//! 3. Requests follow, one at a time: [`BrainInput::Request`]. The brain answers each
//!    with [`BrainOutput::Accepted`] as soon as it has read it, before it runs anything,
//!    then with [`BrainOutput::Reply`]. A request the brain never accepted was never run,
//!    so whoever asked may run it elsewhere; one it accepted may have run, and must not
//!    run again.
//!
//! Every frame is one JSON object on one line, tagged by `type`. The schemas are
//! `brain-input.json` (what rlxd writes) and `brain-output.json` (what the brain writes).
//! As everywhere in this crate, a reader ignores fields it does not know, so a field can
//! be added without a new protocol.

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::check::{self, Invalid, drop_null, present};
use crate::{Data, ErrorBody};

/// The brain protocol this crate speaks.
pub const BRAIN_PROTOCOL: u32 = 1;

/// Every brain protocol this crate speaks.
pub const BRAIN_PROTOCOLS: &[u32] = &[BRAIN_PROTOCOL];

/// A frame rlxd writes on the brain's stdin.
///
/// ```json
/// {"type": "hello", "protocol": 1, "rlxd_version": "0.10.3", "env_allowlist": ["…"]}
/// {"type": "request", "id": 7, "method": "ping", "params": {}}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(title = "BrainInput")]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum BrainInput {
    /// The first frame: the brain protocol rlxd speaks.
    Hello {
        /// The brain protocol rlxd speaks.
        #[schemars(range(min = 1))]
        protocol: u32,
        /// rlxd's version, which is rlx's.
        #[schemars(length(min = 1))]
        rlxd_version: String,
        /// The variables rlxd-hook sends with each hook, by exact name (`HOOK_ENV_NAMES`),
        /// sorted. The brain refuses an rlxd whose list is not the one it sets
        /// (`env_allowlist_differs`).
        #[serde(default)]
        env_allowlist: Vec<String>,
    },
    /// One request. rlxd sends the next only once this one has its reply.
    Request {
        /// The request's number, from 1, which its `accepted` and `reply` repeat.
        #[schemars(range(min = 1))]
        id: u64,
        /// What to do, such as `ping`.
        #[schemars(length(min = 1))]
        method: String,
        /// The method's arguments.
        #[serde(default)]
        params: Data,
    },
}

/// The serde code of `BrainInput`, unchecked. Private, so the only way in or out of the
/// type is its own `Serialize` and `Deserialize`, which run `validate()`.
#[derive(Serialize, Deserialize)]
#[serde(remote = "BrainInput", tag = "type", rename_all = "snake_case")]
enum UncheckedInput {
    Hello {
        protocol: u32,
        rlxd_version: String,
        #[serde(default)]
        env_allowlist: Vec<String>,
    },
    Request {
        id: u64,
        method: String,
        #[serde(default)]
        params: Data,
    },
}

/// A frame the brain writes on its stdout.
///
/// ```json
/// {"type": "hello", "protocol": 1, "pid": 4243, "rlx_version": "0.10.3"}
/// {"type": "accepted", "id": 7}
/// {"type": "reply", "id": 7, "ok": true, "result": {"pid": 4243}}
/// {"type": "reply", "id": 8, "ok": false, "error": {"code": "unknown_method", "message": "…"}}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(title = "BrainOutput")]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum BrainOutput {
    /// The brain's answer to rlxd's hello: the protocol they agree on, and who it is.
    Hello {
        /// The brain protocol the brain agrees on: one rlxd's hello named.
        #[schemars(range(min = 1))]
        protocol: u32,
        /// The brain's process id, from 1 to 2147483647.
        #[schemars(range(min = 1, max = 2147483647))]
        pid: u32,
        /// The version of the rlx running the brain.
        #[schemars(length(min = 1))]
        rlx_version: String,
    },
    /// The brain will not serve this rlxd, and exits: `incompatible_protocol` when they
    /// share no protocol.
    Refused {
        /// Why, as a code, such as `incompatible_protocol`.
        #[schemars(length(min = 1))]
        code: String,
        /// Why, for a person.
        #[schemars(length(min = 1))]
        message: String,
        /// The brain protocols the brain speaks.
        #[serde(default)]
        supported: Vec<u32>,
    },
    /// The brain has read request `id` and is about to run it. Nothing of it ran before
    /// this frame.
    Accepted {
        /// The request's `id`.
        #[schemars(range(min = 1))]
        id: u64,
    },
    /// The answer to request `id`: `result` when `ok`, else `error`.
    Reply {
        /// The request's `id`.
        #[schemars(range(min = 1))]
        id: u64,
        /// Whether the request succeeded.
        ok: bool,
        /// What the method returned, when `ok`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(transform = drop_null)]
        result: Option<Data>,
        /// Why it failed, when not `ok`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(transform = drop_null)]
        error: Option<ErrorBody>,
    },
}

/// The serde code of `BrainOutput`, unchecked. Private, as [`UncheckedInput`] is.
#[derive(Serialize, Deserialize)]
#[serde(remote = "BrainOutput", tag = "type", rename_all = "snake_case")]
enum UncheckedOutput {
    Hello {
        protocol: u32,
        pid: u32,
        rlx_version: String,
    },
    Refused {
        code: String,
        message: String,
        #[serde(default)]
        supported: Vec<u32>,
    },
    Accepted {
        id: u64,
    },
    Reply {
        id: u64,
        ok: bool,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "present"
        )]
        result: Option<Data>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "present"
        )]
        error: Option<ErrorBody>,
    },
}

/// The codes `status.brain.error` says, and the brain's own reply codes.
pub mod codes {
    /// No `rlx` to run as the brain was found: neither beside rlxd, nor in the install
    /// folders, nor on `PATH` (or `RLXD_RLX_BIN` names no program).
    pub const BRAIN_NOT_FOUND: &str = "brain_not_found";
    /// The brain could not be started at all, such as a program that is not executable.
    pub const BRAIN_START_FAILED: &str = "brain_start_failed";
    /// The brain and rlxd share no brain protocol: their releases differ. `detail` says
    /// which protocols each speaks.
    pub const BRAIN_INCOMPATIBLE: &str = "brain_incompatible";
    /// The brain exited, or was killed, and rlxd starts it again after `backoff_ms`.
    pub const BRAIN_EXITED: &str = "brain_exited";
    /// The brain said nothing rlxd could read in time, or something that breaks the
    /// protocol, and was stopped.
    pub const BRAIN_UNRESPONSIVE: &str = "brain_unresponsive";
    /// A request the brain accepted failed inside it.
    pub const BRAIN_FAILED: &str = "brain_failed";
}

fn owned(list: &[&str]) -> Vec<String> {
    list.iter().map(|&item| item.to_owned()).collect()
}

impl BrainInput {
    /// rlxd's hello, speaking [`BRAIN_PROTOCOL`] and naming the hook variables
    /// ([`hook_env_forwarded`](crate::hook_env_forwarded)'s list).
    pub fn hello(rlxd_version: impl Into<String>) -> Result<Self, Invalid> {
        let hello = Self::Hello {
            protocol: BRAIN_PROTOCOL,
            rlxd_version: rlxd_version.into(),
            env_allowlist: owned(crate::HOOK_ENV_NAMES),
        };
        hello.validate()?;
        Ok(hello)
    }

    /// Request `id`, calling `method` with `params`.
    pub fn request(id: u64, method: impl Into<String>, params: Data) -> Result<Self, Invalid> {
        let request = Self::Request {
            id,
            method: method.into(),
            params,
        };
        request.validate()?;
        Ok(request)
    }

    /// Whether this frame keeps every rule of `brain-input.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        match self {
            Self::Hello {
                protocol,
                rlxd_version,
                ..
            } => {
                check::at_least("protocol", u64::from(*protocol), 1)?;
                check::non_empty("rlxd_version", rlxd_version)
            }
            Self::Request { id, method, .. } => {
                check::at_least("id", *id, 1)?;
                check::non_empty("method", method)
            }
        }
    }
}

impl BrainOutput {
    /// Whether this frame keeps every rule of `brain-output.json`, and a reply says
    /// either its result or its error.
    pub fn validate(&self) -> Result<(), Invalid> {
        match self {
            Self::Hello {
                protocol,
                pid,
                rlx_version,
            } => {
                check::at_least("protocol", u64::from(*protocol), 1)?;
                check::pid("pid", *pid)?;
                check::non_empty("rlx_version", rlx_version)
            }
            Self::Refused { code, message, .. } => {
                check::non_empty("code", code)?;
                check::non_empty("message", message)
            }
            Self::Accepted { id } => check::at_least("id", *id, 1),
            Self::Reply {
                id,
                ok,
                result,
                error,
            } => {
                check::at_least("id", *id, 1)?;
                match (ok, result, error) {
                    (true, Some(_), None) | (false, None, Some(_)) => Ok(()),
                    (true, _, _) => Err(Invalid::new(
                        "result",
                        "an ok reply carries its result, and no error",
                    )),
                    (false, _, _) => Err(Invalid::new(
                        "error",
                        "a failed reply carries its error, and no result",
                    )),
                }
            }
        }
    }
}

impl Serialize for BrainInput {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(serde::ser::Error::custom)?;
        UncheckedInput::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for BrainInput {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let frame = UncheckedInput::deserialize(deserializer)?;
        frame.validate().map_err(de::Error::custom)?;
        Ok(frame)
    }
}

impl Serialize for BrainOutput {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(serde::ser::Error::custom)?;
        UncheckedOutput::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for BrainOutput {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let frame = UncheckedOutput::deserialize(deserializer)?;
        frame.validate().map_err(de::Error::custom)?;
        Ok(frame)
    }
}
