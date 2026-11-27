//! Hooks over the socket (handoff 0.10.3, 1.5): what rlxd-hook sends rlxd for one of
//! Claude Code's hooks, what rlxd answers, and the `hook.*` event each publishes (7.3).
//!
//! `rlxd-hook <event>` reads the hook's JSON on stdin and sends `hook` ([`HookParams`]):
//! the bytes, its working directory, and the variables it has that
//! [`hook_env_forwarded`] allows. For
//! the six fleet events rlxd asks the resident brain (1.6), which runs `rlx hook <event>`'s
//! own code as that process would, and answers [`HookReply`]: the bytes for rlxd-hook to
//! write on its stdout and stderr. `pre-tool-use` is the gate's (5.1): rlxd answers it
//! itself, and until the gate decides tool calls it denies every one
//! ([`deny_output`]). Each `hook` that reaches rlxd publishes one `hook.<event>`
//! ([`HookEventData`]) on the `hook` topic.
//!
//! A hook rlxd could not hand to the brain gets [`codes::HOOK_NOT_DELIVERED`]: it never ran,
//! and rlxd-hook runs `rlx hook <event>` itself. Once the brain accepted it, a failure is
//! [`codes::HOOK_FAILED`] and an overrun [`codes::HOOK_TIMED_OUT`]: it may have run, and
//! rlxd-hook runs nothing else.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::check::{self, Invalid, drop_null, present};

/// Claude Code's hooks rlxd-hook takes, as its command line names them: the six fleet
/// events `rlx hook` handles, then the gate's `pre-tool-use`.
pub const HOOK_EVENTS: &[&str] = &[
    "session-start",
    "user-prompt-submit",
    "session-end",
    "stop",
    "stop-failure",
    "notification",
    PRE_TOOL_USE,
];

/// The gate's hook (5.1), which never falls back to `rlx hook`.
pub const PRE_TOOL_USE: &str = "pre-tool-use";

/// The variables rlxd-hook sends with a hook, by exact name, and the brain sets for it (or
/// removes, when rlxd-hook had none): `aisquare.services.brain.ENV_NAMES`, whose doc says
/// why each is here. A coding agent's own come from its adapter (`AgentAdapter.hook_env`),
/// and none is a credential. rlxd names the list in its hello to the brain, which refuses an
/// rlxd whose list is not its own (`env_allowlist_differs`, so `brain_incompatible` in
/// `status`). Sorted. [`hook_env_forwarded`] is the whole rule.
pub const HOOK_ENV_NAMES: &[&str] = &[
    "AISQUARE_BOARD_CONTEXT",
    "AISQUARE_BROKER",
    "AISQUARE_BROKERED",
    "AISQUARE_CI",
    "AISQUARE_DB_BUSY_MS",
    "AISQUARE_EXPERIMENTAL_CAPTAIN",
    "AISQUARE_FLEET_AGENT",
    "AISQUARE_HOME",
    "AISQUARE_LAUNCH_ID",
    "AISQUARE_LAUNCH_PID",
    "AISQUARE_PERSONA",
    "AISQUARE_PIPELINE_ID",
    "AISQUARE_ROLE",
    "AISQUARE_TEAM",
    "AISQUARE_TEAM_DELTA",
    "AISQUARE_TEAM_HUB",
    "AISQUARE_TEAM_LEASE_MIN",
    "APPDATA",
    "CLAUDE_CODE_TMPDIR",
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_EFFORT",
    "CLAUDE_PID",
    "HOME",
    "LC_CTYPE",
    "LOCALAPPDATA",
    "PATH",
    "PYTHONIOENCODING",
    "PYTHONUTF8",
    "RLX_HOME",
    "TEMP",
    "TMP",
    "TMPDIR",
    "TMUX",
    "TMUX_PANE",
    "TZ",
    "USERNAME",
    "USERPROFILE",
];

/// The longest value of a variable rlxd-hook sends with a hook, in bytes. No path, zone or
/// id is longer; a hook with a longer one is `rlx hook`'s, which reads it itself.
pub const HOOK_ENV_VALUE_MOST: usize = 32 << 10;

/// The longest `hook` request rlxd-hook sends, its line's bytes: the longest frame rlxd
/// reads (its `server::MAX_FRAME`). A hook whose request would be longer (its stdin, in
/// base64, with its variables) is `rlx hook`'s, run before anything is sent.
pub const HOOK_FRAME_MOST: usize = 1 << 20;

