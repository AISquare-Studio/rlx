//! What rlxd says on a connection (handoff 0.10.3, 7.2): `hello` first, then requests,
//! each answered with the envelope plus its `id`.
//!
//! - **`hello`.** A `protocol_version` this rlxd does not serve gets
//!   `incompatible_protocol`, read from the raw params before anything else, so even a
//!   hello this rlxd cannot parse gets it. Anything but a valid hello with the token gets
//!   `unauthorized`. A valid one with `role: app` also needs a paired device's
//!   `gate_credential` and, for a device paired locally, a peer that passes the identity
//!   check (`peer`; only the credential in gate dev mode), or it gets `not_gate`. A
//!   refused hello closes the connection and is never counted.
//! - **Requests.** A frame that is not a request gets `invalid_request` and the
//!   connection goes on. An unknown method gets `unknown_method`, and params a method
//!   cannot take get `invalid_params`.
//! - **The event stream** (7.3). `events.subscribe` makes the connection carry the
//!   stream: the events, then the subscribe's reply `{seq}`, which ends it. Without
//!   `follow` the reply comes after the replay, and the connection goes on taking
//!   requests; with it, when rlxd stops. Requests sent meanwhile are answered between
//!   the events, but a second `events.subscribe` gets `invalid_request`: one stream per
//!   connection, so event frames need no `id`; it is refused before its params are read.
//!   Frames are read as they come, so a partial one holds up no event, but a slow request,
//!   such as a `status` that asks Claude Code, delays that connection's events until it
//!   is answered. A topic the bus does not carry gets `unknown_topic`, and a `since` past
//!   the last event `since_ahead`, with `detail.head`. A stream without `follow` ends at
//!   the last event published when it was asked for. How a slow client is kept from
//!   holding up the rest is the bus's (`crate::bus`).
//! - **The gate** (`gate`): `gate.pair`, `gate.pair_requests`, `gate.pair_answer`,
//!   `gate.devices` and `gate.unpair`. An app connection whose credential is replaced, or
//!   whose device is unpaired, gets a last `not_gate` envelope (with no `id`) and is
//!   closed, also while it carries a stream: that envelope then ends the stream.
//! - **Stopping.** `daemon.stopping {reason}` is published before any connection learns
//!   of the stop, so every stream carries it before its reply.
//! - **`debug.emit {count}`**, in test builds only (the `debug-methods` feature), publishes
//!   `count` `daemon.debug {index, count}` events. A release build answers
//!   `unknown_method`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rlxd_client::Home;
use rlxd_proto::{
    Claude, Clients, Data, Envelope, ErrorBody, HelloData, HelloParams, PROTOCOL_VERSION,
    PROTOCOL_VERSIONS, Params, Paths, Request, RequestId, Role, RunRef, RunStartParams,
    SettingsDeleteParams, SettingsGetParams, SettingsListParams, SettingsSetParams,
    SpaceEnsureData, SpaceParams, StatusData, SubscribeParams, VERSION, codes, event_types,
    methods, setting_keys, topics,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::watch;

use crate::bus::{self, Bus, Cursor, Line, Topics};
use crate::gate::{self, AnswerError, Gate, Hold, Refused, Route, UnpairError};
use crate::listener::Hangup;
use crate::log::{info, warning};
use crate::peer::Peer;
use crate::runner::Runner;
use crate::settings::{self, Settings};
use crate::{claude, spaces, token};

/// `daemon.stopping`'s `reason` when a client asked rlxd to stop.
pub const STOP_REQUESTED: &str = "stop";

/// The longest frame rlxd reads: 1 MiB. A longer one gets `invalid_request`, and the
/// connection closes.
pub const MAX_FRAME: usize = 1 << 20;

/// How long a new connection has to send its `hello`.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// How long rlxd waits, after its last reply on a connection it is closing, for the
/// client to read it and close first.
const LINGER: Duration = Duration::from_secs(1);

/// How long a `claude` probe's answer is reused, so a client polling `status` does not
/// start Claude Code several times a second.
const CLAUDE_FRESH: Duration = Duration::from_secs(5);

/// What this rlxd can do, by name, for `hello`'s reply and `status`. The later items add
/// theirs.
const CAPABILITIES: &[&str] = &[];

/// One running rlxd: what every connection shares.
pub struct Daemon {
    home: Home,
    token: String,
    pid: u32,
    pipe_acl: Option<&'static str>,
    clients: Mutex<Clients>,
    claude: tokio::sync::Mutex<Option<(Instant, Claude)>>,
    bus: Arc<Bus>,
    runner: Arc<Runner>,
    settings: Settings,
    stopping: AtomicBool,
    stop: watch::Sender<bool>,
    gate: Arc<Gate>,
}

/// What a connection was let in as.
struct Session {
    protocol_version: u32,
    role: Role,
    peer: Arc<Peer>,
    /// The `client` its hello named, for the log.
    client: String,
    /// For `role: app`: its hold on the gate, which names its paired device. App
    /// connections are counted by the gate.
    app: Option<Hold>,
    _counted: Option<Counted>,
}

/// A connection counted in `status`'s `clients` while it is open.
struct Counted {
    daemon: Arc<Daemon>,
    role: Role,
}

/// What a request gets.
enum Answer {
    /// A reply, and the connection goes on.
    Reply(Envelope),
    /// A reply, then rlxd stops.
    Stop(Envelope),
    /// A reply that carries a new gate credential: see [`Minted`].
    Paired(Envelope, Minted),
    /// The event stream, whose last frame is the reply.
    Stream(Subscription),
}

/// A gate credential `gate.pair` minted for `device`, whose SHA-256 is `sha256`. If its
/// reply can't be delivered, the pairing is undone, so nobody is left paired with a
/// credential nobody received. With `close`, the connection's own credential was the one
/// replaced: it closes after the reply.
struct Minted {
    device: String,
    sha256: String,
    replaced: Option<(String, String)>,
    close: bool,
}

/// An `events.subscribe` that rlxd took: its reply's id, and its place, which ends at
/// the stream's head now unless it follows.
struct Subscription {
    id: RequestId,
    cursor: Cursor,
}

/// What the connection does once a stream has ended.
enum Flow {
    /// Takes the next request.
    GoOn,
    /// rlxd is stopping: waits for the client to close, then closes.
    Linger,
    /// The connection was dropped from the gate: says why, then closes.
    Dropped,
    /// The client has gone, or cannot be written to.
    Close,
}

/// One frame, as read.
enum Frame {
    Line(String),
    Blank,
    NotText,
    TooLong,
}

impl Daemon {
    /// An rlxd serving `home` with `token`, whose gate is `gate`, publishing on `bus` and
    /// serving `settings`.
    /// `pipe_acl` is what `status` says of the pipe's DACL, on Windows.
    pub fn new(
        home: Home,
        token: String,
        pipe_acl: Option<&'static str>,
        gate: Gate,
        bus: Bus,
        settings: Settings,
    ) -> Arc<Self> {
        let bus = Arc::new(bus);
        let runner = Runner::new(home.clone(), Arc::clone(&bus));
        Arc::new(Self {
            home,
            token,
            pid: std::process::id(),
            pipe_acl,
            clients: Mutex::new(Clients::default()),
            claude: tokio::sync::Mutex::new(None),
            bus,
            runner,
            settings,
            stopping: AtomicBool::new(false),
            stop: watch::Sender::new(false),
            gate: Arc::new(gate),
        })
    }

    /// The home's event bus.
    pub fn bus(&self) -> &Bus {
        &self.bus
    }

    /// Publishes `daemon.started {version, pid}`, the first event of this rlxd's life.
    pub fn started(&self) {
        let data = object(&json!({"version": VERSION, "pid": self.pid}));
        if let Err(e) = self
            .bus
            .publish(topics::DAEMON, event_types::DAEMON_STARTED, data)
        {
            warning!("daemon.started was not published: {e}");
        }
    }

    /// Tells every task to stop, the accept loop and each connection, once
    /// `daemon.stopping {reason}` is published: `reason` is a snake_case word, such as
    /// [`STOP_REQUESTED`] or `sigterm`.
    pub fn stop(&self, reason: &str) {
        if self.stopping.swap(true, Ordering::SeqCst) {
            return;
        }
        info!("stopping: {reason}");
        // The stop is signalled when this goes, even if publishing panics: an rlxd that
        // was asked to stop always does.
        struct Signal<'a>(&'a watch::Sender<bool>);
        impl Drop for Signal<'_> {
            fn drop(&mut self) {
                self.0.send_replace(true);
            }
        }
        let _signal = Signal(&self.stop);
        let data = object(&json!({ "reason": reason }));
        if let Err(e) = self
            .bus
            .publish(topics::DAEMON, event_types::DAEMON_STOPPING, data)
        {
            warning!("daemon.stopping was not published: {e}");
        }
    }

    /// Writes what only memory holds, before rlxd exits: when each gate device was last
    /// seen. Ends the runs in flight first, killing their process trees: runs don't
    /// outlive rlxd.
    pub fn flush(&self) {
        self.runner.shutdown();
        self.gate.flush();
    }

    /// Resolves once [`Daemon::stop`] was called.
    pub async fn stopped(&self) {
        let mut stopping = self.stop.subscribe();
        let _ = stopping.wait_for(|stop| *stop).await;
    }

    fn clients(&self) -> MutexGuard<'_, Clients> {
        self.clients
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// What `hello` makes of the first frame: a session and its reply, or the refusal.
    async fn hello(
        self: &Arc<Self>,
        frame: &str,
        peer: Arc<Peer>,
    ) -> Result<(Session, Envelope), Envelope> {
        let refuse = |id, message: String| {
            info!("hello refused: {message}");
            Err(reply_err(id, codes::UNAUTHORIZED, message))
        };
        let Ok(value) = serde_json::from_str::<Value>(frame) else {
            return refuse(
                None,
                "the first frame must be a hello request, as JSON".into(),
            );
        };
        let id = request_id(&value);
        let request = match Request::deserialize(value) {
            Ok(request) => request,
            Err(e) => return refuse(id, format!("the first frame must be a hello request: {e}")),
        };
        if request.method != methods::HELLO {
            let message = format!("the first frame must be hello, not {}", request.method);
            return refuse(id, message);
        }
        if let Some(Value::Number(version)) = request.params.get("protocol_version") {
            let served = version
                .as_u64()
                .is_some_and(|v| PROTOCOL_VERSIONS.iter().any(|s| u64::from(*s) == v));
            if !served {
                info!("hello refused: protocol version {version}");
                let error = ErrorBody::incompatible_protocol(version);
                return Err(Envelope::err(error).with_optional_id(id));
            }
        }
        let mut params = request.params;
        // An empty gate_credential presents no credential: role app then gets not_gate,
        // as with none, rather than a parse error's unauthorized.
        if params.get("gate_credential") == Some(&Value::String(String::new())) {
            params.remove("gate_credential");
        }
        let hello = match HelloParams::deserialize(Value::Object(params)) {
            Ok(hello) => hello,
            Err(e) => return refuse(id, format!("hello's params are not valid: {e}")),
        };
        if !token::matches(&self.token, &hello.token) {
            info!("hello refused: a wrong token, from {}", hello.client);
            let hint = format!("rlxd's token is in {}.", self.home.token().display());
            let error = error(codes::UNAUTHORIZED, "the token is wrong").with_hint(hint);
            return Err(Envelope::err(error).with_optional_id(id));
        }
        let (app, counted) = if hello.role == Role::App {
            match self.admit_app(&hello, &peer).await {
                Ok(hold) => (Some(hold), None),
                Err(message) => {
                    info!(
                        "hello refused: role app, from {} ({peer}): {message} (not_gate)",
                        hello.client
                    );
                    let error = error(codes::NOT_GATE, &message).with_hint(NOT_GATE_HINT);
                    return Err(Envelope::err(error).with_optional_id(id));
                }
            }
        } else {
            let Some(counted) = Counted::new(self, hello.role) else {
                let message = format!("role {} is not one this rlxd serves", role_name(hello.role));
                return refuse(id, message);
            };
            (None, Some(counted))
        };
        info!(
            "hello: {} {} as {}{}",
            hello.client,
            hello.client_version,
            role_name(hello.role),
            app.as_ref()
                .map(|hold| format!(", device {:?}", hold.device()))
                .unwrap_or_default()
        );
        let reply = HelloData::new(PROTOCOL_VERSION, VERSION, capabilities())
            .map(|data| Envelope::ok(object(&data)).with_id(request.id))
            .map_err(|e| reply_err(id, codes::INTERNAL, e.to_string()))?;
        let session = Session {
            protocol_version: hello.protocol_version,
            role: hello.role,
            client: hello.client,
            peer,
            app,
            _counted: counted,
        };
        Ok((session, reply))
    }

    /// Whether a `hello {role: "app"}` may hold the gate: a paired device's credential,
    /// a peer of rlxd's user, and for a device paired locally, a peer that passes the
    /// identity check (outside dev mode). Its hold, or why not.
    async fn admit_app(&self, hello: &HelloParams, peer: &Arc<Peer>) -> Result<Hold, String> {
        let Some(credential) = hello.gate_credential.as_deref() else {
            return Err("role app needs a gate_credential, which gate.pair mints".to_owned());
        };
        let Some((device, route)) = self.gate.device_of(credential) else {
            return Err(
                "the gate_credential is not one this rlxd paired: it is wrong, was \
                        replaced, or its device was unpaired"
                    .to_owned(),
            );
        };
        peer.same_user()?;
        match route {
            // A device paired over SSH would be admitted on its credential alone, its peer
            // being sshd. SSH pairing isn't built (hub q1), so rlxd never minted one, and
            // such an entry in gate.json was written by something else: fail closed.
            Route::Ssh => return Err(gate::ssh_refused(&device)),
            Route::Local if self.gate.dev_mode() => warning!(
                "gate dev mode: hello role app from device {device:?} ({peer}) let in \
                 without the peer identity check"
            ),
            Route::Local => {
                let how = verify(peer).await?;
                info!("gate: device {device:?}: {how}");
            }
        }
        self.gate
            .attach(credential)
            .ok_or_else(|| "the gate_credential was replaced or unpaired meanwhile".to_owned())
    }

    /// What one request frame after `hello` gets. On a connection that carries a stream
    /// already, `events.subscribe` gets `invalid_request`, whatever its params.
    async fn answer(&self, session: &Session, frame: &str, streaming: bool) -> Answer {
        let value = match serde_json::from_str::<Value>(frame) {
            Ok(value) => value,
            Err(e) => {
                let message = format!("the frame is not JSON: {e}");
                return Answer::Reply(reply_err(None, codes::INVALID_REQUEST, message));
            }
        };
        let id = request_id(&value);
        let request = match Request::deserialize(value) {
            Ok(request) => request,
            Err(e) => {
                let message = format!("the frame is not a request: {e}");
                return Answer::Reply(reply_err(id, codes::INVALID_REQUEST, message));
            }
        };
        let id = request.id;
        let outcome = match request.method.as_str() {
            methods::STATUS => self.status(session).await,
            methods::RUN_START | methods::RUN_CANCEL | methods::SPACE_ENSURE => {
                self.runs_and_spaces(session, &request.method, request.params)
                    .await
            }
            methods::STOP => {
                let data = object(&json!({"pid": self.pid}));
                return Answer::Stop(Envelope::ok(data).with_id(id));
            }
            methods::EVENTS_SUBSCRIBE if streaming => Err(error(
                codes::INVALID_REQUEST,
                "this connection carries an event stream already: open another for a second",
            )),
            methods::EVENTS_SUBSCRIBE => match self.subscribe(id, request.params) {
                Ok(subscription) => return Answer::Stream(subscription),
                Err(error) => Err(error),
            },
            #[cfg(feature = "debug-methods")]
            methods::DEBUG_EMIT => self.debug_emit(&request.params).await,
            methods::SETTINGS_GET => self.settings_get(request.params),
            methods::SETTINGS_SET => self.settings_set(session, request.params),
            methods::SETTINGS_LIST => self.settings_list(request.params),
            methods::SETTINGS_DELETE => self.settings_delete(session, request.params),
            methods::GATE_PAIR => match self.gate_pair(session, &request.params).await {
                Ok((data, minted)) => {
                    return Answer::Paired(Envelope::ok(data).with_id(id), minted);
                }
                Err(error) => Err(error),
            },
            methods::GATE_PAIR_REQUESTS => self.gate_pair_requests(session),
            methods::GATE_PAIR_ANSWER => self.gate_pair_answer(session, &request.params),
            methods::GATE_DEVICES => Ok(object(&json!({"devices": self.gate.devices()}))),
            methods::GATE_UNPAIR => self.gate_unpair(session, &request.params),
            methods::HELLO => Err(error(
                codes::INVALID_REQUEST,
                "hello was already said on this connection",
            )),
            method => Err(error(
                codes::UNKNOWN_METHOD,
                &format!("rlxd has no method {method:?}"),
            )
            .with_hint(format!("The methods it serves: {}.", served().join(", ")))),
        };
        Answer::Reply(match outcome {
            Ok(data) => Envelope::ok(data).with_id(id),
            Err(error) => Envelope::err(error).with_id(id),
        })
    }

    /// `events.subscribe`, taken: where its stream starts. Every topic named must be
    /// one the bus carries, and `since` one it has reached.
    fn subscribe(&self, id: RequestId, params: Params) -> Result<Subscription, ErrorBody> {
        let invalid = |e: &dyn std::fmt::Display| {
            error(
                codes::INVALID_PARAMS,
                &format!("events.subscribe's params are not valid: {e}"),
            )
        };
        // A topic the bus does not carry is unknown_topic, whatever its spelling.
        if let Some(Value::Array(names)) = params.get("topics")
            && let Some(unknown) = names
                .iter()
                .filter_map(Value::as_str)
                .find(|name| !topics::ALL.contains(name))
        {
            return Err(unknown_topic(unknown));
        }
        let params =
            SubscribeParams::deserialize(Value::Object(params)).map_err(|e| invalid(&e))?;
        let topics = match &params.topics {
            None => Topics::ALL,
            Some(names) => Topics::of(names.iter().map(String::as_str)).map_err(unknown_topic)?,
        };
        let cursor = self
            .bus
            .cursor(topics, params.since, params.follow)
            .map_err(|head| ErrorBody::since_ahead(params.since.unwrap_or_default(), head))?;
        Ok(Subscription { id, cursor })
    }

    /// `settings.get {key}`. Every settings call runs as blocking work (`off_worker`): a
    /// change writes and fsyncs the file under the settings lock, which a read may wait
    /// on, and neither may hold up the connections the runtime's workers serve.
    fn settings_get(&self, params: Params) -> Result<Data, ErrorBody> {
        let params = params_of::<SettingsGetParams>(methods::SETTINGS_GET, params)?;
        let setting = off_worker(|| self.settings.get(&params.key)).map_err(refused)?;
        printed(&setting)
    }

    /// `settings.set {key, value, expected_version?}`: only an app-role connection that
    /// holds the gate, a paired device, sets a protected key, `send_safety_mode` or one
    /// under `policy.`. One whose device was unpaired, or whose credential was replaced,
    /// no longer does.
    fn settings_set(&self, session: &Session, params: Params) -> Result<Data, ErrorBody> {
        let params = params_of::<SettingsSetParams>(methods::SETTINGS_SET, params)?;
        let who = format!("{} {}", role_name(session.role), session.client);
        let setting = off_worker(|| self.settings.set(params, writer(session, &who), &self.bus))
            .map_err(refused)?;
        printed(&setting)
    }

    /// `settings.delete {key, expected_version?}`, by the same rules as `settings.set`.
    fn settings_delete(&self, session: &Session, params: Params) -> Result<Data, ErrorBody> {
        let params = params_of::<SettingsDeleteParams>(methods::SETTINGS_DELETE, params)?;
        let who = format!("{} {}", role_name(session.role), session.client);
        let setting = off_worker(|| {
            self.settings
                .delete(params, writer(session, &who), &self.bus)
        })
        .map_err(refused)?;
        printed(&setting)
    }

    /// `settings.list {prefix?}`.
    fn settings_list(&self, params: Params) -> Result<Data, ErrorBody> {
        let params = params_of::<SettingsListParams>(methods::SETTINGS_LIST, params)?;
        let list = off_worker(|| self.settings.list(params.prefix.as_deref())).map_err(refused)?;
        printed(&list)
    }

    /// `debug.emit {count}`: `count` events, `daemon.debug {index, count}`, published one
    /// at a time, letting other tasks run every [`bus::BATCH`] of them, as any publisher
    /// that loops must, or its followers starve on rlxd's two workers. Test builds only.
    #[cfg(feature = "debug-methods")]
    async fn debug_emit(&self, params: &Params) -> Result<Data, ErrorBody> {
        const MOST: u64 = 100_000;
        let count = params
            .get("count")
            .and_then(Value::as_u64)
            .filter(|count| (1..=MOST).contains(count))
            .ok_or_else(|| {
                error(
                    codes::INVALID_PARAMS,
                    &format!("debug.emit takes {{\"count\": N}}, N from 1 to {MOST}"),
                )
            })?;
        let mut first = None;
        let mut last = 0;
        for index in 1..=count {
            let data = object(&json!({"index": index, "count": count}));
            last = self
                .bus
                .publish(topics::DAEMON, "daemon.debug", data)
                .map_err(|e| error(codes::INTERNAL, &e.to_string()))?;
            first.get_or_insert(last);
            if index % bus::BATCH as u64 == 0 {
                tokio::task::yield_now().await;
            }
        }
        Ok(object(&json!({"first": first, "last": last})))
    }

    /// `status`'s reply data (7.4).
    async fn status(&self, session: &Session) -> Result<Data, ErrorBody> {
        let home = &self.home;
        let paths = Paths::new(
            home.root().display().to_string(),
            home.endpoint(),
            home.lock().display().to_string(),
            home.token().display().to_string(),
            home.logs().display().to_string(),
        );
        let mut status = StatusData::new(VERSION, self.pid, std::env::consts::OS, paths)
            .map_err(|e| error(codes::INTERNAL, &e.to_string()))?;
        status.protocol_version = session.protocol_version;
        status.capabilities = capabilities();
        status.seq = self.bus.head();
        status.clients = *self.clients();
        status.clients.app = self.gate.app_connections();
        if self.gate.dev_mode() {
            StatusData::GATE_DEV_MODE.clone_into(&mut status.gate);
        }
        status.dev_features = crate::dev_features();
        status.claude = self.claude().await;
        (status.runs.live, status.runs.queued) = self.runner.counts();
        status.pipe_acl = self.pipe_acl.map(str::to_owned);
        match serde_json::to_value(&status) {
            Ok(Value::Object(data)) => Ok(data),
            Ok(_) => Err(error(codes::INTERNAL, "status did not print as an object")),
            Err(e) => Err(error(codes::INTERNAL, &format!("status is not valid: {e}"))),
        }
    }

    /// What Claude Code says about itself, asked at most every [`CLAUDE_FRESH`]. Calls
    /// that come while it is being asked wait for that answer.
    async fn claude(&self) -> Claude {
        let mut cached = self.claude.lock().await;
        if let Some((at, claude)) = cached.as_ref()
            && at.elapsed() < CLAUDE_FRESH
        {
            return claude.clone();
        }
        let claude = claude::probe().await;
        *cached = Some((Instant::now(), claude.clone()));
        claude
    }

    /// The runner's methods (handoff 0.10.3, 7.5) and spaces' (2.5): `run.start`,
    /// `run.cancel` and `space.ensure`. A run's client, in `run.list`, is the `client` the
    /// connection's hello named.
    async fn runs_and_spaces(
        &self,
        session: &Session,
        method: &str,
        params: Params,
    ) -> Result<Data, ErrorBody> {
        match method {
            methods::RUN_START => {
                let params = params_of::<RunStartParams>(method, params)?;
                printed(&self.runner.start(params, &session.client, None)?)
            }
            methods::RUN_CANCEL => {
                let params = params_of::<RunRef>(method, params)?;
                printed(&self.runner.cancel(&params.run_id).await?)
            }
            _ => {
                let params = params_of::<SpaceParams>(method, params)?;
                let path =
                    off_worker(|| spaces::ensure(&self.home, &params.name)).map_err(|e| {
                        error(
                            codes::INTERNAL,
                            &format!("the space {:?} could not be made: {e}", params.name),
                        )
                    })?;
                printed(&SpaceEnsureData::new(
                    params.name,
                    path.display().to_string(),
                ))
            }
        }
    }
}

