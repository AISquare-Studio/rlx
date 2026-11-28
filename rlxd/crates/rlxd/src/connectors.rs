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
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use rlxd_client::Home;
use rlxd_proto::{
    Connector, ConnectorChanged, ConnectorId, ConnectorLoginDone, ConnectorLoginError,
    ConnectorLoginOpenUrl, ConnectorLoginWaiting, ConnectorState, ConnectorTool, ConnectorsList,
    ConnectorsLoginData, ListedFrom, LoginErrorCode, McpServer, PtyKillParams, PtyScreenParams,
    PtySpawnParams, capabilities, connector_types, topics,
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

/// Told every connector the cache holds `connected`, after a change left one so.
pub type OnConnected = Box<dyn Fn(&[String]) + Send + Sync>;

/// What the file holds.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Saved {
    /// When its `system/init` came.
    as_of: Option<String>,
    /// The connectors, by `id`.
    connectors: Vec<Connector>,
}

/// The home's connectors.
pub struct Connectors {
    home: Home,
    bus: Arc<Bus>,
    cache: Mutex<Saved>,
    /// Whether this Claude Code reports connector states before the first message (5.5,
    /// route 1): the capability `connectors.gate_before_turn`.
    route_1: bool,
    /// Whether this rlxd makes `connectors.call` (capability `connectors.call`, 5.3): off
    /// until the owner-run probe confirms Claude Code's route.
    call_route: bool,
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
        let path = home.rlxd_dir().join(FILE);
        let saved = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                warning!(
                    "connectors: {} is not valid ({e}); the list starts empty",
                    path.display()
                );
                Saved::default()
            }),
            Err(_) => Saved::default(),
        };
        Arc::new(Self {
            home,
            bus,
            cache: Mutex::new(saved),
            route_1: test_route_1(),
            call_route: test_call_route(),
            gate: GateTimes::default(),
            logins: Mutex::new(HashMap::new()),
            on_connected: OnceLock::new(),
        })
    }

    /// Has `told` called, with no lock of the cache's held, with every connector the
    /// cache holds `connected` each time a change leaves one so. The first caller's
    /// stands.
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

    /// Whether a gated try waits for the states before its prompt (route 1).
    pub fn route_1(&self) -> bool {
        self.route_1
    }

    /// Whether this rlxd makes `connectors.call` (5.3).
    pub fn call_route(&self) -> bool {
        self.call_route
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
        if self.call_route {
            named.push(capabilities::CONNECTORS_CALL);
        }
        named
    }

    /// `connectors.list`: the cache, as of its `system/init`, having made `model_turns`.
    pub fn list(&self, model_turns: u32) -> ConnectorsList {
        let cache = self.cache();
        ConnectorsList::new(
            cache.connectors.clone(),
            cache.as_of.clone(),
            ListedFrom::Init,
            model_turns,
        )
    }

    /// Takes a `system/init` as the connectors now: its `servers`, with each one's
    /// `source` where it carried one (`sources`), and the `tools` it mounted, with each
    /// tool's `readOnlyHint` where it carried one (`hints`). The cache and its file are
    /// replaced, then each change is published. An init that lists no server changes
    /// nothing a cache that holds some.
    pub fn observe(&self, init: Observed<'_>) {
        let Observed {
            servers,
            sources,
            tools,
            hints,
        } = init;
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
            );
            now.insert(server.name.clone(), connector);
        }
        let mut cache = self.cache();
        if now.is_empty() && !cache.connectors.is_empty() {
            // It can't be told from a Claude Code that loaded no server (E9's stdin at
            // EOF, a `no_tools` start), so it empties nothing.
            return;
        }
        let before: BTreeMap<&str, ConnectorState> = cache
            .connectors
            .iter()
            .map(|c| (c.id.as_str(), c.status))
            .collect();
        let mut events: Vec<(&str, Value)> = Vec::new();
        for (id, connector) in &now {
            match before.get(id.as_str()) {
                None => events.push((connector_types::ADDED, json_of(&ConnectorId::new(id)))),
                Some(&previous) if previous != connector.status => events.push((
                    connector_types::CHANGED,
                    json_of(&ConnectorChanged::new(id, connector.status, previous)),
                )),
                Some(_) => {}
            }
        }
        for id in before.keys() {
            if !now.contains_key(*id) {
                events.push((connector_types::REMOVED, json_of(&ConnectorId::new(*id))));
            }
        }
        let connected_now = now.iter().any(|(id, connector)| {
            connector.status == ConnectorState::Connected
                && before.get(id.as_str()) != Some(&ConnectorState::Connected)
        });
        cache.connectors = now.into_values().collect();
        cache.as_of = Some(clock::now());
        // Kept, then published, both under the cache's lock: two inits' changes are
        // written and published in their order, and none is published before it is
        // kept.
        self.save(&cache);
        for (r#type, data) in events {
            self.publish(r#type, data);
        }
        let connected = connected_now.then(|| connected_in(&cache));
        drop(cache);
        if let Some(connected) = connected {
            self.tell_connected(&connected);
        }
    }

    /// Sets `id`'s status, when the cache has it: a login's end.
    fn set_status(&self, id: &str, status: ConnectorState) {
        let mut cache = self.cache();
        let Some(connector) = cache.connectors.iter_mut().find(|c| c.id == id) else {
            return;
        };
        let previous = connector.status;
        if previous == status {
            return;
        }
        connector.status = status;
        self.save(&cache);
        self.publish(
            connector_types::CHANGED,
            json_of(&ConnectorChanged::new(id, status, previous)),
        );
        let connected = (status == ConnectorState::Connected).then(|| connected_in(&cache));
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

/// The connectors `saved` holds `connected`.
fn connected_in(saved: &Saved) -> Vec<String> {
    saved
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
            Connectors::new(Home::at(&self.0), Arc::new(bus.unwrap()))
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
        let list = connectors.list(0);
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
