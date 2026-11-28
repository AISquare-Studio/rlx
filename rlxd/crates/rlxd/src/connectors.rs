//! The user's connectors (handoff 0.10.3, 2.4, 5.3's listing, 5.5 and 7.7): the MCP
//! servers Claude Code's `system/init` lists, the claude.ai connectors among them, with
//! their status and the tools it mounted.
//!
//! - **The cache.** Each `system/init` rlxd takes as the truth of a run updates it: an
//!   ungated run's, a gated run's once its connectors are confirmed, the last try's of a
//!   gated run that failed, and a one-turn refresh's. A gated try's `system/init` that is
//!   not confirmed and is followed by another try changes nothing, so a connector warming
//!   up is no change. A run with mount `no_tools` lists none (`--strict-mcp-config`) and
//!   is not read, and no `system/init` that lists no server at all empties the cache: a
//!   Claude Code that loaded none says so too (E9's stdin at EOF). The cache is kept in
//!   `<home>/rlxd/connectors.json` (0600), written whole ([`crate::durable::write`])
//!   before any change is published, so a restarted rlxd lists what it listed.
//! - **Events** (topic `connector`): `connector.added` and `connector.removed`, and
//!   `connector.changed {id, status, previous_status}` once for each real change. A
//!   change that leaves a connector `connected` is also told to the runner
//!   ([`Connectors::on_connected`]), which clears a run's `needs_you {reason: "auth"}`
//!   once each connector it named is back.
//! - **`connectors.list`** gives the cache: `from: "init"`, `model_turns: 0`.
//! - **`connectors.login {id}`** runs `claude mcp login <id>` in an rlxd PTY and reports
//!   its steps as `connector.login.*` events. Only an `https://` URL is passed on, and no
//!   URL's query, which holds the login's secrets, is ever in rlxd's log.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use rlxd_client::Home;
use rlxd_proto::{
    AccountConnectors, Connector, ConnectorChanged, ConnectorId, ConnectorLoginDone,
    ConnectorLoginError, ConnectorLoginOpenUrl, ConnectorLoginWaiting, ConnectorState,
    ConnectorTool, ConnectorsList, ConnectorsLoginData, DEFAULT_ACCOUNT, ErrorBody, ListedFrom,
    LoginErrorCode, McpServer, PtyKillParams, PtyScreenParams, PtySpawnParams, capabilities,
    connector_types, topics,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::bus::Bus;
use crate::log::{info, warning};
use crate::pty::Host;
use crate::{claude, clock, durable};

/// The cache's file, under `<home>/rlxd/`.
const FILE: &str = "connectors.json";

/// How long a login may print nothing to open before it is ended `no_url`.
const URL_WAIT: Duration = Duration::from_secs(60);

/// How long a login may take in all before it is ended `timed_out`: the user has the
/// browser to go through.
const LOGIN_WAIT: Duration = Duration::from_secs(300);

/// How often a login's screen is read.
const LOGIN_POLL: Duration = Duration::from_millis(100);

/// The width of a login's PTY, the widest there is, so its URL is on one line.
const LOGIN_COLS: u16 = rlxd_proto::MAX_PTY_SIZE;

/// How long a gated run's try waits on route 1 for its connectors to be connected, and
/// settles before its prompt on route 2 (5.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GateTimes {
    /// `connector_wait_s`: route 1's wait, 10 s.
    pub wait: Duration,
    /// `connector_settle_s`: route 2's settle, 5 s.
    pub settle: Duration,
}

impl Default for GateTimes {
    fn default() -> Self {
        Self {
            wait: Duration::from_secs(10),
            settle: Duration::from_secs(5),
        }
    }
}

/// Told every connector the default account holds `connected`, after a change left one
/// so.
pub type OnConnected = Box<dyn Fn(&[String]) + Send + Sync>;

/// One Claude Code account rlx knows: its slot, and the environment Claude Code runs on
/// there. The brain says which there are (`accounts`); with no brain, only the default.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// Its rlx slot: [`DEFAULT_ACCOUNT`] for the plain `claude` on rlxd's own environment.
    pub slot: u32,
    /// Its alias in rlx.
    #[serde(default)]
    pub label: Option<String>,
    /// Its `CLAUDE_CONFIG_DIR`. Never set for the default account, which runs on rlxd's
    /// own environment as it is: pointing the variable at `~/.claude` is not the same.
    #[serde(default)]
    pub config_dir: Option<PathBuf>,
    /// Its `CLAUDE_CODE_TMPDIR`, which goes with `config_dir`.
    #[serde(default)]
    pub tmp_dir: Option<PathBuf>,
    /// An opaque mark of who is signed in there: another one is another sign-in.
    #[serde(default)]
    pub login: Option<String>,
}

impl Account {
    /// The default account, slot 1: rlxd's own environment.
    pub fn default_one() -> Self {
        Self {
            slot: DEFAULT_ACCOUNT,
            label: None,
            config_dir: None,
            tmp_dir: None,
            login: None,
        }
    }

    /// What Claude Code's environment adds for this account: a managed one's two
    /// variables, or nothing for the default account.
    pub fn env(&self) -> Vec<(OsString, OsString)> {
        let mut env = Vec::new();
        if self.slot == DEFAULT_ACCOUNT {
            return env;
        }
        if let Some(dir) = &self.config_dir {
            env.push(("CLAUDE_CONFIG_DIR".into(), dir.clone().into_os_string()));
        }
        if let Some(dir) = &self.tmp_dir {
            env.push(("CLAUDE_CODE_TMPDIR".into(), dir.clone().into_os_string()));
        }
        env
    }

    /// Whether rlxd knows both of this account's homes, as a read on its own needs: always
    /// for the default account; for a managed one, its `CLAUDE_CONFIG_DIR` and its
    /// `CLAUDE_CODE_TMPDIR` alike, so a read never mixes one account's home with another's.
    pub fn homes_known(&self) -> bool {
        self.slot == DEFAULT_ACCOUNT || (self.config_dir.is_some() && self.tmp_dir.is_some())
    }

    /// What Claude Code's environment takes away for this account: for the default one,
    /// both variables, as rlxd itself may run under a managed slot's.
    pub fn unset(&self) -> Vec<OsString> {
        if self.slot == DEFAULT_ACCOUNT {
            return vec!["CLAUDE_CONFIG_DIR".into(), "CLAUDE_CODE_TMPDIR".into()];
        }
        Vec::new()
    }

    /// Whether `self` is signed in as someone else than when `entry` was read: its
    /// connectors are to be read again.
    fn signed_in_again(&self, entry: &Entry) -> bool {
        self.login != entry.login
    }
}

/// `dir`, resolved, or as it is when it can't be.
fn canonical(dir: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())
}

