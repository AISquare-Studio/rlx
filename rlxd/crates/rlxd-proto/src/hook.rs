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

/// The variables rlxd-hook sends with a hook by name, and the brain sets for it (or
/// removes, when rlxd-hook had none): the allowlist's names, `aisquare.services.brain.
/// ENV_NAMES`, whose doc says why each is here. With [`HOOK_ENV_PREFIXES`] and
/// [`HOOK_ENV_SECRET_WORDS`], rlxd names it in its hello to the brain, which refuses an rlxd
/// whose lists are not its own (`env_allowlist_differs`, so `brain_incompatible` in
/// `status`). Sorted. [`hook_env_forwarded`] is the whole rule.
pub const HOOK_ENV_NAMES: &[&str] = &[
    "AISQUARE_ACCOUNTS_SHARED",
    "AISQUARE_AGENT_BIN",
    "AISQUARE_AGENT_NAME",
    "AISQUARE_API_URL",
    "AISQUARE_BOARD_CONTEXT",
    "AISQUARE_BROKER",
    "AISQUARE_BROKERED",
    "AISQUARE_BROKERED_BOARD",
    "AISQUARE_BROKERED_MEMBERS",
    "AISQUARE_BROKERED_POOL",
    "AISQUARE_BROKERED_REPORTS",
    "AISQUARE_BROKERED_ROOTS",
    "AISQUARE_BROKERED_ROOT_IDS",
    "AISQUARE_BROKERED_SEALS",
    "AISQUARE_BROKER_NONCE",
    "AISQUARE_CI",
    "AISQUARE_CI_DELIVERY_OVERRIDE",
    "AISQUARE_CI_KEY",
    "AISQUARE_CI_RUN",
    "AISQUARE_CI_URL",
    "AISQUARE_CODING_AGENT",
    "AISQUARE_DB_BUSY_MS",
    "AISQUARE_EFFORT",
    "AISQUARE_EXPERIMENTAL_CAPTAIN",
    "AISQUARE_EXPLAINABILITY_TARGET",
    "AISQUARE_FLEET_AGENT",
    "AISQUARE_HARNESS_PROBE",
    "AISQUARE_HOME",
    "AISQUARE_IN_APP",
    "AISQUARE_LAUNCH_AGENT",
    "AISQUARE_LAUNCH_ID",
    "AISQUARE_LAUNCH_PID",
    "AISQUARE_LAUNCH_SETTINGS",
    "AISQUARE_NO_TELEMETRY",
    "AISQUARE_PASTE_LOG",
    "AISQUARE_PERSONA",
    "AISQUARE_PIN_SESSION_ID",
    "AISQUARE_PIPELINE_ID",
    "AISQUARE_PLAIN_CLAUDE_CODE_TMPDIR",
    "AISQUARE_PLAIN_CLAUDE_CONFIG_DIR",
    "AISQUARE_PLAIN_CODEX_HOME",
    "AISQUARE_RELEASES_URL",
    "AISQUARE_REMOTE_AUTO_OFF",
    "AISQUARE_REMOTE_NGROK_URL",
    "AISQUARE_REMOTE_PORT",
    "AISQUARE_REPORTS",
    "AISQUARE_ROLE",
    "AISQUARE_RUN_TRACE_ID",
    "AISQUARE_SERVE_CLIENT",
    "AISQUARE_SERVE_CLOSE_AFTER",
    "AISQUARE_SERVE_PORT",
    "AISQUARE_SERVE_ROLE",
    "AISQUARE_TEAM",
    "AISQUARE_TEAM_DELTA",
    "AISQUARE_TEAM_HUB",
    "AISQUARE_TEAM_LEASE_MIN",
    "AISQUARE_TOKEN",
    "AISQUARE_TRACE_AGENT_NAME",
    "AISQUARE_WATCH_DETACHED",
    "ALL_PROXY",
    "APPDATA",
    "COLORTERM",
    "COLUMNS",
    "FORCE_COLOR",
    "HOME",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LC_TERMINAL",
    "LINES",
    "LOCALAPPDATA",
    "LOGNAME",
    "NO_COLOR",
    "NO_PROXY",
    "PATH",
    "PATHEXT",
    "RLX_HOME",
    "SSL_CERT_DIR",
    "SSL_CERT_FILE",
    "TEMP",
    "TERM",
    "TMP",
    "TMPDIR",
    "TMUX",
    "TMUX_PANE",
    "TMUX_TMPDIR",
    "TZ",
    "USER",
    "USERNAME",
    "USERPROFILE",
    "XDG_BIN_HOME",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_RUNTIME_DIR",
    "XDG_STATE_HOME",
    "all_proxy",
    "http_proxy",
    "https_proxy",
    "no_proxy",
];

/// The allowlist's prefixes: every coding agent's own variables, by the first word of its
/// home's variable (`aisquare.services.brain.ENV_PREFIXES`, which reads them off the agent
/// adapters): Claude Code's and Codex's.
pub const HOOK_ENV_PREFIXES: &[&str] = &["CLAUDE", "CODEX"];

/// The words that make an agent's variable a credential, which is never sent: its token.
pub const HOOK_ENV_SECRET_WORDS: &[&str] = &["CREDENTIALS", "KEY", "PASSWORD", "SECRET", "TOKEN"];

/// Whether rlxd-hook sends the variable `name` with a hook, and the brain sets it: one of
/// [`HOOK_ENV_NAMES`], or one under [`HOOK_ENV_PREFIXES`] none of whose words is one of
/// [`HOOK_ENV_SECRET_WORDS`]. Names are read upper-case, as Windows compares them.
pub fn hook_env_forwarded(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if HOOK_ENV_NAMES.contains(&upper.as_str()) || HOOK_ENV_NAMES.contains(&name) {
        return true;
    }
    HOOK_ENV_PREFIXES
        .iter()
        .any(|prefix| upper.starts_with(prefix))
        && !upper
            .split('_')
            .any(|word| HOOK_ENV_SECRET_WORDS.contains(&word))
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
    /// ignored.
    #[serde(default)]
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
    fn an_agents_variables_go_and_its_credentials_do_not() {
        for name in [
            "CLAUDE_CONFIG_DIR",
            "CLAUDECODE",
            "CODEX_HOME",
            "PATH",
            "Path",
            "TMUX_PANE",
        ] {
            assert!(hook_env_forwarded(name), "{name}");
        }
        for name in [
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CODEX_API_KEY",
            "ANTHROPIC_API_KEY",
            "SECRET",
            "EDITOR",
        ] {
            assert!(!hook_env_forwarded(name), "{name}");
        }
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