/// What `gate.pair` and `gate.pair_answer` take.
#[derive(Deserialize)]
struct PairParams {
    #[serde(default)]
    device: Option<String>,
    #[serde(default)]
    gate_credential: Option<String>,
    #[serde(default)]
    route: Option<String>,
}

#[derive(Deserialize)]
struct AnswerParams {
    request_id: String,
    approve: bool,
    /// Approve a request for a name the user revoked.
    #[serde(default)]
    force: bool,
}

/// Withdraws a pairing request when dropped: when its wait ends, however it ends.
struct Withdraw<'a> {
    gate: &'a Gate,
    request_id: &'a str,
}

impl Drop for Withdraw<'_> {
    fn drop(&mut self) {
        self.gate.withdraw(self.request_id);
    }
}

#[derive(Deserialize)]
struct UnpairParams {
    #[serde(default)]
    device: Option<String>,
}

/// What a `not_gate` refusal says to do.
const NOT_GATE_HINT: &str = "Only rlx's app holds the gate. A paired app presents its \
    gate_credential; to pair again, the user runs `rlxd gate unpair` on this host first.";

impl Daemon {
    /// `gate.pair` (5.1): mints a device's credential, for a peer that passes the
    /// identity check (any peer of rlxd's user, in dev mode).
    async fn gate_pair(
        &self,
        session: &Session,
        params: &Data,
    ) -> Result<(Data, Minted), ErrorBody> {
        let params: PairParams = parse(params)?;
        let refuse = |message: String| {
            info!("gate.pair refused ({}): {message} (not_gate)", session.peer);
            Err(error(codes::NOT_GATE, &message).with_hint(NOT_GATE_HINT))
        };
        if let Some(route) = params.route.as_deref()
            && route != Route::Local.name()
        {
            return refuse(format!(
                "a connection to rlxd's socket pairs only route local, not {route:?}: an \
                 SSH device pairs from inside its SSH session, never through the socket"
            ));
        }
        let device = match params.device.as_deref().map(gate::device_name) {
            None => None,
            Some(Ok(device)) => Some(device),
            Some(Err(message)) => return Err(error(codes::INVALID_PARAMS, &message)),
        };
        if let Err(message) = session.peer.same_user() {
            return refuse(message);
        }
        if self.gate.dev_mode() {
            warning!(
                "gate dev mode: gate.pair from {} without the peer identity check",
                session.peer
            );
        } else {
            match verify(&session.peer).await {
                Ok(how) => info!("gate.pair: {how}"),
                Err(message) => return refuse(message),
            }
        }
        let caller = session.app.as_ref().map(Hold::id);
        let result = self
            .gate
            .pair(device.as_deref(), params.gate_credential.as_deref(), caller);
        let paired = match result {
            Ok(paired) => paired,
            Err(Refused::NeedsApproval(device)) => {
                if let Err(message) = self.await_approval(&device, &session.peer).await {
                    return refuse(message);
                }
                // The wait can be long: check the peer again before minting.
                if !self.gate.dev_mode()
                    && let Err(message) = verify(&session.peer).await
                {
                    return refuse(format!("after the approval, {message}"));
                }
                match self.gate.pair_approved(&device) {
                    Ok(paired) => paired,
                    Err(Refused::NotGate(message) | Refused::Failed(message)) => {
                        return refuse(message);
                    }
                    Err(Refused::NeedsApproval(_)) => {
                        return refuse("it needs another approval".to_owned());
                    }
                }
            }
            Err(Refused::NotGate(message)) => return refuse(message),
            Err(Refused::Failed(message)) => {
                warning!("gate.pair failed: {message}");
                return Err(error(codes::INTERNAL, &message));
            }
        };
        if params.gate_credential.is_some() {
            warning!(
                "gate: device {:?} paired again; its old credential is refused from now on, \
                 and {} app connection(s) that used it were dropped",
                paired.device,
                paired.dropped
            );
        } else {
            info!(
                "gate: device {:?} paired, route {}",
                paired.device,
                paired.route.name()
            );
        }
        let data = json!({
            "gate_credential": paired.credential,
            "device": paired.device,
            "route": paired.route.name(),
        });
        let minted = Minted {
            device: paired.device,
            sha256: paired.sha256,
            replaced: paired.replaced,
            close: paired.drop_caller,
        };
        Ok((object(&data), minted))
    }