/// Which account rlxd's own runs use, as they run on rlxd's environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Runs {
    /// rlxd's environment names a `CLAUDE_CONFIG_DIR`, and rlx hasn't yet said which
    /// accounts there are: a run's init is held until it has.
    Waiting,
    /// This slot's.
    Slot(u32),
    /// No account rlx knows: rlxd's `CLAUDE_CONFIG_DIR` is none of theirs. A run's init
    /// is filed nowhere, never under a guess.
    Nowhere,
}

impl Runs {
    fn slot(self) -> Option<u32> {
        match self {
            Self::Slot(slot) => Some(slot),
            Self::Waiting | Self::Nowhere => None,
        }
    }
}

/// Which account rlxd's own runs use: the plain one when rlxd's environment names no
/// `CLAUDE_CONFIG_DIR` (`own`, resolved); else, once rlx has said which accounts there
/// are or couldn't (`told`), the one whose resolved `CLAUDE_CONFIG_DIR` it is. While no
/// brain has ever listed the accounts (`listed`, kept in the file across restarts), rlx
/// knows the plain account alone, so it is the plain one's, as before accounts; once one
/// has, a home that is none of them is filed nowhere. Resolves each home: never under
/// the cache's lock.
fn runs_of(
    own: Option<&std::path::Path>,
    accounts: &BTreeMap<u32, Account>,
    told: bool,
    listed: bool,
) -> Runs {
    let Some(own) = own else {
        return Runs::Slot(DEFAULT_ACCOUNT);
    };
    if !told {
        return Runs::Waiting;
    }
    let managed = accounts.values().find(|account| {
        account.slot != DEFAULT_ACCOUNT
            && account
                .config_dir
                .as_deref()
                .is_some_and(|dir| canonical(dir) == own)
    });
    match managed {
        Some(account) => Runs::Slot(account.slot),
        None if !listed => Runs::Slot(DEFAULT_ACCOUNT),
        None => Runs::Nowhere,
    }
}

/// What a run's `system/init` listed, kept until rlx says which account it belongs to.
#[derive(Clone, Debug, Default)]
struct Seen {
    servers: Vec<McpServer>,
    sources: HashMap<String, String>,
    tools: Vec<String>,
    hints: HashMap<String, bool>,
}

impl Seen {
    fn of(init: Observed<'_>) -> Self {
        Self {
            servers: init.servers.to_vec(),
            sources: init.sources.clone(),
            tools: init.tools.to_vec(),
            hints: init.hints.clone(),
        }
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

/// What the file holds: each account's entry, by slot.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Saved {
    accounts: BTreeMap<u32, Entry>,
    /// Whether a brain has ever listed the accounts ([`runs_of`]).
    #[serde(default)]
    listed: bool,
}

/// One account's connectors, as rlxd last read them.
///
/// It keeps the account's home and login mark as the brain gave them: the cache is keyed
/// by each account's home, and a new mark is a new sign-in to read at once. They are
/// rlxd's own, in its 0600 file, and never on the wire, in an event or in rlxd's log.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
    #[serde(default)]
    label: Option<String>,
    /// The account's home and sign-in when they were read ([`Account`]).
    #[serde(default)]
    config_dir: Option<PathBuf>,
    #[serde(default)]
    tmp_dir: Option<PathBuf>,
    #[serde(default)]
    login: Option<String>,
    /// When they were last read.
    as_of: Option<String>,
    /// When a read of them was last tried, whether it succeeded or failed: what the
    /// background ages them by.
    #[serde(default)]
    tried_at: Option<String>,
    #[serde(default = "from_init")]
    from: ListedFrom,
    /// Why the latest refresh failed.
    #[serde(default)]
    error: Option<ErrorBody>,
    /// The connectors, by `id`.
    connectors: Vec<Connector>,
}

impl Default for Entry {
    fn default() -> Self {
        Self {
            label: None,
            config_dir: None,
            tmp_dir: None,
            login: None,
            as_of: None,
            tried_at: None,
            from: ListedFrom::Init,
            error: None,
            connectors: Vec::new(),
        }
    }
}

fn from_init() -> ListedFrom {
    ListedFrom::Init
}

/// The file before rlxd kept accounts apart: the default account's alone.
#[derive(Deserialize)]
struct SavedV1 {
    as_of: Option<String>,
    connectors: Vec<Connector>,
}

impl Saved {
    /// The file's text as a cache: either version.
    fn read(text: &str) -> serde_json::Result<Self> {
        let value: Value = serde_json::from_str(text)?;
        if value.get("accounts").is_some() {
            return serde_json::from_value(value);
        }
        let old: SavedV1 = serde_json::from_value(value)?;
        let entry = Entry {
            as_of: old.as_of,
            connectors: old.connectors,
            ..Entry::default()
        };
        Ok(Self {
            accounts: BTreeMap::from([(DEFAULT_ACCOUNT, entry)]),
            listed: false,
        })
    }

    fn entry(&mut self, slot: u32) -> &mut Entry {
        self.accounts.entry(slot).or_default()
    }
}

impl Entry {
    /// Keeps `account`'s label, homes and mark, as rlx gave them, with what was read.
    fn keep(&mut self, account: &Account) {
        self.label.clone_from(&account.label);
        self.config_dir.clone_from(&account.config_dir);
        self.tmp_dir.clone_from(&account.tmp_dir);
        self.login.clone_from(&account.login);
    }
}

/// What a read of one account's connectors found ([`Connectors::observe_on`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read {
    /// A `system/init`: a run's, or a one-turn probe's.
    Init,
    /// A listing with no model turn (`mcp_status`).
    Listing,
}

/// The home's connectors, for each account.
pub struct Connectors {
    home: Home,
    bus: Arc<Bus>,
    cache: Mutex<Saved>,
    /// The accounts rlx knows, by slot: the default one at least.
    accounts: Mutex<BTreeMap<u32, Account>>,
    /// rlxd's own `CLAUDE_CONFIG_DIR`, resolved once: the home its runs, calls and logins
    /// use, when its environment names one.
    own_dir: Option<PathBuf>,
    /// Which account rlxd's runs use ([`Connectors::runs_account`]).
    runs: Mutex<Runs>,
    /// The latest run's `system/init` that came before then: taken as that account's once
    /// rlx has said which it is.
    held: Mutex<Option<Seen>>,
    /// Whether this Claude Code reports connector states before the first message (5.5,
    /// route 1): the capability `connectors.gate_before_turn`.
    route_1: bool,
    /// Whether this rlxd makes `connectors.call` (capability `connectors.call`, 5.3): off
    /// until the owner-run probe confirms Claude Code's route.
    call_route: bool,
    /// Whether this rlxd reads the connectors with no model turn (capability
    /// `connectors.no_turn`, `mcp_status`): off until the owner-run probe confirms it.
    no_turn: bool,
    gate: GateTimes,
    /// The logins running, by connector: their `login_id`s.
    logins: Mutex<HashMap<String, String>>,
    /// Who is told when a change leaves a connector `connected`: the runner.
    on_connected: OnceLock<OnConnected>,
}

