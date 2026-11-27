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
    Claude, Clients, Data, EnvRefusal, EnvValues, Envelope, ErrorBody, Event, HelloData,
    HelloParams, JobListParams, JobPutParams, JobRef, PROTOCOL_VERSION, PROTOCOL_VERSIONS, Params,
    Paths, PtyKillParams, PtyResizeParams, PtyScreenParams, PtySpawnParams, PtySubscribeParams,
    PtyWriteParams, Request, RequestId, Role, RunDecideParams, RunEventsParams, RunListParams,
    RunRef, RunResumeParams, RunStartParams, SettingsDeleteParams, SettingsGetParams,
    SettingsListParams, SettingsSetParams, SpaceEnsureData, SpaceParams, StatusData,
    SubscribeParams, VERSION, codes, event_types, methods, setting_keys, topics,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::watch;

use crate::brain::Brain;
use crate::bus::{self, Bus, Cursor, Line, Topics};
use crate::env::Env;
use crate::gate::{self, AnswerError, Gate, Hold, Refused, Route, UnpairError};
use crate::listener::Hangup;
use crate::log::{info, warning};
use crate::peer::Peer;
use crate::pty::{self, Host};
use crate::runner::{Replay, Runner};
use crate::scheduler::Scheduler;
use crate::settings::{self, Settings};
use crate::{claude, spaces, token};

/// `daemon.stopping`'s `reason` when a client asked rlxd to stop.
pub const STOP_REQUESTED: &str = "stop";

/// The longest frame rlxd reads: 1 MiB. After `hello`, a longer one gets
/// `frame_too_large`, and rlxd skips the rest of it and reads on; as the first frame, it
/// is no `hello`, and the connection closes, as for any other.
pub const MAX_FRAME: usize = 1 << 20;

// The longest `pty.write` fits a frame however it is escaped: six bytes of JSON for each
// of its bytes at most, and room for the envelope.
const _: () = assert!(6 * rlxd_proto::MAX_PTY_WRITE + 64 * 1024 <= MAX_FRAME);

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
    /// The scheduler, or why it couldn't start (its database couldn't be made).
    scheduler: Result<Arc<Scheduler>, String>,
    settings: Settings,
    /// The app's environment for Claude Code (4.3), and the `claude` it resolves to.
    env: Arc<Env>,
    ptys: Host,
    stopping: AtomicBool,
    stop: watch::Sender<bool>,
    gate: Arc<Gate>,
    brain: Arc<Brain>,
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
    /// The PTY this connection's stream watches, and whether read-only, while it does.
    watching: Mutex<Option<(String, bool)>>,
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
    /// A run's events from its log (`run.events`), whose last frame is the reply.
    RunStream(RequestId, Box<Replay>),
    /// No reply: the connection closes (a hook request the gate dropped, 5.1). Only test
    /// builds make hook requests until rlxd-hook's pre-tool-use is wired (#617).
    #[cfg_attr(not(feature = "debug-methods"), allow(dead_code))]
    Hangup,
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
    /// For `pty.subscribe`: the PTY, and the snapshot that starts the stream.
    pty: Option<PtyWatch>,
}

/// A `pty.subscribe` stream's PTY.
struct PtyWatch {
    id: String,
    snapshot: Line,
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
    /// An rlxd serving `home` with `token`, whose gate is `gate`, publishing on `bus`,
    /// serving `settings`, and asking `brain`.
    /// `pipe_acl` is what `status` says of the pipe's DACL, on Windows.
    pub fn new(
        home: Home,
        token: String,
        pipe_acl: Option<&'static str>,
        gate: Gate,
        bus: Bus,
        settings: Settings,
        brain: Arc<Brain>,
    ) -> Arc<Self> {
        let env = Arc::new(Env::open(&home));
        let bus = Arc::new(bus);
        let runner = Runner::new(home.clone(), Arc::clone(&bus), Arc::clone(&env));
        let gate = Arc::new(gate);
        runner.follow_apps(gate.apps());
        let scheduler = Scheduler::new(&home, Arc::clone(&bus), Arc::clone(&runner), gate.apps());
        match &scheduler {
            Ok(scheduler) => {
                tokio::spawn(Arc::clone(scheduler).run(runner.notes()));
            }
            Err(why) => crate::log::error!("scheduler: not started: {why}"),
        }
        Arc::new(Self {
            home,
            token,
            pid: std::process::id(),
            pipe_acl,
            clients: Mutex::new(Clients::default()),
            claude: tokio::sync::Mutex::new(None),
            ptys: Host::new(Arc::clone(&bus)),
            bus,
            runner,
            scheduler,
            settings,
            env,
            stopping: AtomicBool::new(false),
            stop: watch::Sender::new(false),
            gate,
            brain,
        })
    }

    /// The resident Python brain.
    pub fn brain(&self) -> &Brain {
        &self.brain
    }

    /// The home's event bus.
    pub fn bus(&self) -> &Bus {
        &self.bus
    }