    /// Waits for a paired device or the user to approve a new device's pairing. The
    /// request is withdrawn however the wait ends, also when this future is dropped
    /// because the requester left.
    async fn await_approval(&self, device: &str, peer: &Peer) -> Result<(), String> {
        let request = match self.gate.request(device, peer.pid(), peer.image()) {
            Ok(request) => request,
            Err(Refused::NotGate(message) | Refused::Failed(message)) => return Err(message),
            Err(Refused::NeedsApproval(_)) => unreachable!("a request never needs another"),
        };
        let id = request.request_id.clone();
        let _withdraw = Withdraw {
            gate: &self.gate,
            request_id: &id,
        };
        warning!(
            "gate: device {device:?} ({peer}) asks to pair as request {id}; a paired device \
             approves it, or the user runs `rlxd gate pair --request-id {id}` on this host, \
             within {} s",
            gate::PAIR_WAIT.as_secs()
        );
        let answered = tokio::select! {
            answered = tokio::time::timeout(gate::PAIR_WAIT, request.answer) => Some(answered),
            () = self.stopped() => None,
        };
        match answered {
            // gate.pair_answer logged who approved it.
            Some(Ok(Ok(gate::Answer::Approved))) => Ok(()),
            Some(Ok(Ok(gate::Answer::Denied(by)))) => {
                Err(format!("pairing device {device:?} was denied by {by}"))
            }
            Some(Ok(Err(_))) | None => Err("rlxd is stopping".to_owned()),
            Some(Err(_)) => Err(format!(
                "pairing device {device:?} needs an approval: a paired device, or `rlxd gate \
                 pair --request-id {id}` on this host, within {} s, and none came",
                gate::PAIR_WAIT.as_secs()
            )),
        }
    }