impl Connectors {
    /// The connectors of `home`, as its file last held them, their events published on
    /// `bus`.
    pub fn new(home: Home, bus: Arc<Bus>) -> Arc<Self> {
        let own_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        Self::with_own_dir(home, bus, own_dir.as_deref())
    }

    /// [`Connectors::new`], rlxd's own `CLAUDE_CONFIG_DIR` being `own_dir`.
    fn with_own_dir(home: Home, bus: Arc<Bus>, own_dir: Option<&std::path::Path>) -> Arc<Self> {
        let path = home.rlxd_dir().join(FILE);
        let saved = match std::fs::read_to_string(&path) {
            Ok(text) => Saved::read(&text).unwrap_or_else(|e| {
                warning!(
                    "connectors: {} is not valid ({e}); the list starts empty",
                    path.display()
                );
                Saved::default()
            }),
            Err(_) => Saved::default(),
        };
        // The accounts the file holds are known until the brain says otherwise, each as
        // the brain gave it.
        let mut accounts = BTreeMap::from([(DEFAULT_ACCOUNT, Account::default_one())]);
        for (slot, entry) in &saved.accounts {
            let account = Account {
                slot: *slot,
                label: entry.label.clone(),
                config_dir: entry.config_dir.clone(),
                tmp_dir: entry.tmp_dir.clone(),
                login: entry.login.clone(),
            };
            accounts.insert(
                *slot,
                if *slot == DEFAULT_ACCOUNT {
                    Account {
                        label: account.label,
                        login: account.login,
                        ..Account::default_one()
                    }
                } else {
                    account
                },
            );
        }
        let own_dir = own_dir.map(canonical);
        let runs = runs_of(own_dir.as_deref(), &accounts, false, saved.listed);
        Arc::new(Self {
            home,
            bus,
            cache: Mutex::new(saved),
            accounts: Mutex::new(accounts),
            own_dir,
            runs: Mutex::new(runs),
            held: Mutex::new(None),
            route_1: test_route_1(),
            call_route: test_call_route(),
            no_turn: test_no_turn(),
            gate: GateTimes::default(),
            logins: Mutex::new(HashMap::new()),
            on_connected: OnceLock::new(),
        })
    }

    /// Has `told` called, with no lock of the cache's held, with every connector the
    /// default account holds `connected` each time a change leaves one so. The first
    /// caller's stands.
    pub fn on_connected(&self, told: OnConnected) {
        if self.on_connected.set(told).is_err() {
            warning!("connectors: a second on_connected is not taken");
        }
    }

    /// Tells [`Connectors::on_connected`]'s callee who is connected now.
    fn tell_connected(&self, connected: &[String]) {
        if let Some(told) = self.on_connected.get() {
            told(connected);
        }
    }

    fn cache(&self) -> MutexGuard<'_, Saved> {
        self.cache.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn known(&self) -> MutexGuard<'_, BTreeMap<u32, Account>> {
        self.accounts.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Whether a gated try waits for the states before its prompt (route 1).
    pub fn route_1(&self) -> bool {
        self.route_1
    }

    /// Whether this rlxd makes `connectors.call` (5.3).
    pub fn call_route(&self) -> bool {
        self.call_route
    }

    /// Whether this rlxd reads the connectors with no model turn (5.3).
    pub fn no_turn(&self) -> bool {
        self.no_turn
    }

    /// What the runs' account's cache says of `connector`'s `tool`: its
    /// `read_only_hint`, or `None` when it doesn't list that tool of that connector.
    pub fn read_only_hint(&self, connector: &str, tool: &str) -> Option<Option<bool>> {
        let slot = self.runs_slot()?;
        let cache = self.cache();
        cache
            .accounts
            .get(&slot)?
            .connectors
            .iter()
            .find(|c| c.id == connector)
            .and_then(|c| c.tools.iter().find(|t| t.name == tool))
            .map(|t| t.read_only_hint)
    }

    /// The gate's times.
    pub fn gate_times(&self) -> GateTimes {
        self.gate
    }

