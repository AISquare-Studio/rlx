//! Reading one account's connectors with no model turn (handoff 0.10.3, 5.3; spike #5's
//! `mcp_status`, P1 and P7): what `connectors.list {refresh: true}` does with the
//! capability `connectors.no_turn`, once for each account.
//!
//! It is rlxd's own Claude Code, not a run, as the one-turn probe is ([`super::probe`]),
//! and the two never run at once. It is started on a control channel with
//! [`argv::call_args`] (no tools, no session, project settings only) in an empty folder of
//! its own, `<home>/rlxd/connectors-read/<slot>/`, on the account's environment: rlxd's
//! own, with the account's `CLAUDE_CONFIG_DIR` and `CLAUDE_CODE_TMPDIR` for any but the
//! default account ([`Account::env`]).
//!
//! - **`initialize`**, then **`mcp_status`** every [`STATUS_EVERY`], until it lists a
//!   server and none of them is `pending`: that listing is the account's connectors now.
//!   Up to about 0.5 s after its start, Claude Code lists no server at all (the claude.ai
//!   list not fetched yet), which is no answer yet. When [`LISTING_WAIT`] (15 s) has gone
//!   by with no server listed, or one still `pending`, the read fails `settle_timeout`:
//!   the account keeps the connectors read before, and no event is published.
//! - **No user line is ever written**, and no `mcp_call`: it makes no model turn and no
//!   call. A request of Claude Code's is refused, and a model turn's line ends the read.
//! - Then stdin is closed, and the tree is ended once Claude Code has exited, or
//!   [`EXIT_GRACE`] later.
//!
//! A Claude Code that refuses `mcp_status` has no route: `no_route`. rlxd's log says when
//! each read ran, of which slot, with `model_turns: 0`, and never what it listed.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use rlxd_proto::{ErrorBody, McpServer, codes};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncWriteExt, BufReader};
use tokio::process::ChildStdin;
use tokio::time::Instant;

use super::call::{MAX_LINE, Message, listed_of};
use super::process::Launch;
use super::{Runner, argv, blocking, refusal, run_env, stream};
use crate::claude;
use crate::connectors::{Account, Observed, Read};
use crate::log::{info, warning};

/// How long Claude Code has to answer `initialize`.
const INIT_WAIT: Duration = Duration::from_secs(30);

/// How often `mcp_status` is asked.
const STATUS_EVERY: Duration = Duration::from_millis(250);

/// How long the connectors have, from the first `mcp_status`, to be listed with none
/// `pending` (the settle bound, at most 20 s).
const LISTING_WAIT: Duration = Duration::from_secs(15);

/// How long Claude Code has to exit once its stdin is closed.
const EXIT_GRACE: Duration = Duration::from_secs(5);

const INIT_ID: &str = "rlxd-initialize";

/// What one `mcp_status` listed, in the cache's terms.
#[derive(Debug, Default)]
struct Listing {
    servers: Vec<McpServer>,
    sources: HashMap<String, String>,
    tools: Vec<String>,
    hints: HashMap<String, bool>,
}

impl Listing {
    /// `mcp_status`'s `rows`: each server with its status and source, and each connected
    /// one's tools as Claude Code names them, with their read-only hints (`readOnly: true`
    /// is a hint; `mcp_status` gives no other value).
    fn of(rows: &[Value]) -> Self {
        let listed = listed_of(rows);
        let mut listing = Self {
            hints: listed.hints,
            ..Self::default()
        };
        for row in rows {
            let (Some(name), Some(status)) = (row["name"].as_str(), row["status"].as_str()) else {
                continue;
            };
            listing.servers.push(McpServer::new(name, status));
            if let Some(source) = row["source"].as_str() {
                listing.sources.insert(name.to_owned(), source.to_owned());
            }
        }
        listing.tools = listed.tools.into_iter().collect();
        listing.tools.sort();
        listing
    }

    /// Whether this listing is settled: some server, none `pending`.
    fn settled(&self) -> bool {
        !self.servers.is_empty()
            && self.servers.iter().all(|s| {
                rlxd_proto::ConnectorState::of(&s.status) != rlxd_proto::ConnectorState::Pending
            })
    }

    /// What an unsettled listing held, by count only: never a connector's name.
    fn unsettled(&self) -> String {
        if self.servers.is_empty() {
            return "it listed no connector".to_owned();
        }
        let pending = self
            .servers
            .iter()
            .filter(|s| {
                rlxd_proto::ConnectorState::of(&s.status) == rlxd_proto::ConnectorState::Pending
            })
            .count();
        format!(
            "{pending} of its {} connectors were still pending",
            self.servers.len()
        )
    }

    fn observed(&self) -> Observed<'_> {
        Observed {
            servers: &self.servers,
            sources: &self.sources,
            tools: &self.tools,
            hints: &self.hints,
        }
    }
}