/// Whether rlxd-hook sends the variable `name` with a hook, and the brain sets it: one of
/// [`HOOK_ENV_NAMES`], by its exact name, which Windows compares upper-case. Nothing else
/// is, whatever it is called.
pub fn hook_env_forwarded(name: &str) -> bool {
    HOOK_ENV_NAMES.contains(&name) || HOOK_ENV_NAMES.contains(&name.to_ascii_uppercase().as_str())
}

/// The `hook.*` event type of a hook named as rlxd-hook's command line names it:
/// `session-start` is `hook.session_start`.
pub fn event_type(event: &str) -> Option<String> {
    HOOK_EVENTS
        .contains(&event)
        .then(|| format!("hook.{}", event.replace('-', "_")))
}

/// What a `pre-tool-use` hook prints to deny the call, with `reason`: Claude Code's
/// `PreToolUse` reply.
pub fn deny_output(reason: &str) -> String {
    let reply = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    });
    format!("{reply}\n")
}

/// Whether `stdout` is a `pre-tool-use` deny and nothing else: one JSON document that is
/// [`deny_output`]'s for some reason, whitespace around it aside. Until the gate decides
/// tool calls (5.1), rlxd-hook prints rlxd's reply only when it is one; any other (empty,
/// text, `{}`, an allow, an ask, a deny with more in it) is denied as unreached, so no reply
/// lets a call through.
pub fn is_deny_output(stdout: &[u8]) -> bool {
    let Ok(said) = serde_json::from_slice::<serde_json::Value>(stdout) else {
        return false;
    };
    let Some(reason) = said
        .pointer("/hookSpecificOutput/permissionDecisionReason")
        .and_then(serde_json::Value::as_str)
    else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(&deny_output(reason)).is_ok_and(|deny| deny == said)
}

/// `hook`'s params: one hook, as rlxd-hook read it.
///
/// ```json
/// {"event": "session-start", "stdin_b64": "eyJzZXNzaW9uX2lkIjoidDEifQ==",
///  "cwd": "/Users/me/project", "env": {"AISQUARE_ROLE": "coder"},
///  "accept_ms": 5000, "deadline_ms": 117000}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(title = "HookParams")]
#[non_exhaustive]
pub struct HookParams {
    /// The hook, as rlxd-hook's command line names it: `session-start`,
    /// `user-prompt-submit`, `session-end`, `stop`, `stop-failure`, `notification` or
    /// `pre-tool-use`.
    #[schemars(regex(pattern = r"^[a-z]+(-[a-z]+)*$"))]
    pub event: String,
    /// The hook's stdin, Claude Code's JSON, as bytes in base64.
    pub stdin_b64: String,
    /// rlxd-hook's working directory, which Claude Code chose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, length(min = 1))]
    pub cwd: Option<String>,
    /// The variables of the allowlist rlxd-hook has, by name. A name off the list is
    /// ignored. No value is longer than [`HOOK_ENV_VALUE_MOST`] (32 KiB).
    #[serde(default)]
    #[schemars(extend("additionalProperties" = {"type": "string", "maxLength": 32768}))]
    pub env: BTreeMap<String, String>,
    /// How long the brain may take to accept the hook, in milliseconds, before it is
    /// withdrawn unrun (`hook_not_delivered`). Absent is 5000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, range(min = 1))]
    pub accept_ms: Option<u64>,
    /// How long the whole hook may take, in milliseconds, after which rlxd answers
    /// `hook_timeout` and starts the brain again. Absent is 50000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, range(min = 1))]
    pub deadline_ms: Option<u64>,
    /// `pre-tool-use --run`: the run whose tool call this is (5.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, length(min = 1))]
    pub run_id: Option<String>,
    /// `pre-tool-use --attempt`: the run's attempt (5.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, range(min = 1))]
    pub attempt: Option<u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "HookParams")]
struct UncheckedParams {
    event: String,
    stdin_b64: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    cwd: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    accept_ms: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    deadline_ms: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    run_id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    attempt: Option<u64>,
}

/// `hook`'s reply: what rlxd-hook writes, byte for byte, and how the hook ended.
///
/// ```json
/// {"stdout_b64": "c2VlZCBmYWN0Cg==", "stderr_b64": "", "exit_code": 0}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "HookReply")]
#[non_exhaustive]
pub struct HookReply {
    /// The bytes for rlxd-hook's stdout, in base64: what `rlx hook <event>` would print,
    /// which Claude Code reads as the hook's reply.
    pub stdout_b64: String,
    /// The bytes for rlxd-hook's stderr, in base64.
    pub stderr_b64: String,
    /// How `rlx hook <event>` ended. rlxd-hook exits 0 whatever it says, as every hook does.
    pub exit_code: i32,
}