    /// The capabilities connectors add to `hello` and `status`.
    pub fn capabilities(&self) -> Vec<&'static str> {
        let mut named = Vec::new();
        if self.route_1 {
            named.push(capabilities::CONNECTORS_GATE_BEFORE_TURN);
        }
        if self.no_turn {
            named.push(capabilities::CONNECTORS_NO_TURN);
        }
        if self.call_route {
            named.push(capabilities::CONNECTORS_CALL);
        }
        named
    }

    /// The accounts rlx knows, by slot: the default one first.
    pub fn accounts(&self) -> Vec<Account> {
        self.known().values().cloned().collect()
    }

    /// The account of `slot`, when rlx knows it.
    pub fn account(&self, slot: u32) -> Option<Account> {
        self.known().get(&slot).cloned()
    }

    /// The account rlxd's runs, calls and logins use, as they run on rlxd's own
    /// environment, as rlx knows it (its mark included): the plain one when rlxd's
    /// environment names no `CLAUDE_CONFIG_DIR`, else the one whose resolved
    /// `CLAUDE_CONFIG_DIR` it is. `None` while rlx hasn't said which accounts there are,
    /// and when it is none of theirs.
    pub fn runs_account(&self) -> Option<Account> {
        let slot = self.runs_slot()?;
        self.account(slot)
    }

    /// Whether rlx has yet to say which account rlxd's runs use: only while rlxd's
    /// environment names a `CLAUDE_CONFIG_DIR`.
    pub fn runs_account_unknown(&self) -> bool {
        self.runs_now() == Runs::Waiting
    }

    fn runs_now(&self) -> Runs {
        *self.runs.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn runs_slot(&self) -> Option<u32> {
        self.runs_now().slot()
    }

    /// Notes that rlx couldn't say which accounts there are: the accounts stay as they
    /// were, and rlxd's runs are filed by them from now on.
    pub fn unanswered(&self) {
        let known = self.known().clone();
        let listed = self.cache().listed;
        self.settle_runs(runs_of(self.own_dir.as_deref(), &known, true, listed));
    }

    /// Takes `runs` as the runs' account, and files the `system/init` held until it was
    /// known, or, when it is no account rlx knows, lets it go.
    fn settle_runs(&self, runs: Runs) {
        *self.runs.lock().unwrap_or_else(|p| p.into_inner()) = runs;
        if runs == Runs::Waiting {
            return;
        }
        let held = self.held.lock().unwrap_or_else(|p| p.into_inner()).take();
        match (held, self.runs_account()) {
            (Some(seen), Some(account)) => self.observe_on(&account, seen.observed(), Read::Init),
            (Some(_), None) => info!(
                "connectors: a run's connectors are filed under no account: rlxd's \
                 CLAUDE_CONFIG_DIR is no account rlx knows"
            ),
            (None, _) => {}
        }
    }

    /// Takes `listed` as the accounts rlx knows (the default one is always known), and
    /// gives those whose connectors are to be read at once: each new one, and each signed
    /// in as someone else since it was read (another mark). An account no longer listed
    /// is dropped from the cache, with `connector.removed` for each of its connectors.
    pub fn set_accounts(&self, listed: Vec<Account>) -> Vec<Account> {
        let mut now = BTreeMap::from([(DEFAULT_ACCOUNT, Account::default_one())]);
        for account in listed {
            if account.slot == 0 {
                continue;
            }
            if account.slot == DEFAULT_ACCOUNT {
                // Its label only: it runs on rlxd's own environment.
                now.insert(
                    DEFAULT_ACCOUNT,
                    Account {
                        label: account.label,
                        login: account.login,
                        ..Account::default_one()
                    },
                );
                continue;
            }
            now.insert(account.slot, account);
        }
        // Each home resolved before the cache's lock is taken.
        let runs = runs_of(self.own_dir.as_deref(), &now, true, true);
        let mut cache = self.cache();
        let mut changed = !cache.listed;
        cache.listed = true;
        let gone: Vec<u32> = cache
            .accounts
            .keys()
            .copied()
            .filter(|slot| !now.contains_key(slot))
            .collect();
        let mut removed = Vec::new();
        for slot in gone {
            info!("connectors: account {slot} is no longer known; its connectors are dropped");
            if let Some(entry) = cache.accounts.remove(&slot) {
                removed.extend(
                    entry
                        .connectors
                        .into_iter()
                        .map(|c| ConnectorId::new(c.id).on(slot)),
                );
            }
            changed = true;
        }
        let mut to_read = Vec::new();
        for account in now.values() {
            match cache.accounts.get_mut(&account.slot) {
                None => to_read.push(account.clone()),
                Some(entry) => {
                    if account.signed_in_again(entry) {
                        to_read.push(account.clone());
                    }
                    if entry.label != account.label {
                        entry.label.clone_from(&account.label);
                        changed = true;
                    }
                }
            }
        }
        if changed {
            self.save(&cache);
        }
        for gone in removed {
            self.publish(connector_types::REMOVED, json_of(&gone));
        }
        drop(cache);
        *self.known() = now;
        self.settle_runs(runs);
        to_read
    }

    /// The accounts whose connectors a read last tried longer than `age` ago (whether it
    /// succeeded or failed), or never.
    pub fn stale(&self, age: Duration) -> Vec<Account> {
        let accounts = self.accounts();
        let cache = self.cache();
        accounts
            .into_iter()
            .filter(|account| {
                let read = cache
                    .accounts
                    .get(&account.slot)
                    .and_then(|entry| entry.tried_at.as_deref().or(entry.as_of.as_deref()))
                    .and_then(clock::parse);
                read.is_none_or(|read| clock::since(read) > age)
            })
            .collect()
    }

    /// `connectors.list`: the plain account's connectors (slot 1's), and every account's
    /// (or `only`'s), having made `model_turns`.
    pub fn list(&self, model_turns: u32, only: Option<u32>) -> ConnectorsList {
        let accounts = self.accounts();
        let cache = self.cache();
        let default = cache.accounts.get(&DEFAULT_ACCOUNT);
        let listed = accounts
            .iter()
            .filter(|account| only.is_none_or(|slot| slot == account.slot))
            .map(|account| {
                let entry = cache.accounts.get(&account.slot);
                AccountConnectors::new(
                    account.slot,
                    entry.map(|e| e.connectors.clone()).unwrap_or_default(),
                    entry.and_then(|e| e.as_of.clone()),
                    entry.map_or(ListedFrom::Init, |e| e.from),
                )
                .with_label(account.label.clone())
                .with_error(entry.and_then(|e| e.error.clone()))
            })
            .collect();
        ConnectorsList::new(
            default.map(|e| e.connectors.clone()).unwrap_or_default(),
            default.and_then(|e| e.as_of.clone()),
            default.map_or(ListedFrom::Init, |e| e.from),
            model_turns,
        )
        .with_accounts(listed)
    }

    /// Takes a run's `system/init` as the runs' account's connectors now
    /// ([`Connectors::observe_on`]), as rlx knows that account. Until rlx has said which
    /// account that is, the latest init is held, and taken then; when it is no account
    /// rlx knows, it is filed nowhere.
    pub fn observe(&self, init: Observed<'_>) {
        match (self.runs_now(), self.runs_account()) {
            (Runs::Slot(_), Some(account)) => self.observe_on(&account, init, Read::Init),
            (Runs::Waiting, _) => {
                info!("connectors: a run's connectors wait for rlx to say which account it is");
                *self.held.lock().unwrap_or_else(|p| p.into_inner()) = Some(Seen::of(init));
            }
            _ => info!(
                "connectors: a run's connectors are filed under no account: rlxd's \
                 CLAUDE_CONFIG_DIR is no account rlx knows"
            ),
        }
    }

    /// Takes what was read of `account`'s connectors (`read`) as its connectors now: its
    /// `servers`, with each one's `source` where it carried one (`sources`), and the
    /// `tools` it mounted, with each tool's `readOnlyHint` where it carried one (`hints`).
    /// The account's entry and the file are replaced, then each change is published. A
    /// read that lists no server changes nothing in an entry that holds some.
    /// `account` is as rlx knows it: its home and mark are kept with the entry.
    pub fn observe_on(&self, account: &Account, init: Observed<'_>, read: Read) {
        let Observed {
            servers,
            sources,
            tools,
            hints,
        } = init;
        let slot = account.slot;
        let at = clock::now();
        let mut now: BTreeMap<String, Connector> = BTreeMap::new();
        for server in servers {
            if now.contains_key(&server.name)
                || rlxd_proto::check_connector_id("id", &server.name).is_err()
            {
                continue;
            }
            let prefix = tool_prefix(&server.name);
            let mounted = tools
                .iter()
                .filter(|tool| tool.starts_with(&prefix))
                .map(|tool| ConnectorTool::new(tool.clone(), hints.get(tool).copied()))
                .collect();
            let connector = Connector::new(
                server.name.clone(),
                source_of(&server.name, sources.get(&server.name).map(String::as_str)),
                ConnectorState::of(&server.status),
                mounted,
            )
            .checked_at(at.clone());
            now.insert(server.name.clone(), connector);
        }
        let runs = self.runs_slot();
        let mut cache = self.cache();
        let entry = cache.entry(slot);
        if now.is_empty() && !entry.connectors.is_empty() {
            // It can't be told from a Claude Code that loaded no server (E9's stdin at
            // EOF, a `no_tools` start), so it empties nothing.
            return;
        }
        let before: BTreeMap<&str, ConnectorState> = entry
            .connectors
            .iter()
            .map(|c| (c.id.as_str(), c.status))
            .collect();
        let mut events: Vec<(&str, Value)> = Vec::new();
        for (id, connector) in &now {
            match before.get(id.as_str()) {
                None => events.push((
                    connector_types::ADDED,
                    json_of(&ConnectorId::new(id).on(slot)),
                )),
                Some(&previous) if previous != connector.status => events.push((
                    connector_types::CHANGED,
                    json_of(&ConnectorChanged::new(id, connector.status, previous).on(slot)),
                )),
                Some(_) => {}
            }
        }
        for id in before.keys() {
            if !now.contains_key(*id) {
                events.push((
                    connector_types::REMOVED,
                    json_of(&ConnectorId::new(*id).on(slot)),
                ));
            }
        }
        let connected_now = now.iter().any(|(id, connector)| {
            connector.status == ConnectorState::Connected
                && before.get(id.as_str()) != Some(&ConnectorState::Connected)
        });
        entry.connectors = now.into_values().collect();
        entry.error = None;
        entry.tried_at = Some(at.clone());
        entry.as_of = Some(at);
        entry.from = match read {
            Read::Init => ListedFrom::Init,
            Read::Listing => ListedFrom::Listing,
        };
        entry.keep(account);
        let connected = (connected_now && Some(slot) == runs).then(|| connected_in(entry));
        // Kept, then published, both under the cache's lock: two reads' changes are
        // written and published in their order, and none is published before it is
        // kept.
        self.save(&cache);
        for (r#type, data) in events {
            self.publish(r#type, data);
        }
        drop(cache);
        if let Some(connected) = connected {
            self.tell_connected(&connected);
        }
    }

    /// Notes that a refresh of `account` failed with `error`: its connectors stay as they
    /// were read before.
    pub fn refresh_failed(&self, account: &Account, error: ErrorBody) {
        let mut cache = self.cache();
        let entry = cache.entry(account.slot);
        entry.keep(account);
        entry.error = Some(error);
        entry.tried_at = Some(clock::now());
        self.save(&cache);
    }

    /// Sets the runs' account's `id`'s status, when it has it: a login's end, or a
    /// call's `mcp_status` that found it `needs-auth` or `failed`.
    pub fn set_status(&self, id: &str, status: ConnectorState) {
        let Some(slot) = self.runs_slot() else {
            return;
        };
        let mut cache = self.cache();
        let entry = cache.entry(slot);
        let Some(connector) = entry.connectors.iter_mut().find(|c| c.id == id) else {
            return;
        };
        let previous = connector.status;
        connector.checked_at = Some(clock::now());
        if previous == status {
            self.save(&cache);
            return;
        }
        connector.status = status;
        let connected = (status == ConnectorState::Connected).then(|| connected_in(entry));
        self.save(&cache);
        self.publish(
            connector_types::CHANGED,
            json_of(&ConnectorChanged::new(id, status, previous).on(slot)),
        );
        drop(cache);
        if let Some(connected) = connected {
            self.tell_connected(&connected);
        }
    }

    fn path(&self) -> PathBuf {
        self.home.rlxd_dir().join(FILE)
    }

    fn save(&self, saved: &Saved) {
        let path = self.path();
        let written = serde_json::to_string_pretty(saved)
            .map_err(std::io::Error::other)
            .and_then(|text| {
                std::fs::create_dir_all(self.home.rlxd_dir())?;
                durable::write(&path, text.as_bytes())
            });
        if let Err(e) = written {
            warning!("connectors: {} was not written: {e}", path.display());
        }
    }

    fn publish(&self, r#type: &str, data: Value) {
        let Value::Object(data) = data else {
            warning!("connectors: {type} did not print as an object");
            return;
        };
        if let Err(e) = self.bus.publish(topics::CONNECTOR, r#type, data) {
            warning!("connectors: {type} was not published: {e}");
        }
    }

    /// `connectors.login {id}`: starts `claude mcp login <id>` in a PTY of `host`, and
    /// watches it on a thread of its own. A login of `id` already running is the same
    /// login.
    pub fn login(self: &Arc<Self>, host: &Arc<Host>, id: &str) -> ConnectorsLoginData {
        let mut logins = self.logins.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(login_id) = logins.get(id) {
            return ConnectorsLoginData::new(login_id.clone());
        }
        let login_id = format!("login-{}", random_hex());
        logins.insert(id.to_owned(), login_id.clone());
        drop(logins);
        info!("connectors.login {login_id}: for {id}");
        let connectors = Arc::clone(self);
        let host = Arc::clone(host);
        let (id, watched) = (id.to_owned(), login_id.clone());
        let started = std::thread::Builder::new()
            .name("connector-login".to_owned())
            .spawn({
                let (connectors, id, watched) =
                    (Arc::clone(&connectors), id.clone(), watched.clone());
                move || {
                    connectors.watch_login(&host, &id, &watched);
                    connectors
                        .logins
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(&id);
                }
            });
        if let Err(e) = started {
            connectors.login_failed(
                &watched,
                LoginErrorCode::Failed,
                &format!("rlxd could not watch the login: {e}"),
            );
            connectors
                .logins
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&id);
        }
        ConnectorsLoginData::new(login_id)
    }

    fn login_failed(&self, login_id: &str, code: LoginErrorCode, message: &str) {
        warning!(
            "connectors.login {login_id}: {}: {message}",
            code_name(code)
        );
        self.publish(
            connector_types::LOGIN_ERROR,
            json_of(&ConnectorLoginError::new(login_id, code, message)),
        );
    }

    /// Runs and watches one login to its end.
    fn watch_login(&self, host: &Host, id: &str, login_id: &str) {
        let Some(program) = claude::resolve().and_then(|p| std::path::absolute(p).ok()) else {
            self.login_failed(
                login_id,
                LoginErrorCode::ClaudeNotFound,
                &format!(
                    "Claude Code was not found: there is no `claude` on rlxd's PATH, and {} \
                     names none",
                    claude::BIN_ENV
                ),
            );
            return;
        };
        // `--` first, so no id is read as an option (`connectors.login` refuses one that
        // begins with `-` anyway).
        let argv = vec![
            program.to_string_lossy().into_owned(),
            "mcp".to_owned(),
            "login".to_owned(),
            "--".to_owned(),
            id.to_owned(),
        ];
        let spawned = PtySpawnParams::new(argv, LOGIN_COLS, 50)
            .map(|params| {
                // Claude Code's environment, as a run's (run_env): rlxd's own, which the
                // PTY inherits, and what the app adds.
                crate::runner::run_env()
                    .into_iter()
                    .filter_map(|(name, value)| {
                        Some((name.into_string().ok()?, value.into_string().ok()?))
                    })
                    .fold(params, |params, (name, value)| {
                        params.with_env(name, Some(value))
                    })
                    .labelled(format!("connectors.login {id}"))
            })
            .and_then(|params| params.validate().map(|()| params))
            .map_err(|e| e.to_string())
            .and_then(|params| {
                host.spawn(&params, "rlxd connectors.login")
                    .map_err(|e| e.to_string())
            });
        let pty_id = match spawned {
            Ok(pty) => pty.pty_id,
            Err(why) => {
                self.login_failed(
                    login_id,
                    LoginErrorCode::Failed,
                    &format!("`claude mcp login` could not start: {why}"),
                );
                return;
            }
        };
        let ended = self.follow_login(host, &pty_id, id, login_id);
        if let Ok(params) = PtyKillParams::new(&pty_id) {
            let _ = host.kill(&params, "rlxd connectors.login");
        }
        if let Some(status) = ended {
            info!("connectors.login {login_id}: done");
            self.publish(
                connector_types::LOGIN_DONE,
                json_of(&ConnectorLoginDone::new(login_id, id, status)),
            );
            self.set_status(id, status);
        }
    }

    /// Reads the login's screen until it ends: the connector's status when it signed in,
    /// or `None` once it has said why not.
    fn follow_login(
        &self,
        host: &Host,
        pty_id: &str,
        id: &str,
        login_id: &str,
    ) -> Option<ConnectorState> {
        let start = Instant::now();
        let mut opened = false;
        loop {
            let screen = PtyScreenParams::new(pty_id)
                .map_err(|e| e.to_string())
                .and_then(|params| {
                    host.screen(&params.with_scrollback(2000))
                        .map_err(|e| e.to_string())
                });
            let screen = match screen {
                Ok(screen) => screen,
                Err(why) => {
                    self.login_failed(
                        login_id,
                        LoginErrorCode::Failed,
                        &format!("the login's terminal could not be read: {why}"),
                    );
                    return None;
                }
            };
            if !opened && let Some(url) = find_url(&screen.text) {
                if !url.starts_with("https://") {
                    self.login_failed(
                        login_id,
                        LoginErrorCode::NotHttps,
                        "`claude mcp login` printed a URL that is not https://, which rlxd \
                         does not pass on",
                    );
                    return None;
                }
                info!(
                    "connectors.login {login_id}: a URL to open, {}",
                    without_query(&url)
                );
                self.publish(
                    connector_types::LOGIN_OPEN_URL,
                    json_of(&ConnectorLoginOpenUrl::new(login_id, id, url)),
                );
                self.publish(
                    connector_types::LOGIN_WAITING,
                    json_of(&ConnectorLoginWaiting::new(login_id)),
                );
                opened = true;
            }
            if screen.exited {
                return match (opened, screen.status) {
                    (false, _) => {
                        self.login_failed(
                            login_id,
                            LoginErrorCode::NoUrl,
                            "`claude mcp login` ended without printing a URL to open",
                        );
                        None
                    }
                    (true, Some(0)) => Some(ConnectorState::Connected),
                    (true, status) => {
                        let how =
                            status.map_or_else(|| "ended".to_owned(), |s| format!("exited {s}"));
                        self.login_failed(
                            login_id,
                            LoginErrorCode::Failed,
                            &format!("`claude mcp login` {how} without signing in"),
                        );
                        None
                    }
                };
            }
            let waited = start.elapsed();
            if !opened && waited >= URL_WAIT {
                self.login_failed(
                    login_id,
                    LoginErrorCode::NoUrl,
                    &format!(
                        "`claude mcp login` printed no URL to open in {} s",
                        URL_WAIT.as_secs()
                    ),
                );
                return None;
            }
            if waited >= LOGIN_WAIT {
                self.login_failed(
                    login_id,
                    LoginErrorCode::TimedOut,
                    &format!("the login took longer than {} s", LOGIN_WAIT.as_secs()),
                );
                return None;
            }
            std::thread::sleep(LOGIN_POLL);
        }
    }
}