impl Runner {
    /// Reads `account`'s connectors with no model turn, and takes them as its connectors
    /// now. When the read fails, the account keeps the connectors read before, and the
    /// error (`claude_not_found`, `no_route`, or `probe_failed`, saying why).
    pub async fn read_connectors(&self, account: &Account) -> Result<(), ErrorBody> {
        match self.list_connectors(account).await {
            Ok(listing) => {
                blocking(|| {
                    self.connectors
                        .observe_on(account, listing.observed(), Read::Listing);
                });
                Ok(())
            }
            Err(error) => {
                warning!(
                    "connectors: account {}'s read failed: {}",
                    account.slot,
                    error.message
                );
                blocking(|| self.connectors.refresh_failed(account, error.clone()));
                Err(error)
            }
        }
    }

    /// One read: Claude Code started on `account`'s environment, driven, and ended.
    async fn list_connectors(&self, account: &Account) -> Result<Listing, ErrorBody> {
        let _one = self.probing.lock().await;
        let Some(program) = claude::resolve().and_then(|p| std::path::absolute(p).ok()) else {
            return Err(refusal(
                codes::CLAUDE_NOT_FOUND,
                format!(
                    "Claude Code was not found: there is no `claude` on rlxd's PATH, and {} \
                     names none",
                    claude::BIN_ENV
                ),
            ));
        };
        let failed = |why: String| {
            refusal(
                codes::PROBE_FAILED,
                format!("account {}'s connectors were not read: {why}", account.slot),
            )
        };
        let dir = self
            .home
            .rlxd_dir()
            .join("connectors-read")
            .join(account.slot.to_string());
        blocking(|| std::fs::create_dir_all(&dir))
            .map_err(|e| failed(format!("its folder could not be made: {e}")))?;
        info!(
            "connectors: account {}'s connectors are read (model_turns: 0)",
            account.slot
        );
        let mut env = run_env();
        env.extend(account.env());
        let launch = Launch {
            program: program.clone(),
            args: argv::call_args(),
            cwd: dir,
            env,
            unset: account.unset(),
        };
        let (mut child, tree) = self.spawner.spawn(launch).await.map_err(|e| {
            failed(format!(
                "Claude Code ({}) could not start: {e}",
                program.display()
            ))
        })?;
        let tree = Arc::new(tree);
        *self.probe_tree.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::clone(&tree));
        let read = match (child.stdin.take(), child.stdout.take()) {
            (Some(mut stdin), Some(stdout)) => {
                let read = Control {
                    stdin: &mut stdin,
                    stdout: BufReader::new(stdout),
                    line: Vec::new(),
                }
                .listing()
                .await;
                let _ = stdin.shutdown().await;
                drop(stdin);
                read
            }
            _ => Err(Failure::Broken(
                "Claude Code's stdio was not piped".to_owned(),
            )),
        };
        if tree.reap(&mut child, Some(EXIT_GRACE)).await.is_none() {
            tree.kill();
            tree.reap(&mut child, None).await;
        }
        *self.probe_tree.lock().unwrap_or_else(|p| p.into_inner()) = None;
        info!("connectors: account {}'s read ended", account.slot);
        read.map_err(|failure| match failure {
            Failure::NoRoute(why) => refusal(
                codes::NO_ROUTE,
                format!(
                    "account {}'s connectors can't be read with no turn: {why}",
                    account.slot
                ),
            ),
            Failure::Unsettled(listed) => refusal(
                codes::SETTLE_TIMEOUT,
                format!(
                    "account {}'s connectors did not settle in {} ms: {listed}",
                    account.slot,
                    listing_wait().as_millis()
                ),
            ),
            Failure::Broken(why) => failed(why),
        })
    }
}

/// Why a read failed.
#[derive(Debug)]
enum Failure {
    /// This Claude Code has no `mcp_status`.
    NoRoute(String),
    /// `mcp_status` listed no server, or one still `pending`, to the end of the wait: how
    /// many it listed, and how many were pending.
    Unsettled(String),
    /// Anything else.
    Broken(String),
}

/// The read's control channel: Claude Code's stdin and stdout.
struct Control<'a, R> {
    stdin: &'a mut ChildStdin,
    stdout: R,
    line: Vec<u8>,
}