    /// `gate.pair_requests`: for a paired app, or the user (a cli or tui client).
    fn gate_pair_requests(&self, session: &Session) -> Result<Data, ErrorBody> {
        self.may_answer(session)?;
        Ok(object(&json!({"requests": self.gate.requests()})))
    }

    /// `gate.pair_answer`: a paired app, or the user (a cli or tui client), approves or
    /// denies a waiting request.
    fn gate_pair_answer(&self, session: &Session, params: &Data) -> Result<Data, ErrorBody> {
        let by = self.may_answer(session)?;
        let params: AnswerParams = parse(params)?;
        let request_id = params.request_id;
        let device = self
            .gate
            .answer(&request_id, params.approve, params.force, &by)
            .map_err(|e| match e {
                AnswerError::NoRequest => {
                    let waiting = self.gate.requests().len();
                    error(
                        codes::NO_PAIR_REQUEST,
                        &format!("no pairing request {request_id:?} waits ({waiting} wait)"),
                    )
                    .with_hint("`rlxd gate pair` lists the waiting requests.")
                }
                AnswerError::Revoked(device) => error(
                    codes::REVOKED_DEVICE,
                    &format!(
                        "the user unpaired a device named {device:?}: approving a new device \
                         under that name needs force"
                    ),
                )
                .with_hint("If this is that device, approve it with `--force`."),
            })?;
        info!(
            "gate: pairing request {request_id} for {device:?} {} by {by}",
            if params.approve { "approved" } else { "denied" }
        );
        Ok(object(&json!({
            "request_id": request_id,
            "device": device,
            "approved": params.approve,
        })))
    }

