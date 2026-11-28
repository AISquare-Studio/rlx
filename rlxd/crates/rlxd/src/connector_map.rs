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
//! - **At rlxd's start**, when its runs' `CLAUDE_CONFIG_DIR` names one (the app's when it
//!   set one, as a run's Claude Code gets it, 4.3; else rlxd's own), the brain is asked
//!   once, so rlxd knows which account its own runs use (with none named, it is the
//!   default account): a run's `system/init` updates that account's connectors as it
//!   comes, so a connector a run finds failing needs no refresh. Each change to the app's
//!   values settles that account again, asking the brain when it must.
//! - **Identity** (task #14, step 2, [`crate::identity`]): after an explicit refresh's
//!   reads, with the capability `connectors.call` on and an app connected, each account is
//!   asked who it is on each connector the identity table names, with one gated read call
//!   each ([`Runner::identity_call`]): the app decides every one. Never from a tick.
//! - **In the background**, only on the no-turn route, so no turn is ever spent unasked:
//!   every [`TICK`], the brain is asked again, and each account that is new or signed in
//!   again (another [`Account::login`] mark) is read at once; so is each one whose last
//!   read, whether it succeeded or failed, was more than [`STALE`] ago. So an account is
//!   read in the background at most daily, unless it is new or signed in again.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rlxd_proto::{ConnectorsCallParams, DEFAULT_ACCOUNT, Data, ErrorBody, codes};
use tokio::sync::watch;

use crate::brain::{Brain, CallError};
use crate::connectors::{Account, Connectors};
use crate::log::{info, warning};
use crate::runner::Runner;

/// How often the background asks the brain for the accounts, and reads those that need it.
pub const TICK: Duration = Duration::from_secs(10 * 60);

/// How old an account's connectors may be before the background reads them again.
pub const STALE: Duration = Duration::from_secs(24 * 60 * 60);