impl<R: AsyncBufRead + Unpin> Control<'_, R> {
    /// `initialize`, then `mcp_status` until it settles or its wait runs out.
    async fn listing(&mut self) -> Result<Listing, Failure> {
        let init = json!({"type": "control_request", "request_id": INIT_ID,
            "request": {"subtype": "initialize"}});
        let until = Instant::now() + INIT_WAIT;
        self.send(&init, until).await?;
        if let Err(error) = self.reply_to(INIT_ID, until).await? {
            return Err(Failure::Broken(format!(
                "Claude Code refused initialize: {error}"
            )));
        }
        let until = Instant::now() + listing_wait();
        let mut asked = 0_u32;
        loop {
            asked += 1;
            let id = format!("rlxd-mcp-status-{asked}");
            let status = json!({"type": "control_request", "request_id": id,
                "request": {"subtype": "mcp_status"}});
            // The last ask may take a little past the wait: its answer is still taken.
            let answer_by = until.max(Instant::now()) + STATUS_EVERY * 8;
            self.send(&status, answer_by).await?;
            let rows = match self.reply_to(&id, answer_by).await? {
                Ok(reply) => reply["mcpServers"].as_array().cloned().unwrap_or_default(),
                Err(error) => {
                    return Err(Failure::NoRoute(format!(
                        "this Claude Code refused mcp_status: {error}"
                    )));
                }
            };
            let listing = Listing::of(&rows);
            if listing.settled() {
                return Ok(listing);
            }
            let next = Instant::now() + STATUS_EVERY;
            if next >= until {
                return Err(Failure::Unsettled(listing.unsettled()));
            }
            tokio::time::sleep_until(next).await;
        }
    }

    /// Writes `message` as one line, taken by `until`.
    async fn send(&mut self, message: &Value, until: Instant) -> Result<(), Failure> {
        let mut line = message.to_string();
        line.push('\n');
        let written = tokio::time::timeout_at(until, async {
            self.stdin.write_all(line.as_bytes()).await?;
            self.stdin.flush().await
        })
        .await;
        match written {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(Failure::Broken(format!(
                "Claude Code's stdin could not be written: {e}"
            ))),
            Err(_) => Err(Failure::Broken(
                "Claude Code took no request in time".to_owned(),
            )),
        }
    }

    /// The reply to rlxd's request `id`, by `until`: Claude Code's own requests
    /// meanwhile are refused, and a model turn's line ends the read.
    async fn reply_to(
        &mut self,
        id: &str,
        until: Instant,
    ) -> Result<Result<Value, String>, Failure> {
        loop {
            let read = tokio::select! {
                read = stream::next_line(&mut self.stdout, &mut self.line, MAX_LINE) => read,
                () = tokio::time::sleep_until(until) => {
                    return Err(Failure::Broken("Claude Code did not answer in time".to_owned()));
                }
            };
            match read {
                Ok(Some(())) => {}
                Ok(None) => {
                    return Err(Failure::Broken("Claude Code's output ended".to_owned()));
                }
                Err(stream::ReadError::TooLong) => {
                    return Err(Failure::Broken(format!(
                        "Claude Code wrote a control line longer than {MAX_LINE} bytes"
                    )));
                }
                Err(stream::ReadError::Io(e)) => {
                    return Err(Failure::Broken(format!(
                        "Claude Code's output could not be read: {e}"
                    )));
                }
            }
            let Ok(message) = serde_json::from_slice::<Value>(&self.line) else {
                continue;
            };
            match Message::of(&message) {
                Message::Replied { request_id, result } if request_id == id => return Ok(result),
                Message::Asked {
                    request_id,
                    request,
                } => {
                    let subtype = request["subtype"].as_str().unwrap_or("?");
                    let response = json!({"type": "control_response", "response": {
                        "subtype": "error", "request_id": request_id,
                        "error": format!("rlxd answers no {subtype} here")}});
                    self.send(&response, until).await?;
                }
                Message::Turn(kind) => {
                    return Err(Failure::Broken(format!(
                        "Claude Code wrote {kind:?}, a model turn's, which a read never makes"
                    )));
                }
                Message::Replied { .. } | Message::Other => {}
            }
        }
    }
}

/// The settle bound: [`LISTING_WAIT`], or in a test build `RLXD_TEST_CONNECTORS_SETTLE_MS`.
fn listing_wait() -> Duration {
    test_listing_wait().unwrap_or(LISTING_WAIT)
}

#[cfg(feature = "debug-methods")]
fn test_listing_wait() -> Option<Duration> {
    std::env::var("RLXD_TEST_CONNECTORS_SETTLE_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
        .map(Duration::from_millis)
}

#[cfg(not(feature = "debug-methods"))]
fn test_listing_wait() -> Option<Duration> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listing_is_settled_once_it_lists_a_server_and_none_is_pending() {
        let rows = |statuses: &[&str]| -> Vec<Value> {
            statuses
                .iter()
                .enumerate()
                .map(|(n, status)| json!({"name": format!("s{n}"), "status": status}))
                .collect()
        };
        assert!(!Listing::of(&[]).settled());
        assert!(!Listing::of(&rows(&["connected", "pending"])).settled());
        assert!(Listing::of(&rows(&["connected", "needs-auth", "failed"])).settled());
    }

    #[test]
    fn a_listing_names_tools_as_claude_code_does_and_keeps_sources() {
        let rows = [
            json!({"name": "claude.ai Fixture", "status": "connected", "source": "claudeai",
                "tools": [{"name": "read", "annotations": {"readOnly": true}},
                          {"name": "write", "annotations": {}}]}),
            json!({"name": "fixture-local", "status": "needs-auth",
                "tools": [{"name": "echo"}]}),
        ];
        let listing = Listing::of(&rows);
        assert_eq!(
            listing.tools,
            [
                "mcp__claude_ai_Fixture__read",
                "mcp__claude_ai_Fixture__write"
            ]
        );
        assert_eq!(
            listing.hints.get("mcp__claude_ai_Fixture__read"),
            Some(&true)
        );
        assert_eq!(listing.hints.get("mcp__claude_ai_Fixture__write"), None);
        assert_eq!(
            listing.sources.get("claude.ai Fixture").map(String::as_str),
            Some("claudeai")
        );
        assert_eq!(listing.servers.len(), 2);
    }
}