/// A `system/init`'s facts of the connectors ([`Connectors::observe`]).
#[derive(Clone, Copy, Debug)]
pub struct Observed<'a> {
    pub servers: &'a [McpServer],
    /// Each server's `source`, where its entry carried one.
    pub sources: &'a HashMap<String, String>,
    pub tools: &'a [String],
    /// Each tool's `readOnlyHint`, where its entry carried one.
    pub hints: &'a HashMap<String, bool>,
}

/// The connectors `entry` holds `connected`.
fn connected_in(entry: &Entry) -> Vec<String> {
    entry
        .connectors
        .iter()
        .filter(|c| c.status == ConnectorState::Connected)
        .map(|c| c.id.clone())
        .collect()
}

/// The prefix of the tools Claude Code mounts for the server `name`: `mcp__`, its name
/// as Claude Code writes it in tool names (the tool-call gate's
/// [`crate::runner::claude_server_name`]), then `__`.
pub fn tool_prefix(name: &str) -> String {
    format!("mcp__{}__", crate::runner::claude_server_name(name))
}

/// Where a server comes from: `claude.ai` for a claude.ai connector, else `mcp`. A
/// `source` from `system/init` decides (`claudeai` is a claude.ai connector, so a local
/// server named like one is not); with none, a name that begins `claude.ai ` is one.
fn source_of(name: &str, source: Option<&str>) -> &'static str {
    match source {
        Some("claudeai") => "claude.ai",
        Some(_) => "mcp",
        None if name.starts_with("claude.ai ") => "claude.ai",
        None => "mcp",
    }
}