/// How long rlxd waits, before it asks the brain, for the brain to be up or found missing.
const BRAIN_SETTLE: Duration = Duration::from_secs(5);

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
    /// Whether [`ConnectorMap::follow_env`]'s ask of the brain for the accounts is under
    /// way: one at a time.
    asking: AtomicBool,
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
            asking: AtomicBool::new(false),
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
                off_worker(|| self.connectors.unanswered());
                Vec::new()
            }
        }
    }

    /// The accounts the brain lists.
    async fn listed(&self) -> Result<Vec<Account>, String> {
        if let Some(listed) = test_accounts() {
            return listed;
        }
        // At rlxd's start the brain may not be up yet, nor found missing: wait for one or
        // the other, so a missing brain says so at once rather than at the accept wait's
        // end (and a run's init held meanwhile is filed soon).
        let until = tokio::time::Instant::now() + BRAIN_SETTLE;
        loop {
            let state = self.brain.status();
            if state.pid.is_some() || state.error.is_some() || tokio::time::Instant::now() >= until
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let data = match self
            .brain
            .call("accounts", Data::new(), ACCOUNTS_ACCEPT, ACCOUNTS_RUN)
            .await
        {
            Ok(data) => data,
            // Its code only: a brain's message may name a home.
            Err(CallError::NotDelivered(error) | CallError::Failed(error)) => {
                return Err(format!("the brain said {}", error.code));
            }
            Err(CallError::TimedOut) => return Err("the brain did not answer in time".into()),
        };
        accounts_of(data.get("accounts").cloned().unwrap_or_default())
    }

    /// `connectors.list {refresh: true}` on the no-turn route: reads `only`'s connectors,
    /// or every account's (asking the brain for them first). Fails only when no read
    /// succeeded: with the first read's error.
    pub async fn refresh(&self, only: Option<u32>, identify: bool) -> Result<(), ErrorBody> {
        self.learn_accounts().await;
        let accounts: Vec<Account> = match only {
            Some(slot) => self.connectors.account(slot).into_iter().collect(),
            None => self.connectors.accounts(),
        };
        let mut first_error = None;
        let mut fresh = Vec::new();
        for account in &accounts {
            match self.read(account).await {
                Ok(()) => fresh.push(account),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        // Who each account is on its connectors: on an explicit refresh only (never a
        // tick's) by a client that may make a connectors.call (`identify`: app, cli and
        // tui, not remote), with the call route on and an app connected to decide each.
        if identify && self.connectors.call_route() && self.runner.app_present() {
            for account in &fresh {
                self.identify(account).await;
            }
        }
        let read = fresh.len();
        match first_error {
            Some(error) if read == 0 => Err(error),
            _ => Ok(()),
        }
    }

    /// Reads who `account` is on each connector the identity table names, that its own
    /// listing holds `connected` with the table's tool read-only: one call each, at most,
    /// through `connectors.call`'s gate path, so the app decides each one. A deny, a
    /// failure or a result that says no identity changes nothing. rlxd's log holds counts
    /// and codes, never what was read.
    async fn identify(&self, account: &Account) {
        let sources = off_worker(|| self.connectors.identity_sources(account.slot));
        let (mut read, mut unread) = (0_usize, 0_usize);
        for source in sources {
            let Ok(params) = ConnectorsCallParams::new(
                crate::scheduler::fresh_run_id(),
                source.connector,
                source.tool,
                (source.input)(),
            ) else {
                continue;
            };
            let identity = match self.runner.identity_call(params, account).await {
                Ok(data) if data.is_error != Some(true) => {
                    data.content.as_ref().and_then(source.read)
                }
                Ok(_) => None,
                Err(error) => {
                    info!(
                        "connectors: account {}'s identity on {} was not read: {}",
                        account.slot, source.connector, error.code
                    );
                    None
                }
            };
            match identity {
                Some(identity) => {
                    // Under the sign-in it was asked under, or not at all.
                    let stored = off_worker(|| {
                        self.connectors.set_identity(
                            account.slot,
                            account.login.as_deref(),
                            source.connector,
                            identity,
                        )
                    });
                    if stored {
                        read += 1;
                    } else {
                        info!(
                            "connectors: account {}'s identity on {} is dropped: signed_in_again",
                            account.slot, source.connector
                        );
                        unread += 1;
                    }
                }
                None => unread += 1,
            }
        }
        if read + unread > 0 {
            info!(
                "connectors: account {}: {read} identities read, {unread} not",
                account.slot
            );
        }
    }

    /// `connectors.list {refresh: true, allow_turn: true}` without the no-turn route: the
    /// one-turn probe, of `only` or the default account, as rlx knows it.
    pub async fn probe(&self, only: Option<u32>) -> Result<(), ErrorBody> {
        let account = self
            .connectors
            .account(only.unwrap_or(DEFAULT_ACCOUNT))
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

    /// The background, for as long as rlxd runs. When its runs' `CLAUDE_CONFIG_DIR` (the
    /// app's, else rlxd's own) names one, it asks the brain for the accounts at its start,
    /// so rlxd knows which account its own runs use, and follows each change to the app's
    /// values ([`ConnectorMap::follow_env`]); then, on the no-turn route only, it ticks.
    pub async fn run(self: Arc<Self>) {
        tokio::spawn(Arc::clone(&self).follow_env());
        let ticks = test_times().unwrap_or(Some((TICK, STALE)));
        if !self.connectors.no_turn() || ticks.is_none() {
            return;
        }
        let Some((tick, stale)) = ticks else {
            return;
        };
        loop {
            self.tick(stale).await;
            tokio::time::sleep(tick).await;
        }
    }

    /// Works out again which account rlxd's runs use on each change to the app's values,
    /// as runs take the app's `CLAUDE_CONFIG_DIR` (4.3), and asks the brain for the
    /// accounts when rlx has yet to say them and it matters. Once at its start too, for a
    /// change made before it listened. The ask runs on its own, so a change made while it
    /// waits on the brain is taken at once.
    async fn follow_env(self: Arc<Self>) {
        let mut changes = self.connectors.env_changes();
        loop {
            off_worker(|| self.connectors.env_changed());
            if self.connectors.runs_account_unknown() && !self.asking.swap(true, Ordering::SeqCst) {
                let map = Arc::clone(&self);
                tokio::spawn(async move {
                    map.learn_accounts().await;
                    map.asking.store(false, Ordering::SeqCst);
                });
            }
            if changes.changed().await.is_err() {
                return;
            }
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
