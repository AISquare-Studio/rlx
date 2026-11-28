//! The connector map (handoff 0.10.3, 5.3; the owner's connector-aware accounts, §1): for
//! each Claude Code account rlx knows, each connector's status, kind and when it was
//! checked, kept in the connectors' cache ([`crate::connectors`]) and fresh.
//!
//! - **The accounts** come from the brain (`accounts`: Python owns them), read-only: rlxd
//!   never writes the account registry and never reads the Keychain. Without a brain, or
//!   with one that lists none, there is the default account alone, slot 1: the plain
//!   `claude` on rlxd's own environment.
//! - **A refresh** (`connectors.list {refresh: true}`) reads each account's connectors,
//!   or `account`'s, with no model turn ([`Runner::read_connectors`]), one account at a
//!   time. A read that fails keeps the account's connectors as read before, with its
//!   error. Without the route (capability `connectors.no_turn`), `allow_turn: true` runs
//!   the one-turn probe instead, of one account.
//! - **In the background**, only on the no-turn route, so no turn is ever spent unasked:
//!   every [`TICK`], the brain is asked again, and each account that is new, in another
//!   home or signed in again ([`Account::login`]) is read; so is each one last read more
//!   than [`STALE`] ago. A run's `system/init` updates the default account's connectors
//!   as it comes, so a connector a run finds failing needs no refresh.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rlxd_proto::{Data, ErrorBody, codes};
use tokio::sync::watch;

use crate::brain::{Brain, CallError};
use crate::connectors::{Account, Connectors};
use crate::log::{info, warning};
use crate::runner::Runner;

/// How often the background asks the brain for the accounts, and reads those that need it.
pub const TICK: Duration = Duration::from_secs(10 * 60);

/// How old an account's connectors may be before the background reads them again.
pub const STALE: Duration = Duration::from_secs(24 * 60 * 60);

/// How long the brain has to take `accounts`, and to answer it.
const ACCOUNTS_ACCEPT: Duration = Duration::from_secs(5);
const ACCOUNTS_RUN: Duration = Duration::from_secs(15);

/// The map: who keeps it fresh.
pub struct ConnectorMap {
    runner: Arc<Runner>,
    connectors: Arc<Connectors>,
    brain: Arc<Brain>,
    /// The reads under way, by slot: a refresh of an account being read joins that read,
    /// and takes its outcome.
    reading: Mutex<HashMap<u32, Reading>>,
}

/// One read under way: its outcome, once it has one.
type Reading = watch::Receiver<Option<Result<(), ErrorBody>>>;

/// Takes a read out of [`ConnectorMap::reading`] when it ends, or its caller goes.
struct Done<'a> {
    reading: &'a Mutex<HashMap<u32, Reading>>,
    slot: u32,
}

impl Drop for Done<'_> {
    fn drop(&mut self) {
        self.reading
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.slot);
    }
}

impl ConnectorMap {
    /// The map of `connectors`, read by `runner`, its accounts from `brain`.
    pub fn new(runner: Arc<Runner>, connectors: Arc<Connectors>, brain: Arc<Brain>) -> Arc<Self> {
        Arc::new(Self {
            runner,
            connectors,
            brain,
            reading: Mutex::new(HashMap::new()),
        })
    }

    /// Reads `account`'s connectors with no turn, or, while a read of it is under way,
    /// waits for that one and takes its outcome. Reads run one at a time
    /// ([`Runner::read_connectors`]).
    pub async fn read(&self, account: &Account) -> Result<(), ErrorBody> {
        let slot = account.slot;
        let joined = {
            let mut reading = self.reading.lock().unwrap_or_else(PoisonError::into_inner);
            match reading.get(&slot) {
                Some(under_way) => Err(under_way.clone()),
                None => {
                    let (outcome, under_way) = watch::channel(None);
                    reading.insert(slot, under_way);
                    Ok(outcome)
                }
            }
        };
        let outcome = match joined {
            Ok(outcome) => outcome,
            Err(mut under_way) => {
                let ended = under_way.wait_for(Option::is_some).await;
                return match ended {
                    Ok(outcome) => outcome.clone().unwrap_or(Ok(())),
                    Err(_) => Err(refusal(
                        codes::PROBE_FAILED,
                        format!("account {slot}'s read ended with no outcome"),
                    )),
                };
            }
        };
        let done = Done {
            reading: &self.reading,
            slot,
        };
        let read = self.runner.read_connectors(account).await;
        drop(done);
        outcome.send_replace(Some(read.clone()));
        read
    }

    /// Asks the brain which accounts there are, and takes them: those to read now, each
    /// new one and each in another home or signed in again. When the brain can't say,
    /// the accounts stay as they were, and none is to read.
    pub async fn learn_accounts(&self) -> Vec<Account> {
        match self.listed().await {
            Ok(listed) => off_worker(|| self.connectors.set_accounts(listed)),
            Err(why) => {
                info!("connectors: the accounts stay as they were: {why}");
                Vec::new()
            }
        }
    }

    /// The accounts the brain lists.
    async fn listed(&self) -> Result<Vec<Account>, String> {
        if let Some(listed) = test_accounts() {
            return listed;
        }
        let data = match self
            .brain
            .call("accounts", Data::new(), ACCOUNTS_ACCEPT, ACCOUNTS_RUN)
            .await
        {
            Ok(data) => data,
            Err(CallError::NotDelivered(error) | CallError::Failed(error)) => {
                return Err(format!("the brain said {}: {}", error.code, error.message));
            }
            Err(CallError::TimedOut) => return Err("the brain did not answer in time".into()),
        };
        accounts_of(data.get("accounts").cloned().unwrap_or_default())
    }