    /// Who answers for `session`: its device, or the user. `not_gate` for anyone else.
    fn may_answer(&self, session: &Session) -> Result<String, ErrorBody> {
        if let Some(hold) = &session.app {
            return Ok(format!("device {:?}", hold.device()));
        }
        match session.role {
            Role::Cli | Role::Tui => Ok("the user on this host".to_owned()),
            _ => Err(error(
                codes::NOT_GATE,
                "only a paired app, or the user on this host, answers pairing requests",
            )),
        }
    }

    /// `gate.unpair`: the user's (a cli or tui client), never the app's.
    fn gate_unpair(&self, session: &Session, params: &Data) -> Result<Data, ErrorBody> {
        if !matches!(session.role, Role::Cli | Role::Tui) {
            return Err(error(
                codes::NOT_GATE,
                "gate.unpair is the user's: run `rlxd gate unpair` on this host",
            ));
        }
        let params: UnpairParams = parse(params)?;
        let (names, dropped) = self
            .gate
            .unpair(params.device.as_deref())
            .map_err(|e| match e {
                UnpairError::Unknown(name) => error(
                    codes::UNKNOWN_DEVICE,
                    &format!("no device {name:?} is paired"),
                )
                .with_hint("`rlxd gate devices` lists the paired devices."),
                UnpairError::Failed(message) => error(codes::INTERNAL, &message),
            })?;
        let which = match params.device.as_deref() {
            Some(name) => format!("device {name:?}"),
            None => format!("every device ({})", names.join(", ")),
        };
        warning!("gate unpaired: {which}, by the user; {dropped} app connection(s) dropped");
        Ok(object(&json!({"unpaired": names, "dropped": dropped})))
    }
}

/// Checks `peer`'s identity off the async runtime.
async fn verify(peer: &Arc<Peer>) -> Result<String, String> {
    let peer = Arc::clone(peer);
    tokio::task::spawn_blocking(move || peer.verify_identity())
        .await
        .unwrap_or_else(|e| Err(format!("the peer check failed: {e}")))
}

/// A method's params, as `T`, or `invalid_params` saying why not.
fn parse<T: serde::de::DeserializeOwned>(params: &Data) -> Result<T, ErrorBody> {
    T::deserialize(Value::Object(params.clone())).map_err(|e| {
        error(
            codes::INVALID_PARAMS,
            &format!("the params are not valid: {e}"),
        )
    })
}

impl Counted {
    /// Counts a connection of `role`, or `None` for a role rlxd does not count.
    fn new(daemon: &Arc<Daemon>, role: Role) -> Option<Self> {
        *count_of(&mut daemon.clients(), role)? += 1;
        Some(Self {
            daemon: Arc::clone(daemon),
            role,
        })
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        if let Some(count) = count_of(&mut self.daemon.clients(), self.role) {
            *count = count.saturating_sub(1);
        }
    }
}

/// `role` as `hello` writes it: `app`, `tui`, `remote` or `cli`.
fn role_name(role: Role) -> String {
    match serde_json::to_value(role) {
        Ok(Value::String(name)) => name,
        _ => format!("{role:?}"),
    }
}

fn count_of(clients: &mut Clients, role: Role) -> Option<&mut u32> {
    match role {
        // App connections are the gate's holders, which `Gate` counts.
        Role::App => None,
        Role::Tui => Some(&mut clients.tui),
        Role::Remote => Some(&mut clients.remote),
        Role::Cli => Some(&mut clients.cli),
        _ => None,
    }
}