/// The `data` of a `hook.*` event (7.3): whose hook it was, where, and for which run.
///
/// ```json
/// {"session_id": "t1", "cwd": "/Users/me/project"}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(title = "HookEventData")]
#[non_exhaustive]
pub struct HookEventData {
    /// Claude Code's session id, from the hook's JSON; empty when it named none.
    pub session_id: String,
    /// The session's working directory: the hook JSON's `cwd`, else rlxd-hook's; empty
    /// when neither is known.
    pub cwd: String,
    /// The run the hook belongs to, for a run's `pre-tool-use` (5.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, length(min = 1))]
    pub run_id: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "HookEventData")]
struct UncheckedEventData {
    session_id: String,
    cwd: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    run_id: Option<String>,
}

/// The codes a `hook` call fails with.
pub mod codes {
    /// rlxd did not hand the hook to the brain, so nothing of it ran: no brain runs, it
    /// was starting, or it did not accept the hook in time. rlxd-hook runs `rlx hook
    /// <event>` itself.
    pub const HOOK_NOT_DELIVERED: &str = "hook_not_delivered";
    /// The brain accepted the hook, then failed: it may have run.
    pub const HOOK_FAILED: &str = "hook_failed";
    /// The brain accepted the hook and did not finish it by its deadline: it may have
    /// run. rlxd starts the brain again.
    pub const HOOK_TIMED_OUT: &str = "hook_timeout";
}

impl HookParams {
    /// A hook `event` with `stdin` (its bytes, in base64), run from `cwd` with `env`.
    /// Refuses an event rlxd-hook does not take, and `stdin_b64` that is not base64.
    pub fn new(
        event: impl Into<String>,
        stdin_b64: impl Into<String>,
        cwd: Option<String>,
        env: BTreeMap<String, String>,
    ) -> Result<Self, Invalid> {
        let params = Self {
            event: event.into(),
            stdin_b64: stdin_b64.into(),
            cwd,
            env,
            accept_ms: None,
            deadline_ms: None,
            run_id: None,
            attempt: None,
        };
        params.validate()?;
        Ok(params)
    }

    /// These params, with the brain to accept the hook within `accept_ms` and finish it
    /// within `deadline_ms`.
    pub fn with_times(mut self, accept_ms: u64, deadline_ms: u64) -> Result<Self, Invalid> {
        self.accept_ms = Some(accept_ms);
        self.deadline_ms = Some(deadline_ms);
        self.validate()?;
        Ok(self)
    }

    /// These params, for a run's `pre-tool-use` (5.1).
    pub fn with_run(mut self, run_id: impl Into<String>, attempt: u64) -> Result<Self, Invalid> {
        self.run_id = Some(run_id.into());
        self.attempt = Some(attempt);
        self.validate()?;
        Ok(self)
    }

    /// Whether these params keep every rule of `hook.json`, and name a hook rlxd-hook
    /// takes.
    pub fn validate(&self) -> Result<(), Invalid> {
        if !HOOK_EVENTS.contains(&self.event.as_str()) {
            return Err(Invalid::new(
                "event",
                format!("{:?} is not one of {HOOK_EVENTS:?}", self.event),
            ));
        }
        if crate::b64::decode(&self.stdin_b64).is_none() {
            return Err(Invalid::new("stdin_b64", "is not base64"));
        }
        if let Some(cwd) = &self.cwd {
            check::non_empty("cwd", cwd)?;
        }
        if let Some((name, value)) = self
            .env
            .iter()
            .find(|(_, value)| value.len() > HOOK_ENV_VALUE_MOST)
        {
            return Err(Invalid::new(
                "env",
                format!(
                    "{name} is {} bytes, more than {HOOK_ENV_VALUE_MOST}",
                    value.len()
                ),
            ));
        }
        for (field, value) in [
            ("accept_ms", self.accept_ms),
            ("deadline_ms", self.deadline_ms),
        ] {
            if let Some(value) = value {
                check::at_least(field, value, 1)?;
            }
        }
        if let Some(run_id) = &self.run_id {
            check::non_empty("run_id", run_id)?;
        }
        if let Some(attempt) = self.attempt {
            check::at_least("attempt", attempt, 1)?;
        }
        Ok(())
    }
}