    /// `connectors.list {refresh: true}` on the no-turn route: reads `only`'s connectors,
    /// or every account's (asking the brain for them first). Fails only when no read
    /// succeeded: with the first read's error.
    pub async fn refresh(&self, only: Option<u32>) -> Result<(), ErrorBody> {
        self.learn_accounts().await;
        let accounts: Vec<Account> = match only {
            Some(slot) => self.connectors.account(slot).into_iter().collect(),
            None => self.connectors.accounts(),
        };
        let mut first_error = None;
        let mut read = 0_usize;
        for account in &accounts {
            match self.read(account).await {
                Ok(()) => read += 1,
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        // Identity (task #14's second step) is read here, after the reads, on an explicit
        // refresh only: until then each connector's `identity` stays null.
        match first_error {
            Some(error) if read == 0 => Err(error),
            _ => Ok(()),
        }
    }

    /// `connectors.list {refresh: true, allow_turn: true}` without the no-turn route: the
    /// one-turn probe, of `only` or the default account.
    pub async fn probe(&self, only: Option<u32>) -> Result<(), ErrorBody> {
        let account = only
            .and_then(|slot| self.connectors.account(slot))
            .unwrap_or_else(Account::default_one);
        self.runner.probe_connectors(&account).await
    }

    /// Whether rlx knows the account of `slot`, asking the brain again when it doesn't
    /// yet.
    pub async fn knows(&self, slot: u32) -> Result<(), ErrorBody> {
        if self.connectors.account(slot).is_some() {
            return Ok(());
        }
        self.learn_accounts().await;
        if self.connectors.account(slot).is_some() {
            return Ok(());
        }
        Err(refusal(
            codes::NO_SUCH_ACCOUNT,
            format!("rlx knows no Claude Code account in slot {slot}"),
        ))
    }

    /// The background, for as long as rlxd runs: on the no-turn route only.
    pub async fn run(self: Arc<Self>) {
        if !self.connectors.no_turn() {
            return;
        }
        let Some((tick, stale)) = test_times().unwrap_or(Some((TICK, STALE))) else {
            return;
        };
        loop {
            self.tick(stale).await;
            tokio::time::sleep(tick).await;
        }
    }

    /// One tick: the accounts asked again, and each that needs it read.
    async fn tick(&self, stale: Duration) {
        let mut to_read = self.learn_accounts().await;
        for account in off_worker(|| self.connectors.stale(stale)) {
            if !to_read.iter().any(|each| each.slot == account.slot) {
                to_read.push(account);
            }
        }
        for account in to_read {
            if let Err(error) = self.read(&account).await {
                warning!(
                    "connectors: the background read of account {} failed: {}",
                    account.slot,
                    error.message
                );
            }
        }
    }
}

/// An error body for a refused map call.
fn refusal(code: &str, message: String) -> ErrorBody {
    ErrorBody::new(code, message).expect("a map refusal has a code and a message")
}

/// The brain's `accounts` list: each Claude Code account's slot, label, homes and login
/// mark.
fn accounts_of(listed: serde_json::Value) -> Result<Vec<Account>, String> {
    serde_json::from_value(listed).map_err(|e| format!("the brain's accounts were not read: {e}"))
}

/// Runs `work`, which reads or writes the cache's file, off the async worker.
fn off_worker<T>(work: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(work)
        }
        _ => work(),
    }
}

/// Test builds only: `RLXD_TEST_ACCOUNTS` names a file holding the accounts, as the
/// brain's `accounts` would list them, read each time they are asked for.
#[cfg(feature = "debug-methods")]
fn test_accounts() -> Option<Result<Vec<Account>, String>> {
    let path = std::env::var_os("RLXD_TEST_ACCOUNTS")?;
    Some(
        std::fs::read_to_string(&path)
            .map_err(|e| format!("RLXD_TEST_ACCOUNTS could not be read: {e}"))
            .and_then(|text| {
                serde_json::from_str(&text).map_err(|e| format!("RLXD_TEST_ACCOUNTS: {e}"))
            })
            .and_then(accounts_of),
    )
}

#[cfg(not(feature = "debug-methods"))]
fn test_accounts() -> Option<Result<Vec<Account>, String>> {
    None
}

/// Test builds only: `RLXD_TEST_CONNECTOR_MAP_MS=<tick>,<stale>` is the background's
/// tick and how old an account's connectors may be, in milliseconds; `off` runs no
/// background.
#[cfg(feature = "debug-methods")]
fn test_times() -> Option<Option<(Duration, Duration)>> {
    let text = std::env::var("RLXD_TEST_CONNECTOR_MAP_MS").ok()?;
    if text == "off" {
        return Some(None);
    }
    let (tick, stale) = text.split_once(',')?;
    Some(Some((
        Duration::from_millis(tick.trim().parse().ok()?),
        Duration::from_millis(stale.trim().parse().ok()?),
    )))
}

#[cfg(not(feature = "debug-methods"))]
fn test_times() -> Option<Option<(Duration, Duration)>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_brains_accounts_are_read_with_their_homes_and_fields_it_may_add() {
        let listed = serde_json::json!([
            {"slot": 1, "label": "main", "login": "a1"},
            {"slot": 2, "label": null, "config_dir": "/h/2", "tmp_dir": "/t/2", "login": "b2",
             "agent": "claude-code", "later": true},
        ]);
        let accounts = accounts_of(listed).unwrap();
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].label.as_deref(), Some("main"));
        assert!(
            accounts[0].env().is_empty(),
            "the default account runs on rlxd's env"
        );
        let env = accounts[1].env();
        assert_eq!(env.len(), 2);
        assert_eq!(env[0].0, "CLAUDE_CONFIG_DIR");
        assert_eq!(env[1].0, "CLAUDE_CODE_TMPDIR");
        assert!(accounts_of(serde_json::json!({"slot": 1})).is_err());
    }
}