/// Serves one connection, whose client is `peer`, until the client closes it, it is
/// refused, it is dropped from the gate, or rlxd stops.
pub async fn serve_connection<S>(daemon: Arc<Daemon>, stream: S, peer: Peer, hangup: Hangup)
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    // Settings calls run through `block_in_place` (`off_worker`), which only this flavour
    // of runtime allows.
    assert_eq!(
        tokio::runtime::Handle::current().runtime_flavor(),
        tokio::runtime::RuntimeFlavor::MultiThread,
        "rlxd serves connections on a multi-thread runtime"
    );
    let (reader, mut writer) = tokio::io::split(stream);
    let mut frames = Frames {
        reader: BufReader::new(reader),
        line: Vec::new(),
    };
    let first = tokio::select! {
        first = tokio::time::timeout(HELLO_TIMEOUT, frames.next()) => first,
        () = daemon.stopped() => return,
    };
    let frame = match first {
        Ok(Ok(Some(Frame::Line(frame)))) => frame,
        Ok(Ok(None) | Err(_)) => return,
        Ok(Ok(Some(Frame::Blank | Frame::NotText | Frame::TooLong))) => {
            let message = "the first frame must be a hello request, as JSON".to_owned();
            let reply = reply_err(None, codes::UNAUTHORIZED, message);
            return refuse(&mut frames, &mut writer, &reply).await;
        }
        Err(_) => {
            let message = format!("no hello came within {} s", HELLO_TIMEOUT.as_secs());
            let reply = reply_err(None, codes::UNAUTHORIZED, message);
            return refuse(&mut frames, &mut writer, &reply).await;
        }
    };
    let session = match daemon.hello(&frame, Arc::new(peer)).await {
        Ok((session, reply)) => {
            if send(&mut writer, &reply).await.is_err() {
                return;
            }
            session
        }
        Err(reply) => return refuse(&mut frames, &mut writer, &reply).await,
    };
    let mut watch = Watch {
        hangup,
        queued: VecDeque::new(),
        read_ended: false,
    };
    loop {
        let frame = tokio::select! {
            frame = watch.next_line(&mut frames) => frame,
            () = daemon.stopped() => return linger(&mut frames).await,
            () = kicked(&session) => return dropped(&session, &mut frames, &mut writer).await,
        };
        let frame = match frame {
            Ok(Some(Frame::Line(frame))) => frame,
            Ok(Some(Frame::Blank)) => continue,
            Ok(Some(Frame::NotText)) => {
                let message = "the frame is not UTF-8 text".to_owned();
                if send(
                    &mut writer,
                    &reply_err(None, codes::INVALID_REQUEST, message),
                )
                .await
                .is_err()
                {
                    return;
                }
                continue;
            }
            Ok(Some(Frame::TooLong)) => {
                let message = format!("the frame is longer than {MAX_FRAME} bytes");
                let reply = reply_err(None, codes::INVALID_REQUEST, message);
                return refuse(&mut frames, &mut writer, &reply).await;
            }
            Ok(None) | Err(_) => return,
        };
        let answer = match watch
            .answer(&daemon, &session, &mut frames, &frame, false)
            .await
        {
            Outcome::Answered(answer) => *answer,
            Outcome::Kicked => return dropped(&session, &mut frames, &mut writer).await,
            Outcome::Left => return,
        };
        match answer {
            Answer::Reply(reply) => {
                if send(&mut writer, &reply).await.is_err() {
                    return;
                }
            }
            Answer::Stop(reply) => {
                if send(&mut writer, &reply).await.is_err() {
                    return;
                }
                daemon.stop(STOP_REQUESTED);
            }
            Answer::Paired(reply, minted) => {
                if send_paired(&daemon.gate, &mut writer, &reply, &minted)
                    .await
                    .is_err()
                {
                    return;
                }
                if minted.close {
                    return dropped(&session, &mut frames, &mut writer).await;
                }
            }
            Answer::Stream(subscription) => {
                let flow = send_stream(
                    &daemon,
                    &session,
                    &mut frames,
                    &mut writer,
                    &mut watch,
                    subscription,
                )
                .await;
                match flow {
                    Flow::GoOn => {}
                    Flow::Linger => return linger(&mut frames).await,
                    Flow::Dropped => return dropped(&session, &mut frames, &mut writer).await,
                    Flow::Close => return,
                }
            }
        }
    }
}

/// The most frames queued while a request is answered.
const QUEUED_MAX: usize = 16;

/// What a connection watches while one of its requests is answered.
struct Watch {
    /// Whether the client is gone both ways, for when frames are not read.
    hangup: Hangup,
    /// Frames the client sent while a request was still being answered, in order.
    queued: VecDeque<std::io::Result<Option<Frame>>>,
    /// Whether the client's sending half has ended (a half-close, or all of it).
    read_ended: bool,
}

/// How an in-flight request ended.
enum Outcome {
    /// It was answered.
    Answered(Box<Answer>),
    /// The connection was dropped from the gate meanwhile.
    Kicked,
    /// The client closed the connection, both ways, meanwhile.
    Left,
}

impl Watch {
    /// The next line: one queued while a request was answered, else the next read.
    /// Cancel safe, as [`Frames::next_line`] is.
    async fn next_line<R: AsyncRead + Unpin>(
        &mut self,
        frames: &mut Frames<R>,
    ) -> std::io::Result<Option<Frame>> {
        match self.queued.pop_front() {
            Some(frame) => frame,
            None => frames.next_line().await,
        }
    }

    /// Answers `frame`, still watching the connection while it is answered, which for a
    /// new device's gate.pair can take two minutes:
    /// - A kick aborts the request. Anything that acts as the gate (the gate task's
    ///   run.decide) must still check its hold when it acts.
    /// - A client that is gone, both ways, aborts it too, so a waiting pairing request
    ///   is withdrawn and never pairs a device nobody is there to receive.
    /// - A client that only half-closed, or sent more frames meanwhile, still gets every
    ///   reply. Frames are queued, up to [`QUEUED_MAX`]; past that, and after a
    ///   half-close, the hangup probe watches instead of a read.
    async fn answer<R: AsyncRead + Unpin>(
        &mut self,
        daemon: &Daemon,
        session: &Session,
        frames: &mut Frames<R>,
        frame: &str,
        streaming: bool,
    ) -> Outcome {
        if session.app.as_ref().is_some_and(Hold::is_kicked) {
            return Outcome::Kicked;
        }
        let answering = daemon.answer(session, frame, streaming);
        tokio::pin!(answering);
        let outcome = loop {
            let reading = !self.read_ended && self.queued.len() < QUEUED_MAX;
            tokio::select! {
                answer = &mut answering => break Outcome::Answered(Box::new(answer)),
                () = kicked(session) => break Outcome::Kicked,
                next = frames.next_line(), if reading => match next {
                    Ok(Some(Frame::Blank)) => {}
                    Ok(None) | Err(_) if self.hangup.gone() => break Outcome::Left,
                    Ok(None) | Err(_) => {
                        self.read_ended = true;
                        self.queued.push_back(Ok(None));
                    }
                    next => self.queued.push_back(next),
                },
                () = self.hangup.closed(), if !reading => break Outcome::Left,
            }
        };
        if matches!(outcome, Outcome::Left) {
            info!("a client left before its request was answered; the request is dropped");
        }
        outcome
    }
}

/// Sends a reply that carries a minted gate credential. Unless it is sent whole, the
/// pairing is undone ([`UndoUnlessSent`]), and an error says the connection is done.
async fn send_paired<W: AsyncWrite + Unpin>(
    gate: &Gate,
    writer: &mut W,
    reply: &Envelope,
    minted: &Minted,
) -> std::io::Result<()> {
    let mut undo = UndoUnlessSent {
        gate,
        minted,
        sent: false,
    };
    let sent = send(writer, reply).await;
    undo.sent = sent.is_ok();
    sent
}

/// Undoes a pairing when dropped, unless its reply was sent whole: also when the send
/// never ends, and the connection's task is aborted mid-send, as rlxd's stop does to a
/// connection still open after its wait.
struct UndoUnlessSent<'a> {
    gate: &'a Gate,
    minted: &'a Minted,
    sent: bool,
}

impl Drop for UndoUnlessSent<'_> {
    fn drop(&mut self) {
        if !self.sent {
            let minted = self.minted;
            self.gate
                .undo_undelivered(&minted.device, &minted.sha256, minted.replaced.as_ref());
        }
    }
}

