//! The environment a PTY's process starts with: rlxd's own, as the fleet's tmux server
//! gives a window its global environment, less what ties a process to one session, board
//! or agent (#2574 major 3).
//!
//! The fleet takes the same out before it starts a tmux server, in core/spawn.py's
//! `tmux_client_env`: `untraced_env` (the tracing identity, [`IDENTITY`], and the
//! [`BROKERED`] scope), less [`TEAM`], [`AGENT`] and, from an agent's tool shell, that
//! agent's home ([`HOMES`]). rlxd also takes out [`TERMINAL`]: what tmux sets for its
//! panes, which names the server the starter sat in, or the terminal it ran in. A
//! `pty.spawn`'s `env` can put any of them back. tests/pty.rs checks these lists against
//! Python's, so the two can't drift.

use std::ffi::{OsStr, OsString};

/// core/spawn.py's `IDENTITY_ENV_VARS`: the tracing headers and the run's marker.
pub const IDENTITY: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_CUSTOM_HEADERS",
    "AISQUARE_PIPELINE_ID",
    "AISQUARE_TRACE_AGENT_NAME",
    "AISQUARE_RUN_TRACE_ID",
    "AISQUARE_LAUNCH_ID",
    "AISQUARE_LAUNCH_AGENT",
    "AISQUARE_FLEET_AGENT",
];

/// `untraced_env`'s prefix: a sandboxed agent's broker's scope.
pub const BROKERED: &str = "AISQUARE_BROKERED";

/// core/spawn.py's `TEAM_ENV_VARS`: what ties a process to one board, seat or role.
pub const TEAM: &[&str] = &[
    "AISQUARE_TEAM_HUB",
    "AISQUARE_FLEET_AGENT",
    "AISQUARE_ROLE",
    "AISQUARE_PERSONA",
    "AISQUARE_BOARD_CONTEXT",
];

/// core/spawn.py's `agent_identity_env_vars()`: which agent process a shell is under.
pub const AGENT: &[&str] = &[
    "CLAUDE_PID",
    "CLAUDECODE",
    "CODEX_THREAD_ID",
    "CODEX_SESSION_ID",
    "CODEX_TOOL_CALL_ID",
    "CODEX_CI",
    "CODEX_SANDBOX_NETWORK_DISABLED",
    "CODEX_PERMISSION_PROFILE",
    "CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS",
    "CODEX_VERSION",
    "CODEX_MANAGED_BY_NPM",
    "CODEX_MANAGED_PACKAGE_ROOT",
];

/// core/spawn.py's `tool_shell_homes`: each agent's home, taken out when rlxd's own
/// environment is that agent's tool shell, one that holds any of its markers.
pub const HOMES: &[(&str, &[&str])] = &[
    ("CLAUDE_CONFIG_DIR", &[]),
    (
        "CODEX_HOME",
        &[
            "CODEX_THREAD_ID",
            "CODEX_SESSION_ID",
            "CODEX_TOOL_CALL_ID",
            "CODEX_CI",
            "CODEX_SANDBOX_NETWORK_DISABLED",
            "CODEX_PERMISSION_PROFILE",
            "CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS",
            "CODEX_VERSION",
            "CODEX_MANAGED_BY_NPM",
            "CODEX_MANAGED_PACKAGE_ROOT",
        ],
    ),
];

/// What tmux sets for its panes, rlxd's own: a PTY's process is in no tmux, and in no
/// terminal program but rlxd's.
pub const TERMINAL: &[&str] = &["TMUX", "TMUX_PANE", "TERM_PROGRAM", "TERM_PROGRAM_VERSION"];

/// Whether two names are one: as they are on macOS and Linux, and on Windows, whatever
/// their case.
fn same(a: &OsStr, b: &str) -> bool {
    if cfg!(windows) {
        a.to_str().is_some_and(|a| a.eq_ignore_ascii_case(b))
    } else {
        a == b
    }
}

/// Whether `name` starts with [`BROKERED`]: whatever its case on Windows, where names
/// are one whatever their case, as Python's `os.environ` upper-cases them there.
fn brokered(name: &OsStr) -> bool {
    let (name, prefix) = (name.as_encoded_bytes(), BROKERED.as_bytes());
    name.len() >= prefix.len()
        && if cfg!(windows) {
            name[..prefix.len()].eq_ignore_ascii_case(prefix)
        } else {
            name.starts_with(prefix)
        }
}

/// `env`, rlxd's own, as a PTY's process inherits it: less every name above, and an
/// agent's home when `env` is that agent's tool shell.
pub fn inherited(env: impl IntoIterator<Item = (OsString, OsString)>) -> Vec<(OsString, OsString)> {
    let env: Vec<(OsString, OsString)> = env.into_iter().collect();
    let marked = |marker: &str| {
        env.iter()
            .any(|(name, value)| same(name, marker) && !value.is_empty())
    };
    let homes: Vec<&str> = HOMES
        .iter()
        .filter(|(_, markers)| markers.iter().any(|marker| marked(marker)))
        .map(|(home, _)| *home)
        .collect();
    env.iter()
        .filter(|(name, _)| {
            let named = |names: &[&str]| names.iter().any(|dropped| same(name, dropped));
            let brokered = brokered(name);
            !(named(IDENTITY)
                || named(TEAM)
                || named(AGENT)
                || named(TERMINAL)
                || named(&homes)
                || brokered)
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kept(names: &[&str]) -> Vec<String> {
        let env = names.iter().map(|name| ((*name).into(), "v".into()));
        inherited(env)
            .into_iter()
            .map(|(name, _)| name.into_string().unwrap())
            .collect()
    }

    #[test]
    fn a_brokered_scope_is_taken_out_whatever_follows_the_prefix() {
        assert_eq!(
            kept(&[
                "AISQUARE_BROKERED_SCOPE",
                "AISQUARE_BROKERED",
                "PATH",
                "AISQUARE_BROKE"
            ]),
            ["PATH", "AISQUARE_BROKE"]
        );
    }

    /// #2641's nit: on Windows a name is one whatever its case, the prefix's included.
    #[test]
    fn on_windows_names_and_the_prefix_match_whatever_their_case() {
        let names = ["aisquare_brokered_scope", "Tmux", "claudecode", "PATH"];
        if cfg!(windows) {
            assert_eq!(kept(&names), ["PATH"]);
        } else {
            assert_eq!(kept(&names), names);
        }
    }
}