    /// Ends every PTY's process, as rlxd stops: none outlives it.
    pub fn end_ptys(&self) {
        self.ptys.kill_all();
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
        if let Ok(scheduler) = &self.scheduler {
            scheduler.shutdown();
        }
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
        // env (4.3) is the app's alone: any other hello carrying it is refused, and one
        // the app sends is checked whole before the gate admits it.
        let env_change = match &hello.env {
            None => None,
            Some(_) if hello.role != Role::App => {
                info!(
                    "hello refused: env from {} as {} (not_gate)",
                    hello.client,
                    role_name(hello.role)
                );
                return Err(Envelope::err(env_not_gate("hello's env")).with_optional_id(id));
            }
            Some(env) => match EnvValues::from_map(env) {
                Ok(change) => Some(change),
                Err(refusal) => {
                    info!("hello refused: {refusal}");
                    return Err(Envelope::err(env_refused(refusal)).with_optional_id(id));
                }
            },
        };
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
        if let Some(change) = env_change.filter(|change| !change.is_empty()) {
            let who = format!("app {}", hello.client);
            if let Err(e) = off_worker(|| self.env.set(&change, &who)) {
                return Err(reply_err(id, codes::INTERNAL, e.to_string()));
            }
            *self.claude.lock().await = None;
        }
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
            watching: Mutex::new(None),
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
            methods::RUN_EVENTS | methods::EVENTS_SUBSCRIBE if streaming => Err(error(
                codes::INVALID_REQUEST,
                "this connection carries an event stream already: open another for a second",
            )),
            methods::RUN_EVENTS => {
                match params_of::<RunEventsParams>(methods::RUN_EVENTS, request.params)
                    .and_then(|params| self.runner.replay(&params))
                {
                    Ok(replay) => return Answer::RunStream(id, Box::new(replay)),
                    Err(error) => Err(error),
                }
            }
            methods::RUN_START
            | methods::RUN_CANCEL
            | methods::RUN_RESUME
            | methods::RUN_GET
            | methods::RUN_LIST
            | methods::SPACE_ENSURE
            | methods::SPACE_ERASE => {
                self.runs_and_spaces(session, &request.method, request.params)
                    .await
            }
            #[cfg(feature = "debug-methods")]
            methods::DEBUG_RUN_START => self.debug_run_start(session, request.params),
            methods::RUN_DECIDE => self.run_decide(session, request.params).await,
            #[cfg(feature = "debug-methods")]
            methods::DEBUG_PRE_TOOL_USE => {
                match self.debug_pre_tool_use(session, request.params).await {
                    Ok(Some(data)) => Ok(data),
                    Ok(None) => return Answer::Hangup,
                    Err(error) => Err(error),
                }
            }
            #[cfg(feature = "debug-methods")]
            methods::DEBUG_DROP_HOOK_CONNECTIONS => {
                Ok(object(&json!({"dropped": self.runner.drop_hooks()})))
            }
            methods::JOB_PUT
            | methods::JOB_LIST
            | methods::JOB_DELETE
            | methods::JOB_PAUSE
            | methods::JOB_RESUME
            | methods::JOB_RUN_NOW => self.jobs(&request.method, request.params),
            #[cfg(feature = "debug-methods")]
            methods::DEBUG_CLOCK_ADVANCE | methods::DEBUG_CLOCK_SET => {
                self.debug_clock(&request.method, request.params)
            }
            methods::STOP => {
                let data = object(&json!({"pid": self.pid}));
                return Answer::Stop(Envelope::ok(data).with_id(id));
            }
            methods::HOOK_PING => match cli_only(session, methods::HOOK_PING) {
                Ok(()) => crate::brain::ping(&self.brain).await,
                Err(refused) => Err(refused),
            },
            methods::EVENTS_SUBSCRIBE => match self.subscribe(id, request.params) {
                Ok(subscription) => return Answer::Stream(subscription),
                Err(error) => Err(error),
            },
            #[cfg(feature = "debug-methods")]
            methods::DEBUG_EMIT => self.debug_emit(&request.params).await,
            #[cfg(feature = "debug-methods")]
            methods::DEBUG_PTY_PANIC => self.debug_pty_panic(&request.params),
            methods::SETTINGS_GET => self.settings_get(request.params),
            methods::SETTINGS_SET => self.settings_set(session, request.params),
            methods::SETTINGS_LIST => self.settings_list(request.params),
            methods::SETTINGS_DELETE => self.settings_delete(session, request.params),
            methods::ENV_GET => printed(&self.env.values()),
            methods::ENV_SET => self.env_set(session, request.params).await,
            methods::PTY_SUBSCRIBE if streaming => Err(error(
                codes::INVALID_REQUEST,
                "this connection carries a stream already: open another for a second",
            )),
            methods::PTY_SUBSCRIBE => match self.pty_subscribe(session, id, request.params) {
                Ok(subscription) => return Answer::Stream(subscription),
                Err(error) => Err(error),
            },
            methods::PTY_SPAWN
            | methods::PTY_WRITE
            | methods::PTY_RESIZE
            | methods::PTY_SCREEN
            | methods::PTY_KILL
            | methods::PTY_LIST => self.pty_call(session, &request.method, request.params),
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
        Ok(Subscription {
            id,
            cursor,
            pty: None,
        })
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

    /// `env.set {…}` (4.3): only an app-role connection that holds the gate, a paired
    /// device, sets the app's environment; any other gets `not_gate`. A key that is not
    /// one of the three is `bad_env_key`. Either way, nothing changes.
    async fn env_set(&self, session: &Session, params: Params) -> Result<Data, ErrorBody> {
        if !session.app.as_ref().is_some_and(|hold| !hold.is_kicked()) {
            return Err(env_not_gate("env.set"));
        }
        let change = EnvValues::from_object(&params).map_err(env_refused)?;
        let who = format!("{} {}", role_name(session.role), session.client);
        let values = off_worker(|| self.env.set(&change, &who))
            .map_err(|e| error(codes::INTERNAL, &e.to_string()))?;
        *self.claude.lock().await = None;
        printed(&values)
    }

    /// `settings.list {prefix?}`.
    fn settings_list(&self, params: Params) -> Result<Data, ErrorBody> {
        let params = params_of::<SettingsListParams>(methods::SETTINGS_LIST, params)?;
        let list = off_worker(|| self.settings.list(params.prefix.as_deref())).map_err(refused)?;
        printed(&list)
    }

    /// A `pty.*` call, if the connection's role may make it (`pty_allowed`). Each runs
    /// as blocking work: a spawn forks, a kill waits for the process group to go.
    fn pty_call(&self, session: &Session, method: &str, params: Params) -> Result<Data, ErrorBody> {
        if !pty_allowed(session.role, method) {
            return Err(role_not_allowed(session.role, method));
        }
        let who = format!("{} {}", role_name(session.role), session.client);
        match method {
            methods::PTY_SPAWN => {
                let params = params_of::<PtySpawnParams>(method, params)?;
                let spawned = off_worker(|| self.ptys.spawn(&params, &who));
                printed(&spawned.map_err(pty_refused)?)
            }
            methods::PTY_WRITE => {
                write_fits(&params)?;
                let params = params_of::<PtyWriteParams>(method, params)?;
                if watching_read_only(session, &params.pty_id) {
                    return Err(error(
                        codes::READ_ONLY,
                        &format!("this connection watches pty {} read-only", params.pty_id),
                    )
                    .with_hint("Write from a connection that doesn't watch it read-only."));
                }
                printed(&off_worker(|| self.ptys.write(&params)).map_err(pty_refused)?)
            }
            methods::PTY_RESIZE => {
                let params = params_of::<PtyResizeParams>(method, params)?;
                printed(&off_worker(|| self.ptys.resize(&params)).map_err(pty_refused)?)
            }
            methods::PTY_SCREEN => {
                let params = params_of::<PtyScreenParams>(method, params)?;
                let data =
                    printed(&off_worker(|| self.ptys.screen(&params)).map_err(pty_refused)?)?;
                let long = serde_json::to_vec(&data).map_or(usize::MAX, |data| data.len());
                // The reply's envelope around it, an id among it, takes far less than this.
                if long + 4096 > bus::MAX_EVENT {
                    return Err(screen_too_long(long));
                }
                Ok(data)
            }
            methods::PTY_KILL => {
                let params = params_of::<PtyKillParams>(method, params)?;
                printed(&off_worker(|| self.ptys.kill(&params, &who)).map_err(pty_refused)?)
            }
            _ => printed(&off_worker(|| self.ptys.list()).map_err(pty_refused)?),
        }
    }

    /// `pty.subscribe {pty_id, read_only}`: the stream of one PTY, which starts with its
    /// snapshot. Only `cli` and `tui` may watch with `read_only: false`.
    fn pty_subscribe(
        &self,
        session: &Session,
        id: RequestId,
        params: Params,
    ) -> Result<Subscription, ErrorBody> {
        let method = methods::PTY_SUBSCRIBE;
        if !pty_allowed(session.role, method) {
            return Err(role_not_allowed(session.role, method));
        }
        let params = params_of::<PtySubscribeParams>(method, params)?;
        if !params.read_only && !matches!(session.role, Role::Cli | Role::Tui) {
            return Err(role_not_allowed(session.role, method).with_hint(
                "Only cli and tui may watch a pty with read_only false; subscribe with \
                 read_only true.",
            ));
        }
        let (snapshot, seq) = off_worker(|| self.pty_snapshot(&params.pty_id))?;
        let topics = Topics::of([topics::PTY]).expect("the bus carries pty events");
        let cursor = self
            .bus
            .cursor(topics, Some(seq), true)
            .map_err(|head| ErrorBody::since_ahead(seq, head))?;
        *session.watching.lock().unwrap_or_else(|p| p.into_inner()) =
            Some((params.pty_id.clone(), params.read_only));
        Ok(Subscription {
            id,
            cursor,
            pty: Some(PtyWatch {
                id: params.pty_id,
                snapshot,
            }),
        })
    }

    /// `pty_id`'s screen as a `pty.snapshot` event's line, and the seq it is pinned to.
    fn pty_snapshot(&self, pty_id: &str) -> Result<(Line, u64), ErrorBody> {
        let params = PtyScreenParams::new(pty_id)
            .map_err(|e| error(codes::INVALID_PARAMS, &e.to_string()))?;
        let screen = self.ptys.screen(&params).map_err(pty_refused)?;
        let data = screen
            .to_data()
            .map_err(|e| error(codes::INTERNAL, &e.to_string()))?;
        let event = Event::new(
            screen.seq.max(1),
            crate::clock::now(),
            topics::PTY,
            event_types::PTY_SNAPSHOT,
            data,
        )
        .map_err(|e| error(codes::INTERNAL, &e.to_string()))?;
        let mut line =
            serde_json::to_vec(&event).map_err(|e| error(codes::INTERNAL, &e.to_string()))?;
        if line.len() >= bus::MAX_EVENT {
            return Err(screen_too_long(line.len()));
        }
        line.push(b'\n');
        Ok((Line::from(line), screen.seq))
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

    /// `debug.pty_panic {pty_id, at}`: the PTY's screen model panics the next time rlxd
    /// calls it `at` (`output`, `screen` or `resize`), as a bug there would, to show the
    /// panic ends that PTY alone. Test builds only.
    #[cfg(feature = "debug-methods")]
    fn debug_pty_panic(&self, params: &Params) -> Result<Data, ErrorBody> {
        use crate::vt::TestPanic;
        let at = match params.get("at").and_then(Value::as_str) {
            Some("output") => Some(TestPanic::Output),
            Some("screen") => Some(TestPanic::Screen),
            Some("resize") => Some(TestPanic::Resize),
            _ => None,
        };
        let (Some(pty_id), Some(at)) = (params.get("pty_id").and_then(Value::as_str), at) else {
            return Err(error(
                codes::INVALID_PARAMS,
                "debug.pty_panic takes {\"pty_id\": ID, \"at\": \"output\"|\"screen\"|\"resize\"}",
            ));
        };
        printed(&off_worker(|| self.ptys.panic_next(pty_id, at)).map_err(pty_refused)?)
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
        status.brain = self.brain.status();
        (status.runs.live, status.runs.queued) = self.runner.counts();
        status.jobs = match &self.scheduler {
            Ok(scheduler) => scheduler.status(),
            Err(why) => {
                let mut jobs = rlxd_proto::Jobs::default();
                jobs.db = Some("unavailable".to_owned());
                jobs.reason = Some(why.clone());
                jobs
            }
        };
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
        let (resolved, source) = self.env.claude();
        let claude = claude::probe_at(&resolved, source).await;
        *cached = Some((Instant::now(), claude.clone()));
        claude
    }

    /// `debug.run_start`: `run.start`'s params plus a scheduler `job_id`, which only rlxd
    /// sets otherwise (7.6), so tests can check `run.list {job_id}`. Test builds only.
    #[cfg(feature = "debug-methods")]
    fn debug_run_start(&self, session: &Session, mut params: Params) -> Result<Data, ErrorBody> {
        let job_id = match params.remove("job_id") {
            Some(Value::String(job_id)) if !job_id.is_empty() => job_id,
            _ => {
                return Err(error(
                    codes::INVALID_PARAMS,
                    "debug.run_start takes run.start's params and a job_id, a string",
                ));
            }
        };
        let params = params_of::<RunStartParams>(methods::DEBUG_RUN_START, params)?;
        printed(&off_worker(|| {
            self.runner.start(
                params,
                &session.client,
                Some(crate::runner::JobTag {
                    job_id,
                    loud: false,
                    watch: false,
                }),
            )
        })?)
    }

    /// `run.decide`: the app's answer to a pending tool call (5.1), from an app-role
    /// connection that holds the gate only. Any other gets `not_gate`.
    async fn run_decide(&self, session: &Session, params: Params) -> Result<Data, ErrorBody> {
        if session.app.as_ref().is_none_or(Hold::is_kicked) {
            info!(
                "run.decide refused ({} {}, {}): not_gate",
                role_name(session.role),
                session.client,
                session.peer
            );
            return Err(error(
                codes::NOT_GATE,
                "only rlx's app, holding the gate, decides a run's tool calls",
            )
            .with_hint(NOT_GATE_HINT));
        }
        let params = params_of::<RunDecideParams>(methods::RUN_DECIDE, params)?;
        printed(&self.runner.decide(params).await?)
    }

    /// `debug.pre_tool_use {run_id, attempt, key, stdin}`: a hook request, as rlxd-hook's
    /// pre-tool-use will send it, from this connection's process. `None` when the gate
    /// dropped it. Test builds only.
    #[cfg(feature = "debug-methods")]
    async fn debug_pre_tool_use(
        &self,
        session: &Session,
        params: Params,
    ) -> Result<Option<Data>, ErrorBody> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct HookParams {
            run_id: String,
            attempt: u64,
            key: String,
            stdin: Value,
        }
        let params = params_of::<HookParams>(methods::DEBUG_PRE_TOOL_USE, params)?;
        let ask = crate::runner::Ask {
            run_id: params.run_id,
            attempt: params.attempt,
            key: params.key,
            peer: Arc::clone(&session.peer),
            stdin: params.stdin,
        };
        Ok(self
            .runner
            .pre_tool_use(ask)
            .await
            .map(|output| object(&json!({"output": output}))))
    }

    /// The scheduler's methods (handoff 0.10.3, 7.6): `job.put`, `job.list`, `job.delete`,
    /// `job.pause`, `job.resume` and `job.run_now`. They write its store: blocking work.
    fn jobs(&self, method: &str, params: Params) -> Result<Data, ErrorBody> {
        let scheduler = self.scheduler.as_ref().map_err(|why| {
            error(
                codes::INTERNAL,
                &format!("the scheduler is not running: {why}"),
            )
        })?;
        match method {
            methods::JOB_PUT => {
                let params = params_of::<JobPutParams>(method, params)?;
                printed(&off_worker(|| scheduler.put(params))?)
            }
            methods::JOB_LIST => {
                params_of::<JobListParams>(method, params)?;
                printed(&off_worker(|| scheduler.list()))
            }
            methods::JOB_DELETE => {
                let job = params_of::<JobRef>(method, params)?;
                printed(&off_worker(|| scheduler.delete(&job))?)
            }
            methods::JOB_PAUSE | methods::JOB_RESUME => {
                let job = params_of::<JobRef>(method, params)?;
                let paused = method == methods::JOB_PAUSE;
                printed(&off_worker(|| scheduler.pause(&job, paused))?)
            }
            methods::JOB_RUN_NOW => {
                let job = params_of::<JobRef>(method, params)?;
                printed(&off_worker(|| scheduler.run_now(&job))?)
            }
            _ => Err(error(codes::UNKNOWN_METHOD, method)),
        }
    }

    /// Test builds only: `debug.clock_advance {seconds}` and `debug.clock_set {at}` move
    /// the scheduler's test clock, and answer `{now}` once every decision due by then is
    /// made.
    #[cfg(feature = "debug-methods")]
    fn debug_clock(&self, method: &str, params: Params) -> Result<Data, ErrorBody> {
        let scheduler = self.scheduler.as_ref().map_err(|why| {
            error(
                codes::INTERNAL,
                &format!("the scheduler is not running: {why}"),
            )
        })?;
        let at = if method == methods::DEBUG_CLOCK_ADVANCE {
            let params = params_of::<rlxd_proto::ClockAdvanceParams>(method, params)?;
            let seconds = i64::try_from(params.seconds).unwrap_or(i64::MAX);
            scheduler
                .now()
                .checked_add(jiff::SignedDuration::from_secs(seconds))
                .map_err(|e| error(codes::INVALID_PARAMS, &e.to_string()))?
        } else {
            let params = params_of::<rlxd_proto::ClockSetParams>(method, params)?;
            params
                .at
                .parse::<jiff::Timestamp>()
                .map_err(|e| error(codes::INVALID_PARAMS, &format!("at: {e}")))?
        };
        let now = off_worker(|| scheduler.clock_to(at))?;
        Ok(object(&json!({"now": now.to_string()})))
    }

    /// The runner's methods (handoff 0.10.3, 5.4 and 7.5) and spaces' (2.5): `run.start`,
    /// `run.cancel`, `run.resume`, `run.get`, `run.list`, `space.ensure` and `space.erase`.
    /// A run's client, in `run.list`, is the `client` the connection's hello named.
    async fn runs_and_spaces(
        &self,
        session: &Session,
        method: &str,
        params: Params,
    ) -> Result<Data, ErrorBody> {
        match method {
            methods::RUN_START => {
                let params = params_of::<RunStartParams>(method, params)?;
                // Blocking work: it may wait on a run's lock, or write the run's record.
                printed(&off_worker(|| {
                    self.runner.start(params, &session.client, None)
                })?)
            }
            methods::RUN_CANCEL => {
                let params = params_of::<RunRef>(method, params)?;
                printed(&self.runner.cancel(&params.run_id).await?)
            }
            methods::RUN_RESUME => {
                let params = params_of::<RunResumeParams>(method, params)?;
                printed(&off_worker(|| self.runner.resume(params))?)
            }
            methods::RUN_GET => {
                let params = params_of::<RunRef>(method, params)?;
                printed(&off_worker(|| self.runner.get(&params.run_id))?)
            }
            methods::RUN_LIST => {
                let params = params_of::<RunListParams>(method, params)?;
                printed(&off_worker(|| self.runner.list(&params)))
            }
            methods::SPACE_ERASE => {
                let params = params_of::<SpaceParams>(method, params)?;
                printed(&off_worker(|| self.runner.erase(&params.name))?)
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

/// `role_not_allowed {role, method}` unless `session` is a `cli` client: the hook methods
/// are rlxd-hook's, a command-line client (1.5).
fn cli_only(session: &Session, method: &str) -> Result<(), ErrorBody> {
    if session.role == Role::Cli && session.app.is_none() {
        return Ok(());
    }
    let role = role_name(session.role);
    let refused = error(
        codes::ROLE_NOT_ALLOWED,
        &format!("{method} is for command-line clients, such as rlxd-hook, not role {role}"),
    );
    let detail = object(&json!({"role": role, "method": method}));
    Err(refused.clone().with_detail(detail).unwrap_or(refused))
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
        skipping: false,
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
                if send(&mut writer, &frame_too_large()).await.is_err() {
                    return;
                }
                continue;
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
            Answer::Hangup => return,
            Answer::RunStream(id, replay) => {
                let flow = send_run_stream(
                    &daemon,
                    &session,
                    &mut frames,
                    &mut writer,
                    &mut watch,
                    id,
                    *replay,
                )
                .await;
                match flow {
                    Flow::GoOn => {}
                    Flow::Linger => return linger(&mut frames).await,
                    Flow::Dropped => return dropped(&session, &mut frames, &mut writer).await,
                    Flow::Close => return,
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
    let mut gone = daemon.ptys.gone();
    let mut stopping = false;
    if let Some(pty) = &subscription.pty
        && send_lines(writer, std::slice::from_ref(&pty.snapshot))
            .await
            .is_err()
    {
        return end_watch(session, Flow::Close);
    }
    loop {
        loop {
            let lines = daemon
                .bus
                .read(&mut subscription.cursor, bus::BATCH, bus::BATCH_BYTES);
            if lines.is_empty() {
                break;
            }
            let lines = match &subscription.pty {
                None => lines,
                Some(pty) => match pty_lines(daemon, &mut subscription.cursor, pty, lines) {
                    Ok(lines) => lines,
                    Err(()) => break,
                },
            };
            if send_lines(writer, &lines).await.is_err() {
                return end_watch(session, Flow::Close);
            }
        }
        let pty_gone = subscription
            .pty
            .as_ref()
            .is_some_and(|pty| !daemon.ptys.exists(&pty.id));
        if stopping || subscription.cursor.is_done() || pty_gone {
            let data = object(&json!({"seq": subscription.cursor.after()}));
            let reply = Envelope::ok(data).with_id(subscription.id);
            if send(writer, &reply).await.is_err() {
                return Flow::Close;
            }
            return end_watch(session, if stopping { Flow::Linger } else { Flow::GoOn });
        }
        let after = subscription.cursor.after();
        tokio::select! {
            moved = async { head.wait_for(|&seq| seq > after).await.map(|_| ()) } => {
                stopping = moved.is_err();
            }
            _ = gone.changed(), if subscription.pty.is_some() => {}
            () = daemon.stopped() => stopping = true,
            () = kicked(session) => return end_watch(session, Flow::Dropped),
            frame = watch.next_line(frames) => {
                if let Some(flow) =
                    answer_in_stream(daemon, session, frames, writer, watch, frame).await
                {
                    return end_watch(session, flow);
                }
            }
        }
    }
}

/// Ends a stream's watch of a PTY, if it had one: the connection may write to it again.
fn end_watch(session: &Session, flow: Flow) -> Flow {
    session
        .watching
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take();
    flow
}

/// Whether this connection watches `pty_id` read-only.
fn watching_read_only(session: &Session, pty_id: &str) -> bool {
    session
        .watching
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .is_some_and(|(id, read_only)| *read_only && id == pty_id)
}

/// A `pty.subscribe` stream's lines out of a batch the bus gave: its PTY's events only.
/// A gap means output it had not read was dropped: it gets a fresh snapshot instead, and
/// its cursor goes on from that. `Err` when the snapshot can't be taken, as when the PTY
/// has just gone.
fn pty_lines(
    daemon: &Daemon,
    cursor: &mut Cursor,
    pty: &PtyWatch,
    lines: Vec<Line>,
) -> Result<Vec<Line>, ()> {
    let id = format!("\"pty_id\":\"{}\"", pty.id);
    let mut kept = Vec::with_capacity(lines.len());
    for line in lines {
        if contains(&line, b"\"type\":\"daemon.gap\"") {
            let (snapshot, seq) = off_worker(|| daemon.pty_snapshot(&pty.id)).map_err(|_| ())?;
            let topics = Topics::of([topics::PTY]).expect("the bus carries pty events");
            *cursor = daemon.bus.cursor(topics, Some(seq), true).map_err(|_| ())?;
            // The snapshot stands for the output before it, not for the PTY's end: a
            // pty.exited of this PTY's already read goes before it (#2641 minor 5).
            kept.retain(|kept: &Line| contains(kept, b"\"type\":\"pty.exited\""));
            kept.push(snapshot);
            return Ok(kept);
        }
        if contains(&line, id.as_bytes()) {
            kept.push(line);
        }
    }
    Ok(kept)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Sends a `run.events` stream: the run's events from its log on disk, read as they are
/// logged, then its reply `{run_id, last_run_seq}` once the run has no live process and
/// every event it logged is sent. Answers the client's requests meanwhile, as
/// [`send_stream`] does. When rlxd stops, the reply comes at once.
async fn send_run_stream<R, W>(
    daemon: &Arc<Daemon>,
    session: &Session,
    frames: &mut Frames<R>,
    writer: &mut W,
    watch: &mut Watch,
    id: RequestId,
    mut replay: Replay,
) -> Flow
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut stopping = false;
    loop {
        loop {
            let lines = off_worker(|| replay.read());
            if lines.is_empty() {
                break;
            }
            if send_lines(writer, &lines).await.is_err() {
                return Flow::Close;
            }
        }
        if stopping || off_worker(|| replay.done()) {
            let reply = match printed(&replay.reply()) {
                Ok(data) => Envelope::ok(data).with_id(id),
                Err(error) => Envelope::err(error).with_id(id),
            };
            if send(writer, &reply).await.is_err() {
                return Flow::Close;
            }
            return if stopping { Flow::Linger } else { Flow::GoOn };
        }
        tokio::select! {
            () = replay.changed() => {}
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
            return send(writer, &frame_too_large())
                .await
                .is_err()
                .then_some(Flow::Close);
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
        Answer::Hangup => Some(Flow::Close),
        Answer::Stream(_) | Answer::RunStream(..) => {
            unreachable!("a streaming connection takes no second stream")
        }
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
    /// The rest of a frame too long to read is being skipped, to its newline.
    skipping: bool,
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
            if self.skipping {
                // The rest of a frame too long to read: dropped as it comes.
                self.skipping = !ended;
                self.reader.consume(used);
                continue;
            }
            if self.line.len() + part.len() > MAX_FRAME {
                self.reader.consume(used);
                self.line.clear();
                self.skipping = !ended;
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

/// Whether a connection of `role` may call the PTY `method` (1.4): `cli` and `tui`, the
/// fleet and the user, every one; `remote` the reads and `pty.write`, as the phone answers
/// prompts; `app` the reads only, its panes being read-only.
fn pty_allowed(role: Role, method: &str) -> bool {
    let read = matches!(
        method,
        methods::PTY_LIST | methods::PTY_SCREEN | methods::PTY_SUBSCRIBE
    );
    match role {
        Role::Cli | Role::Tui => true,
        Role::Remote => read || method == methods::PTY_WRITE,
        Role::App => read,
        _ => false,
    }
}

fn role_not_allowed(role: Role, method: &str) -> ErrorBody {
    let name = role_name(role);
    let mut detail = Data::new();
    detail.insert("role".to_owned(), Value::from(name.clone()));
    detail.insert("method".to_owned(), Value::from(method));
    error(
        codes::ROLE_NOT_ALLOWED,
        &format!("a {name} connection may not call {method}"),
    )
    .with_hint(
        "cli and tui may use every pty method; remote may list, read, subscribe read-only \
         and write; app may list, read and subscribe read-only.",
    )
    .with_detail(detail)
    .expect("role_not_allowed's detail is an object")
}

/// The error a refused PTY call gets.
fn pty_refused(refused: pty::Refused) -> ErrorBody {
    match refused {
        pty::Refused::TooMany => {
            let mut detail = Data::new();
            detail.insert("max".to_owned(), Value::from(pty::MAX_PTYS));
            error(
                codes::TOO_MANY_PTYS,
                &format!("the home holds {} ptys, the most it may", pty::MAX_PTYS),
            )
            .with_hint("pty.kill one first: an exited pty counts until it is killed.")
            .with_detail(detail)
            .expect("too_many_ptys' detail is an object")
        }
        pty::Refused::Unknown(id) => error(codes::UNKNOWN_PTY, &format!("no pty {id}"))
            .with_hint("pty.list names every pty of the home."),
        pty::Refused::Exited(id) => {
            error(codes::PTY_EXITED, &format!("pty {id}'s process has ended"))
                .with_hint("Its screen stays until pty.kill.")
        }
        pty::Refused::SpawnFailed(why) => error(codes::SPAWN_FAILED, &why),
        pty::Refused::Invalid(why) => error(codes::INVALID_PARAMS, &why),
        pty::Refused::Busy(why) => error(codes::PTY_BUSY, &why)
            .with_hint("Nothing of it was taken: write again once the program reads its input."),
        pty::Refused::ScreenFailed(id) => error(
            codes::PTY_EXITED,
            &format!("pty {id}'s screen model failed, so rlxd is ending its process"),
        )
        .with_hint("Its screen says error: screen_failed, and stays, blank, until pty.kill."),
    }
}

/// `frame_too_large`'s reply, to a frame rlxd skipped.
fn frame_too_large() -> Envelope {
    let mut detail = Data::new();
    detail.insert("max".to_owned(), Value::from(MAX_FRAME));
    let body = error(
        codes::FRAME_TOO_LARGE,
        &format!("the frame is longer than the {MAX_FRAME} bytes rlxd reads: it was skipped"),
    )
    .with_hint("Send less in one request: a pty.write carries at most 128 KiB.")
    .with_detail(detail)
    .expect("frame_too_large's detail is an object");
    Envelope::err(body)
}

/// Refuses a `pty.write` whose input is more than [`MAX_PTY_WRITE`] bytes, by name,
/// before its params are read: `write_too_large {max, bytes}`.
fn write_fits(params: &Params) -> Result<(), ErrorBody> {
    // Each input there is, whatever else is there (a `null` beside it, or a second
    // input, which the params refuse after): the largest is the write's size (#2649).
    let size = |value: Option<&Value>| match value {
        Some(Value::String(text)) => text.len(),
        Some(Value::Array(keys)) => keys.iter().filter_map(Value::as_str).map(str::len).sum(),
        _ => 0,
    };
    let bytes = ["text", "paste", "keys"]
        .into_iter()
        .map(|field| size(params.get(field)))
        .max()
        .unwrap_or(0);
    if bytes <= rlxd_proto::MAX_PTY_WRITE {
        return Ok(());
    }
    let mut detail = Data::new();
    detail.insert("max".to_owned(), Value::from(rlxd_proto::MAX_PTY_WRITE));
    detail.insert("bytes".to_owned(), Value::from(bytes));
    Err(error(
        codes::WRITE_TOO_LARGE,
        &format!(
            "the write is {bytes} bytes, more than the {} one carries",
            rlxd_proto::MAX_PTY_WRITE
        ),
    )
    .with_hint("Nothing of it was taken: send it in parts.")
    .with_detail(detail)
    .expect("write_too_large's detail is an object"))
}

/// A screen of `long` bytes, longer than a client reads.
fn screen_too_long(long: usize) -> ErrorBody {
    error(
        codes::SCREEN_TOO_LONG,
        &format!(
            "the screen is {long} bytes, longer than the {} a client reads",
            bus::MAX_EVENT
        ),
    )
    .with_hint("Ask for less scrollback.")
}

/// The methods this rlxd serves after `hello`.
fn served() -> Vec<&'static str> {
    let mut served = vec![
        methods::STATUS,
        methods::STOP,
        methods::HOOK_PING,
        methods::EVENTS_SUBSCRIBE,
        methods::SETTINGS_GET,
        methods::SETTINGS_SET,
        methods::SETTINGS_LIST,
        methods::SETTINGS_DELETE,
        methods::ENV_GET,
        methods::ENV_SET,
        methods::GATE_PAIR,
        methods::GATE_PAIR_REQUESTS,
        methods::GATE_PAIR_ANSWER,
        methods::GATE_DEVICES,
        methods::GATE_UNPAIR,
        methods::PTY_SPAWN,
        methods::PTY_WRITE,
        methods::PTY_RESIZE,
        methods::PTY_SCREEN,
        methods::PTY_SUBSCRIBE,
        methods::PTY_KILL,
        methods::PTY_LIST,
        methods::RUN_START,
        methods::RUN_CANCEL,
        methods::RUN_RESUME,
        methods::RUN_GET,
        methods::RUN_LIST,
        methods::RUN_EVENTS,
        methods::RUN_DECIDE,
        methods::SPACE_ENSURE,
        methods::SPACE_ERASE,
        methods::JOB_PUT,
        methods::JOB_LIST,
        methods::JOB_DELETE,
        methods::JOB_PAUSE,
        methods::JOB_RESUME,
        methods::JOB_RUN_NOW,
    ];
    if cfg!(feature = "debug-methods") {
        served.push(methods::DEBUG_EMIT);
        served.push(methods::DEBUG_PTY_PANIC);
        served.push(methods::DEBUG_RUN_START);
        served.push(methods::DEBUG_CLOCK_ADVANCE);
        served.push(methods::DEBUG_CLOCK_SET);
        served.push(methods::DEBUG_PRE_TOOL_USE);
        served.push(methods::DEBUG_DROP_HOOK_CONNECTIONS);
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

/// The `not_gate` for `what`, an env change from anything but the gate holder (4.3).
fn env_not_gate(what: &str) -> ErrorBody {
    error(
        codes::NOT_GATE,
        &format!(
            "only an app-role connection holding the gate, a paired device, may change the \
             app's environment ({what})"
        ),
    )
    .with_hint(
        "PATH, CLAUDE_CONFIG_DIR and CLAUDE_SECURESTORAGE_CONFIG_DIR decide which claude \
         every run starts, so only rlx's app sets them. `rlxd call env.get` reads them.",
    )
}

/// The error a refused env change gets: `bad_env_key`, or `invalid_params` for a value.
fn env_refused(refusal: EnvRefusal) -> ErrorBody {
    match &refusal {
        EnvRefusal::BadKey(_) => error(codes::BAD_ENV_KEY, &refusal.to_string()),
        EnvRefusal::BadValue { .. } => error(codes::INVALID_PARAMS, &refusal.to_string()),
    }
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