/// The first URL on a screen, as `capture-pane -e` prints it: an `http://` or `https://`
/// in its text, else an OSC 8 link's target.
fn find_url(screen: &str) -> Option<String> {
    let (text, links) = visible(screen);
    let first = ["https://", "http://"]
        .iter()
        .filter_map(|scheme| text.find(scheme))
        .min();
    if let Some(at) = first {
        let url: String = text[at..]
            .chars()
            .take_while(|&c| !c.is_whitespace() && !c.is_control() && !"\"'<>".contains(c))
            .collect();
        return Some(url.trim_end_matches(['.', ',', ';', ')']).to_owned());
    }
    links
        .into_iter()
        .find(|link| link.starts_with("https://") || link.starts_with("http://"))
}

/// A screen's text without its escape sequences, and the targets of its OSC 8 links.
fn visible(screen: &str) -> (String, Vec<String>) {
    let mut text = String::with_capacity(screen.len());
    let mut links = Vec::new();
    let mut chars = screen.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            text.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                let mut body = String::new();
                while let Some(c) = chars.next() {
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' {
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                    body.push(c);
                }
                if let Some(rest) = body.strip_prefix("8;")
                    && let Some((_, target)) = rest.split_once(';')
                    && !target.is_empty()
                {
                    links.push(target.to_owned());
                }
            }
            _ => {}
        }
    }
    (text, links)
}