/// Sends `subscription`'s stream: the events after its place, a batch at a time, then
/// live ones while it follows, then its reply. Answers the client's requests meanwhile:
/// frames are read as they come, so a partial one holds up no event. An app connection
/// dropped from the gate ends it with the `not_gate` envelope instead of the reply.
async fn send_stream<R, W>(
    daemon: &Arc<Daemon>,
    session: &Session,
    frames: &mut Frames<R>,
    writer: &mut W,
    watch: &mut Watch,
    mut subscription: Subscription,
) -> Flow
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut head = daemon.bus.watch();
    let mut stopping = false;
    loop {
        loop {
            let lines = daemon
                .bus
                .read(&mut subscription.cursor, bus::BATCH, bus::BATCH_BYTES);
            if lines.is_empty() {
                break;
            }
            if send_lines(writer, &lines).await.is_err() {
                return Flow::Close;
            }
        }
        if stopping || subscription.cursor.is_done() {
            let data = object(&json!({"seq": subscription.cursor.after()}));
            let reply = Envelope::ok(data).with_id(subscription.id);
            if send(writer, &reply).await.is_err() {
                return Flow::Close;
            }
            return if stopping { Flow::Linger } else { Flow::GoOn };
        }
        let after = subscription.cursor.after();
        tokio::select! {
            moved = async { head.wait_for(|&seq| seq > after).await.map(|_| ()) } => {
                stopping = moved.is_err();
            }
            () = daemon.stopped() => stopping = true,
            () = kicked(session) => return Flow::Dropped,
            frame = watch.next_line(frames) => {
                if let Some(flow) =
                    answer_in_stream(daemon, session, frames, writer, watch, frame).await
                {
                    return flow;
                }
            }
        }
    }
}

/// Answers `frame`, read while the connection carries a stream. `None` when the stream
/// goes on.
async fn answer_in_stream<R, W>(
    daemon: &Arc<Daemon>,
    session: &Session,
    frames: &mut Frames<R>,
    writer: &mut W,
    watch: &mut Watch,
    frame: std::io::Result<Option<Frame>>,
) -> Option<Flow>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let frame = match frame {
        Ok(Some(Frame::Line(frame))) => frame,
        Ok(Some(Frame::Blank)) => return None,
        Ok(Some(Frame::NotText)) => {
            let message = "the frame is not UTF-8 text".to_owned();
            let reply = reply_err(None, codes::INVALID_REQUEST, message);
            return send(writer, &reply).await.is_err().then_some(Flow::Close);
        }
        Ok(Some(Frame::TooLong)) => {
            let message = format!("the frame is longer than {MAX_FRAME} bytes");
            let reply = reply_err(None, codes::INVALID_REQUEST, message);
            refuse(frames, writer, &reply).await;
            return Some(Flow::Close);
        }
        Ok(None) | Err(_) => return Some(Flow::Close),
    };
    let answer = match watch.answer(daemon, session, frames, &frame, true).await {
        Outcome::Answered(answer) => *answer,
        Outcome::Kicked => return Some(Flow::Dropped),
        Outcome::Left => return Some(Flow::Close),
    };
    match answer {
        Answer::Reply(reply) => send(writer, &reply).await.is_err().then_some(Flow::Close),
        Answer::Stop(reply) => {
            if send(writer, &reply).await.is_err() {
                return Some(Flow::Close);
            }
            daemon.stop(STOP_REQUESTED);
            None
        }
        Answer::Paired(reply, minted) => {
            if send_paired(&daemon.gate, writer, &reply, &minted)
                .await
                .is_err()
            {
                return Some(Flow::Close);
            }
            minted.close.then_some(Flow::Dropped)
        }
        Answer::Stream(_) => unreachable!("a streaming connection takes no second stream"),
    }
}

/// Sends `lines`, each one event and its newline, as they are: vectored writes of the
/// buffer's own lines, so a batch is never copied.
async fn send_lines<W: AsyncWrite + Unpin>(writer: &mut W, lines: &[Line]) -> std::io::Result<()> {
    let mut slices: Vec<std::io::IoSlice<'_>> = lines
        .iter()
        .map(|line| std::io::IoSlice::new(line))
        .collect();
    let mut rest = &mut slices[..];
    while !rest.is_empty() {
        let written = writer.write_vectored(rest).await?;
        if written == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        std::io::IoSlice::advance_slices(&mut rest, written);
    }
    writer.flush().await
}

fn unknown_topic(topic: &str) -> ErrorBody {
    error(
        codes::UNKNOWN_TOPIC,
        &format!("rlxd carries no topic {topic:?}"),
    )
    .with_hint(format!("Its topics: {}.", topics::ALL.join(", ")))
}

/// Resolves once an app connection is dropped from the gate; never for another.
async fn kicked(session: &Session) {
    match &session.app {
        Some(hold) => hold.kicked().await,
        None => std::future::pending().await,
    }
}

/// Tells an app connection dropped from the gate why, then closes it.
async fn dropped<R, W>(session: &Session, frames: &mut Frames<R>, writer: &mut W)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let device = session.app.as_ref().map_or("", Hold::device);
    info!(
        "gate: dropping an app connection of device {device:?}: its credential was replaced or unpaired"
    );
    let message = format!(
        "this connection no longer holds the gate: device {device:?}'s gate credential was \
         replaced, or the device was unpaired"
    );
    let error = error(codes::NOT_GATE, &message).with_hint(NOT_GATE_HINT);
    refuse(frames, writer, &Envelope::err(error)).await;
}

/// Sends `reply`, then waits a moment for the client to read it and close.
async fn refuse<R, W>(frames: &mut Frames<R>, writer: &mut W, reply: &Envelope)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    if send(writer, reply).await.is_ok() {
        linger(frames).await;
    }
}

/// Reads and drops what the client still sends until it closes, for up to [`LINGER`].
/// A named pipe has no half-close, so this is how its last reply is read before the
/// pipe goes.
async fn linger<R: AsyncRead + Unpin>(frames: &mut Frames<R>) {
    let drain = async { while let Ok(Some(_)) = frames.next().await {} };
    let _ = tokio::time::timeout(LINGER, drain).await;
}

async fn send<W: AsyncWrite + Unpin>(writer: &mut W, reply: &Envelope) -> std::io::Result<()> {
    let mut line = match serde_json::to_vec(reply) {
        Ok(line) => line,
        Err(e) => {
            warning!("a reply did not print: {e}");
            let message = format!("the reply was not valid: {e}");
            serde_json::to_vec(&reply_err(reply.id, codes::INTERNAL, message))
                .map_err(std::io::Error::other)?
        }
    };
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await
}

struct Frames<R> {
    reader: BufReader<R>,
    /// The frame read so far, until its newline comes.
    line: Vec<u8>,
}

impl<R: AsyncRead + Unpin> Frames<R> {
    /// The next frame, skipping blank lines, or `None` once the client has closed.
    async fn next(&mut self) -> std::io::Result<Option<Frame>> {
        loop {
            match self.next_line().await? {
                Some(Frame::Blank) => {}
                frame => return Ok(frame),
            }
        }
    }

