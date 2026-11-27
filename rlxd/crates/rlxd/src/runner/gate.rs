//! The connector gate (handoff 0.10.3, 5.5): a run with `needs_connectors` starts no turn
//! until each is `connected`, as Claude Code's `system/init` reports them.
//!
//! An attempt makes up to [`TRIES`] tries, each its own Claude Code, with a backoff of
//! 2 s and then 5 s between them ([`backoff`]). What a try does depends on the route:
//!
//! - **Route 1** (capability `connectors.gate_before_turn`): Claude Code reports the
//!   states before the first message. The try waits up to `connector_wait_s` for every
//!   named connector to be `connected`, then sends the prompt. Worst case 3 × 10 + 2 + 5
//!   = 37 s.
//! - **Route 2**, the default: the states show only in the `system/init` that follows the
//!   first message. The try settles `connector_settle_s` with stdin open, sends the
//!   prompt, and reads `system/init`; on a miss it is ended at once. Worst case 3 × 5 +
//!   2 + 5 = 22 s, and [`START_ALLOWANCE`] a try more.
//!
//! A try stops the gate at once when a named connector is `needs-auth` or `failed`.
//! Nothing a try that is not confirmed wrote reaches a client: not its `system/init`,
//! nor any text. When the tries run out, the run ends `run.failed {reason:
//! "connectors", connectors: [{id, status}]}`, naming each connector that was not
//! connected: `pending`, `needs-auth`, `failed`, or `absent` when the `system/init` did
//! not list it.

use std::time::Duration;

use rlxd_proto::{ConnectorState, ConnectorStatus, McpServer};

/// The most tries an attempt makes.
pub const TRIES: u32 = 3;

/// What a settle adds for Claude Code to come up. rlxd can't see when Claude Code has
/// started: it counts from the spawn, which on macOS and Linux is the guard's start, the
/// guard then starting Claude Code. So Claude Code itself has the whole settle, the
/// fixtures' `stdin[0].at >= 5000` with it, however slowly a loaded machine starts it.
pub const START_ALLOWANCE: Duration = Duration::from_millis(500);

/// The wait after try `done`, before the next: 2 s after the first, 5 s after the second.
pub fn backoff(done: u32) -> Duration {
    if done <= 1 {
        Duration::from_secs(2)
    } else {
        Duration::from_secs(5)
    }
}

/// A gated attempt's rules.
#[derive(Clone, Debug)]
pub struct Gate {
    /// The connectors that must be connected, by name.
    pub named: Vec<String>,
    /// Whether the states come before the first message.
    pub route_1: bool,
    /// Route 1's wait for them.
    pub wait: Duration,
    /// Route 2's settle before the prompt.
    pub settle: Duration,
}

/// What a `system/init` says of the named connectors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Each is connected: the try goes on.
    Connected,
    /// Some are not connected yet, as these say: another try may find them.
    Missed(Vec<ConnectorStatus>),
    /// One is `needs-auth` or `failed`, which another try won't change: these, every
    /// named connector not connected.
    Stopped(Vec<ConnectorStatus>),
}

impl Gate {
    /// The verdict of a `system/init` listing `servers`.
    pub fn verdict(&self, servers: &[McpServer]) -> Verdict {
        let mut missing = Vec::new();
        let mut stop = false;
        for id in &self.named {
            let status = match servers.iter().find(|server| &server.name == id) {
                None => "absent",
                Some(server) => match ConnectorState::of(&server.status) {
                    ConnectorState::Connected => continue,
                    state @ (ConnectorState::NeedsAuth | ConnectorState::Failed) => {
                        stop = true;
                        state.as_str()
                    }
                    ConnectorState::Pending => "pending",
                },
            };
            missing.push(ConnectorStatus::new(id.clone(), status));
        }
        match (missing.is_empty(), stop) {
            (true, _) => Verdict::Connected,
            (false, true) => Verdict::Stopped(missing),
            (false, false) => Verdict::Missed(missing),
        }
    }

    /// What a route-1 try that saw no `system/init` reports: each named connector
    /// `pending`.
    pub fn none_seen(&self) -> Vec<ConnectorStatus> {
        self.named
            .iter()
            .map(|id| ConnectorStatus::new(id.clone(), "pending"))
            .collect()
    }
}

/// The run's message for a gate that ended with `connectors` not connected.
pub fn failure_message(connectors: &[ConnectorStatus], tries: u32) -> String {
    let named = connectors
        .iter()
        .map(|c| format!("{} is {}", c.id, c.status))
        .collect::<Vec<_>>()
        .join(", ");
    let tries = if tries == 1 {
        "1 try".to_owned()
    } else {
        format!("{tries} tries")
    };
    format!("the run's connectors were not connected after {tries}: {named}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(named: &[&str]) -> Gate {
        Gate {
            named: named.iter().map(|&n| n.to_owned()).collect(),
            route_1: false,
            wait: Duration::from_secs(10),
            settle: Duration::from_secs(5),
        }
    }

    #[test]
    fn the_verdict_names_each_connector_not_connected() {
        let g = gate(&["claude.ai Slack", "claude.ai Gmail"]);
        let slack = |s: &str| McpServer::new("claude.ai Slack", s);
        let gmail = |s: &str| McpServer::new("claude.ai Gmail", s);
        assert_eq!(
            g.verdict(&[slack("connected"), gmail("connected")]),
            Verdict::Connected
        );
        assert_eq!(
            g.verdict(&[slack("pending"), gmail("connected")]),
            Verdict::Missed(vec![ConnectorStatus::new("claude.ai Slack", "pending")])
        );
        assert_eq!(
            g.verdict(&[slack("pending")]),
            Verdict::Missed(vec![
                ConnectorStatus::new("claude.ai Slack", "pending"),
                ConnectorStatus::new("claude.ai Gmail", "absent"),
            ])
        );
        assert_eq!(
            g.verdict(&[slack("needs-auth"), gmail("pending")]),
            Verdict::Stopped(vec![
                ConnectorStatus::new("claude.ai Slack", "needs-auth"),
                ConnectorStatus::new("claude.ai Gmail", "pending"),
            ])
        );
        assert_eq!(
            g.verdict(&[slack("connected"), gmail("failed")]),
            Verdict::Stopped(vec![ConnectorStatus::new("claude.ai Gmail", "failed")])
        );
    }

    #[test]
    fn backoffs_are_two_then_five_seconds() {
        assert_eq!(backoff(1), Duration::from_secs(2));
        assert_eq!(backoff(2), Duration::from_secs(5));
    }

    #[test]
    fn the_message_says_each_status() {
        let c = [ConnectorStatus::new("claude.ai Slack", "pending")];
        assert_eq!(
            failure_message(&c, 3),
            "the run's connectors were not connected after 3 tries: claude.ai Slack is pending"
        );
    }
}