/// `url` as rlxd's log may hold it: its query and fragment, where a login's secrets are,
/// left out.
fn without_query(url: &str) -> String {
    let end = url.find(['?', '#']).unwrap_or(url.len());
    let mut kept = url[..end].to_owned();
    if end < url.len() {
        kept.push_str(" (its query is not logged)");
    }
    kept
}

fn code_name(code: LoginErrorCode) -> String {
    serde_json::to_value(code)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn json_of(data: &impl Serialize) -> Value {
    serde_json::to_value(data).unwrap_or(Value::Null)
}

fn random_hex() -> String {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("the OS gives random bytes");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Test builds only: `RLXD_TEST_CONNECTOR_ROUTE=1` makes this rlxd take route 1, as the
/// probe finding it would, and announce `connectors.gate_before_turn`.
#[cfg(feature = "debug-methods")]
fn test_route_1() -> bool {
    std::env::var("RLXD_TEST_CONNECTOR_ROUTE").is_ok_and(|route| route == "1")
}

/// A release build takes route 2 until the probe has found route 1 in Claude Code.
#[cfg(not(feature = "debug-methods"))]
fn test_route_1() -> bool {
    false
}

/// Test builds only: `RLXD_TEST_CONNECTORS_CALL=1` makes this rlxd serve `connectors.call`
/// and announce it, as the owner-run probe confirming Claude Code's route would.
#[cfg(feature = "debug-methods")]
fn test_call_route() -> bool {
    std::env::var("RLXD_TEST_CONNECTORS_CALL").is_ok_and(|on| on == "1")
}

/// A release build makes no `connectors.call` until the owner-run probe confirms the
/// route (spike #5): `connectors.call` is `no_route`, and watch jobs take the fallback.
#[cfg(not(feature = "debug-methods"))]
fn test_call_route() -> bool {
    false
}

/// Test builds only: `RLXD_TEST_CONNECTORS_NO_TURN=1` makes this rlxd read the
/// connectors with no model turn and announce `connectors.no_turn`, as the owner-run
/// probe confirming the route would.
#[cfg(feature = "debug-methods")]
fn test_no_turn() -> bool {
    std::env::var("RLXD_TEST_CONNECTORS_NO_TURN").is_ok_and(|on| on == "1")
}

/// A release build reads no connectors with no turn until the owner-run probe confirms
/// the route (spike #5's `mcp_status`): `refresh` is `no_route` without `allow_turn`, and
/// no account is read in the background.
#[cfg(not(feature = "debug-methods"))]
fn test_no_turn() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_servers_tools_are_those_with_its_prefix() {
        assert_eq!(tool_prefix("claude.ai Slack"), "mcp__claude_ai_Slack__");
        assert_eq!(
            tool_prefix("claude.ai Slack (Beta)"),
            "mcp__claude_ai_Slack_Beta__"
        );
        assert_eq!(tool_prefix("my-server_2"), "mcp__my-server_2__");
    }

    #[test]
    fn init_s_source_decides_where_a_server_comes_from() {
        assert_eq!(source_of("claude.ai Slack", Some("claudeai")), "claude.ai");
        // A local server named like a connector is not one.
        assert_eq!(source_of("claude.ai Slack", Some("local")), "mcp");
        assert_eq!(source_of("claude.ai Slack", None), "claude.ai");
        assert_eq!(source_of("claude.aiSlack", None), "mcp");
    }

    /// A home of its own, removed after.
    struct Dir(PathBuf);

    impl Dir {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("rlxd-conn-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn connectors(&self) -> Arc<Connectors> {
            let bus = crate::bus::Bus::open_at(self.0.join("seq"), crate::bus::Limits::DEFAULT);
            Connectors::with_own_dir(Home::at(&self.0), Arc::new(bus.unwrap()), None)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn seen<'a>(
        servers: &'a [McpServer],
        tools: &'a [String],
        none: &'a (HashMap<String, String>, HashMap<String, bool>),
    ) -> Observed<'a> {
        Observed {
            servers,
            sources: &none.0,
            tools,
            hints: &none.1,
        }
    }

    #[test]
    fn an_init_with_no_server_empties_no_cache() {
        let dir = Dir::new();
        let connectors = dir.connectors();
        let none = (HashMap::new(), HashMap::new());
        let slack = [McpServer::new("claude.ai Slack", "connected")];
        let tools = ["mcp__claude_ai_Slack__read".to_owned()];
        connectors.observe(seen(&slack, &tools, &none));
        connectors.observe(seen(&[], &[], &none));
        let list = connectors.list(0, None);
        assert_eq!(list.connectors.len(), 1);
        assert_eq!(list.connectors[0].tools.len(), 1);
    }

    #[test]
    fn a_change_that_connects_one_is_told_with_every_connected_one() {
        let dir = Dir::new();
        let connectors = dir.connectors();
        let told = Arc::new(Mutex::new(Vec::new()));
        connectors.on_connected(Box::new({
            let told = Arc::clone(&told);
            move |ids| told.lock().unwrap().push(ids.to_vec())
        }));
        let none = (HashMap::new(), HashMap::new());
        let slack = |status| McpServer::new("claude.ai Slack", status);
        let local = McpServer::new("local", "connected");
        for status in ["needs-auth", "needs-auth", "connected", "connected"] {
            connectors.observe(seen(&[slack(status), local.clone()], &[], &none));
        }
        let both = vec!["claude.ai Slack".to_owned(), "local".to_owned()];
        // local's coming, then Slack's sign-in back; nothing for no change.
        assert_eq!(
            *told.lock().unwrap(),
            [vec!["local".to_owned()], both.clone()]
        );
        connectors.set_status("claude.ai Slack", ConnectorState::NeedsAuth);
        connectors.set_status("claude.ai Slack", ConnectorState::Connected);
        assert_eq!(told.lock().unwrap().len(), 3);
        assert_eq!(told.lock().unwrap()[2], both);
    }

    fn managed(slot: u32, login: &str) -> Account {
        Account {
            slot,
            label: Some(format!("slot{slot}")),
            config_dir: Some(PathBuf::from(format!("/accounts/{slot}"))),
            tmp_dir: Some(PathBuf::from(format!("/tmp-accounts/{slot}"))),
            login: Some(login.to_owned()),
        }
    }

    #[test]
    fn each_account_keeps_its_own_connectors_and_the_default_is_the_top_level() {
        let dir = Dir::new();
        let connectors = dir.connectors();
        let none = (HashMap::new(), HashMap::new());
        assert_eq!(connectors.set_accounts(vec![managed(2, "b")]).len(), 2);
        let slack = |status| [McpServer::new("claude.ai Slack", status)];
        connectors.observe(seen(&slack("connected"), &[], &none));
        connectors.observe_on(
            &managed(2, "b"),
            seen(&slack("needs-auth"), &[], &none),
            Read::Listing,
        );
        let list = connectors.list(0, None);
        assert_eq!(list.connectors[0].status, ConnectorState::Connected);
        assert!(list.connectors[0].checked_at.is_some());
        let slots: Vec<u32> = list.accounts.iter().map(|a| a.account).collect();
        assert_eq!(slots, [1, 2]);
        assert_eq!(
            list.accounts[1].connectors[0].status,
            ConnectorState::NeedsAuth
        );
        assert_eq!(list.accounts[1].from, ListedFrom::Listing);
        assert_eq!(list.accounts[1].label.as_deref(), Some("slot2"));
        let only = connectors.list(0, Some(2));
        assert_eq!(only.accounts.len(), 1);
        // Read, the same home and sign-in again: nothing to read; signed in again: 2.
        assert!(connectors.set_accounts(vec![managed(2, "b")]).is_empty());
        let again = connectors.set_accounts(vec![managed(2, "c")]);
        assert_eq!(again.iter().map(|a| a.slot).collect::<Vec<_>>(), [2]);
        // An account no longer listed is dropped.
        connectors.set_accounts(Vec::new());
        assert_eq!(connectors.list(0, None).accounts.len(), 1);
    }

    #[test]
    fn a_file_from_before_accounts_is_the_default_accounts() {
        let dir = Dir::new();
        std::fs::create_dir_all(dir.0.join("rlxd")).unwrap();
        std::fs::write(
            dir.0.join("rlxd").join(FILE),
            r#"{"as_of": "2026-11-27T09:00:00.000Z", "connectors": [{"id": "claude.ai Slack",
                "source": "claude.ai", "status": "needs-auth", "tools": []}]}"#,
        )
        .unwrap();
        let connectors = dir.connectors();
        let list = connectors.list(0, None);
        assert_eq!(list.connectors.len(), 1);
        assert_eq!(list.as_of.as_deref(), Some("2026-11-27T09:00:00.000Z"));
        assert_eq!(list.accounts.len(), 1);
        assert_eq!(list.accounts[0].account, DEFAULT_ACCOUNT);
        assert!(list.connectors[0].identity.is_none());
        // Stale: read a day ago and more, or never.
        assert_eq!(connectors.stale(Duration::from_secs(1)).len(), 1);
        let none = (HashMap::new(), HashMap::new());
        let slack = [McpServer::new("claude.ai Slack", "connected")];
        connectors.observe(seen(&slack, &[], &none));
        assert!(connectors.stale(Duration::from_secs(3600)).is_empty());
    }

    #[test]
    fn under_a_managed_slots_home_a_runs_init_waits_for_the_accounts_then_files_there() {
        let dir = Dir::new();
        let slot = dir.0.join("slot-2");
        std::fs::create_dir_all(&slot).unwrap();
        let bus = crate::bus::Bus::open_at(dir.0.join("seq"), crate::bus::Limits::DEFAULT);
        let connectors =
            Connectors::with_own_dir(Home::at(&dir.0), Arc::new(bus.unwrap()), Some(&slot));
        let none = (HashMap::new(), HashMap::new());
        let slack = [McpServer::new("claude.ai Slack", "connected")];
        connectors.observe(seen(&slack, &[], &none));
        assert!(
            connectors.list(0, None).accounts[0].connectors.is_empty(),
            "held"
        );
        let mut two = managed(2, "b");
        // The same folder by another spelling.
        two.config_dir = Some(slot.join("..").join("slot-2"));
        connectors.set_accounts(vec![two]);
        let list = connectors.list(0, None);
        assert_eq!(row_of(&list, 2).connectors.len(), 1, "filed under slot 2");
        assert!(row_of(&list, 1).connectors.is_empty());
        assert!(
            list.connectors.is_empty(),
            "the top level is the plain account's"
        );
        assert_eq!(connectors.read_only_hint("claude.ai Slack", "x"), None);
    }

    fn row_of(list: &ConnectorsList, slot: u32) -> AccountConnectors {
        list.accounts
            .iter()
            .find(|a| a.account == slot)
            .unwrap()
            .clone()
    }

    #[test]
    fn a_failed_refresh_keeps_the_connectors_with_its_error() {
        let dir = Dir::new();
        let connectors = dir.connectors();
        let none = (HashMap::new(), HashMap::new());
        let slack = [McpServer::new("claude.ai Slack", "connected")];
        connectors.observe(seen(&slack, &[], &none));
        let error = ErrorBody::new("no_route", "no mcp_status").unwrap();
        connectors.refresh_failed(&Account::default_one(), error.clone());
        let list = connectors.list(0, None);
        assert_eq!(list.connectors.len(), 1);
        assert_eq!(list.accounts[0].error.as_ref(), Some(&error));
        connectors.observe(seen(&slack, &[], &none));
        assert_eq!(connectors.list(0, None).accounts[0].error, None);
    }

    #[test]
    fn the_first_url_is_found_in_text_or_a_link() {
        let screen = "Visit\x1b[1m https://claude.ai/oauth?state=s3cret&x=1.\x1b[0m then\n";
        assert_eq!(
            find_url(screen).as_deref(),
            Some("https://claude.ai/oauth?state=s3cret&x=1")
        );
        assert_eq!(
            find_url("go: http://claude.ai/a then https://claude.ai/b").as_deref(),
            Some("http://claude.ai/a")
        );
        let linked = "\x1b]8;;https://claude.ai/l?code=c\x1b\\click here\x1b]8;;\x1b\\\n";
        assert_eq!(
            find_url(linked).as_deref(),
            Some("https://claude.ai/l?code=c")
        );
        assert_eq!(find_url("Authenticating…\nDone.\n"), None);
    }

    #[test]
    fn the_log_never_holds_a_query() {
        assert_eq!(
            without_query("https://claude.ai/oauth?state=s3cret"),
            "https://claude.ai/oauth (its query is not logged)"
        );
        assert_eq!(
            without_query("https://claude.ai/a#frag"),
            "https://claude.ai/a (its query is not logged)"
        );
        assert_eq!(without_query("https://claude.ai/a"), "https://claude.ai/a");
    }
}