    /// The next line, blank or not, or `None` once the client has closed.
    ///
    /// Cancel safe: what it has read of a frame is kept until the frame's newline comes,
    /// so a `select!` can race it, and a client that sends part of a frame holds up
    /// nothing else on its connection.
    async fn next_line(&mut self) -> std::io::Result<Option<Frame>> {
        loop {
            let chunk = self.reader.fill_buf().await?;
            if chunk.is_empty() {
                let line = std::mem::take(&mut self.line);
                return Ok((!is_blank(&line)).then(|| text(line)));
            }
            let (part, used, ended) = match chunk.iter().position(|&b| b == b'\n') {
                Some(at) => (&chunk[..at], at + 1, true),
                None => (chunk, chunk.len(), false),
            };
            if self.line.len() + part.len() > MAX_FRAME {
                self.reader.consume(used);
                self.line.clear();
                return Ok(Some(Frame::TooLong));
            }
            self.line.extend_from_slice(part);
            self.reader.consume(used);
            if ended {
                let line = std::mem::take(&mut self.line);
                if is_blank(&line) {
                    return Ok(Some(Frame::Blank));
                }
                return Ok(Some(text(line)));
            }
        }
    }
}

fn is_blank(line: &[u8]) -> bool {
    line.iter().all(u8::is_ascii_whitespace)
}

fn text(mut line: Vec<u8>) -> Frame {
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    String::from_utf8(line).map_or(Frame::NotText, Frame::Line)
}

/// A request's `id`, when the frame has a valid one, so even a refusal can name it.
fn request_id(frame: &Value) -> Option<RequestId> {
    frame
        .get("id")
        .and_then(|id| RequestId::deserialize(id).ok())
}

fn capabilities() -> Vec<String> {
    CAPABILITIES.iter().map(|&c| c.to_owned()).collect()
}

/// Runs `work`, which may block on a lock or on the disk, as blocking work: the runtime
/// hands this worker's other tasks to another, so no other connection waits on it. It
/// needs the multi-thread runtime `serve` builds (`block_in_place` panics on any other),
/// which `serve_connection` asserts. A call waiting on the settings lock holds a thread
/// of tokio's blocking pool meanwhile; the lock's holder never needs the runtime, so
/// every waiter gets its turn.
fn off_worker<T>(work: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(work)
}

/// Who a connection is, to the settings: app-role only while it holds the gate.
fn writer<'a>(session: &'a Session, who: &'a str) -> settings::Writer<'a> {
    let hold = session.app.as_ref().filter(|hold| !hold.is_kicked());
    settings::Writer {
        app: hold.is_some(),
        device: hold.map(Hold::device),
        who,
    }
}

/// A method's params, read as its wire type: `invalid_params` naming the field when not.
fn params_of<T: serde::de::DeserializeOwned>(method: &str, params: Params) -> Result<T, ErrorBody> {
    serde_json::from_value(Value::Object(params)).map_err(|e| {
        error(
            codes::INVALID_PARAMS,
            &format!("{method}'s params are not valid: {e}"),
        )
    })
}

/// `value`, printed as a reply's `data`.
fn printed(value: &impl serde::Serialize) -> Result<Data, ErrorBody> {
    match serde_json::to_value(value) {
        Ok(Value::Object(data)) => Ok(data),
        Ok(_) => Err(error(
            codes::INTERNAL,
            "the reply did not print as an object",
        )),
        Err(e) => Err(error(
            codes::INTERNAL,
            &format!("the reply is not valid: {e}"),
        )),
    }
}

/// The error a refused setting gets.
fn refused(refused: settings::Refused) -> ErrorBody {
    match refused {
        settings::Refused::UnknownKey(key) => error(
            codes::UNKNOWN_SETTING,
            &format!("rlxd knows no setting {key:?}"),
        )
        .with_hint(format!(
            "Its settings: {}; any dotted key, <namespace>.<name>, is free to use.",
            setting_keys::KNOWN.join(", ")
        )),
        settings::Refused::NotGate(key) => error(
            codes::NOT_GATE,
            &format!(
                "only an app-role connection holding the gate, a paired device, may change \
                 {key}"
            ),
        )
        .with_hint("send_safety_mode and every key under policy. are protected."),
        settings::Refused::Conflict(version) => {
            let mut detail = Data::new();
            detail.insert("version".to_owned(), Value::from(version));
            error(
                codes::CONFLICT,
                &format!("the setting is at version {version} now, not the one expected"),
            )
            .with_hint("Read it again, and write from its version now.")
            .with_detail(detail)
            .expect("conflict's detail is an object")
        }
        settings::Refused::Invalid(why) => error(codes::INVALID_PARAMS, &why),
        settings::Refused::Unwritten(why) => error(codes::INTERNAL, &why),
    }
}

/// The methods this rlxd serves after `hello`.
fn served() -> Vec<&'static str> {
    let mut served = vec![
        methods::STATUS,
        methods::STOP,
        methods::EVENTS_SUBSCRIBE,
        methods::SETTINGS_GET,
        methods::SETTINGS_SET,
        methods::SETTINGS_LIST,
        methods::SETTINGS_DELETE,
        methods::GATE_PAIR,
        methods::GATE_PAIR_REQUESTS,
        methods::GATE_PAIR_ANSWER,
        methods::GATE_DEVICES,
        methods::GATE_UNPAIR,
        methods::RUN_START,
        methods::RUN_CANCEL,
        methods::SPACE_ENSURE,
    ];
    if cfg!(feature = "debug-methods") {
        served.push(methods::DEBUG_EMIT);
    }
    served
}

/// An error whose code and message are both non-empty, as every caller's are.
fn error(code: &str, message: &str) -> ErrorBody {
    ErrorBody::new(code, message).expect("a code and a message that are not empty")
}

fn reply_err(id: Option<RequestId>, code: &str, message: String) -> Envelope {
    Envelope::err(error(code, &message)).with_optional_id(id)
}

/// `value`'s fields, for an envelope's `data`. Every caller passes an object.
fn object(value: &impl serde::Serialize) -> Data {
    match serde_json::to_value(value) {
        Ok(Value::Object(data)) => data,
        _ => Data::new(),
    }
}

trait WithOptionalId {
    fn with_optional_id(self, id: Option<RequestId>) -> Self;
}

impl WithOptionalId for Envelope {
    fn with_optional_id(self, id: Option<RequestId>) -> Self {
        match id {
            Some(id) => self.with_id(id),
            None => self,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use super::*;

    /// A client that never takes what is written: every write waits.
    struct Stuck;

    impl AsyncWrite for Stuck {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Pending
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    /// A folder of the test's own, removed when dropped.
    struct Dir(PathBuf);

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn a_pairing_whose_reply_is_dropped_mid_send_is_undone() {
        // Review #2068, r3 minor 3: rlxd's stop aborts a connection still sending a
        // re-pairing's reply, and the old credential must be the one left paired.
        let dir =
            Dir(std::env::temp_dir().join(format!("rlxd-send-paired-{}", std::process::id())));
        std::fs::create_dir_all(&dir.0).unwrap();
        let path = dir.0.join("gate.json");
        let gate = Gate::load(&path, false).unwrap();
        let first = gate.pair(None, None, None).ok().unwrap();
        let again = gate.pair(None, Some(&first.credential), None).ok().unwrap();
        let minted = Minted {
            device: again.device,
            sha256: again.sha256,
            replaced: again.replaced,
            close: false,
        };
        let reply = Envelope::ok(Data::new());
        let mut stuck = Stuck;
        let sending = send_paired(&gate, &mut stuck, &reply, &minted);
        // Dropped while it waits to write, as an aborted task's future is.
        let waited = tokio::time::timeout(Duration::from_millis(50), sending).await;
        assert!(waited.is_err(), "a stuck client takes nothing");
        let reloaded = Gate::load(&path, false).unwrap();
        assert!(
            reloaded.device_of(&first.credential).is_some(),
            "the credential the client holds is paired again"
        );
        assert!(reloaded.device_of(&again.credential).is_none());
    }
}