impl HookEventData {
    /// The event of a hook of session `session_id`, in `cwd`.
    pub fn new(session_id: impl Into<String>, cwd: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            cwd: cwd.into(),
            run_id: None,
        }
    }

    /// This event, of run `run_id`. Refuses an empty one.
    pub fn with_run(mut self, run_id: impl Into<String>) -> Result<Self, Invalid> {
        self.run_id = Some(run_id.into());
        self.validate()?;
        Ok(self)
    }

    /// Whether this event's data keeps every rule of `hook-event.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        if let Some(run_id) = &self.run_id {
            check::non_empty("run_id", run_id)?;
        }
        Ok(())
    }
}

impl Serialize for HookParams {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(serde::ser::Error::custom)?;
        UncheckedParams::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for HookParams {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let params = UncheckedParams::deserialize(deserializer)?;
        params.validate().map_err(de::Error::custom)?;
        Ok(params)
    }
}

impl Serialize for HookEventData {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(serde::ser::Error::custom)?;
        UncheckedEventData::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for HookEventData {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let data = UncheckedEventData::deserialize(deserializer)?;
        data.validate().map_err(de::Error::custom)?;
        Ok(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_hook_has_its_event_type() {
        assert_eq!(
            event_type("session-start").as_deref(),
            Some("hook.session_start")
        );
        assert_eq!(
            event_type("pre-tool-use").as_deref(),
            Some("hook.pre_tool_use")
        );
        assert_eq!(event_type("codex"), None);
        assert_eq!(HOOK_EVENTS.len(), 7);
    }

    #[test]
    fn only_the_listed_names_go_and_no_credential_does() {
        for name in [
            "CLAUDE_CONFIG_DIR",
            "CLAUDE_EFFORT",
            "AISQUARE_ROLE",
            "PATH",
            "Path",
            "TMUX_PANE",
        ] {
            assert!(hook_env_forwarded(name), "{name}");
        }
        // What the prefix rule let through (review #2555, F5), and what it never did.
        for name in [
            "CLAUDE_CODE_AUTH",
            "CLAUDE_CODE_SESSION_COOKIE",
            "CODEX_ACCESS_TOKENS",
            "CLAUDE_CODE_APIKEY",
            "CLAUDE_CODE_PASSPHRASE",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDECODE",
            "CODEX_HOME",
            "CODEX_API_KEY",
            "ANTHROPIC_API_KEY",
            "SECRET",
            "EDITOR",
            "AISQUARE_ROLE_X",
            "claude_config",
        ] {
            assert!(!hook_env_forwarded(name), "{name}");
        }
    }

    #[test]
    fn only_a_deny_is_a_deny() {
        assert!(is_deny_output(deny_output("no gate yet").as_bytes()));
        assert!(is_deny_output(
            br#" {"hookSpecificOutput": {"permissionDecision": "deny", "hookEventName": "PreToolUse", "permissionDecisionReason": ""}} "#
        ));
        for said in [
            &b""[..],
            b"\n",
            b"just some text\n",
            b"{}",
            b"null",
            b"[]",
            br#"{"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "allow", "permissionDecisionReason": "r"}}"#,
            br#"{"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "ask", "permissionDecisionReason": "r"}}"#,
            br#"{"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny"}}"#,
            br#"{"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": 3}}"#,
            br#"{"hookSpecificOutput": {"hookEventName": "PostToolUse", "permissionDecision": "deny", "permissionDecisionReason": "r"}}"#,
            br#"{"decision": "approve", "hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": "r"}}"#,
            br#"{"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": "r", "updatedInput": {}}}"#,
        ] {
            assert!(!is_deny_output(said), "{}", String::from_utf8_lossy(said));
        }
        let twice = format!("{}{}", deny_output("r"), deny_output("r"));
        assert!(!is_deny_output(twice.as_bytes()));
    }

    #[test]
    fn the_allowlist_is_sorted_and_once_each() {
        let mut sorted = HOOK_ENV_NAMES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted, HOOK_ENV_NAMES);
    }

    #[test]
    fn a_deny_is_claude_codes_pre_tool_use_reply() {
        let printed = deny_output("no gate yet");
        assert!(printed.ends_with('\n'));
        let reply: serde_json::Value = serde_json::from_str(&printed).unwrap();
        assert_eq!(reply["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(reply["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(
            reply["hookSpecificOutput"]["permissionDecisionReason"],
            "no gate yet"
        );
    }
}
