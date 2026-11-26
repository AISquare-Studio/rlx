//! What rlxd says on a connection (handoff 0.10.3, 7.2): `hello` first, then requests,
//! each answered with the envelope plus its `id`.
//!
//! - **`hello`.** A `protocol_version` this rlxd does not serve gets
//!   `incompatible_protocol`, read from the raw params before anything else, so even a
//!   hello this rlxd cannot parse gets it. Anything but a valid hello with the token gets
//!   `unauthorized`. A valid one with `role: app` gets `not_gate`: gate pairing is not
//!   here yet, so no connection may be the gate (fail closed). A refused hello closes the
//!   connection and is never counted.
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
//! - **Stopping.** `daemon.stopping {reason}` is published before any connection learns
//!   of the stop, so every stream carries it before its reply.
//! - **`debug.emit {count}`**, in test builds only (the `debug-methods` feature), publishes
//!   `count` `daemon.debug {index, count}` events. A release build answers
//!   `unknown_method`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rlxd_client::Home;
use rlxd_proto::{
    Claude, Clients, Data, Envelope, ErrorBody, HelloData, HelloParams, PROTOCOL_VERSION,
    PROTOCOL_VERSIONS, Params, Paths, Request, RequestId, Role, StatusData, SubscribeParams,
    VERSION, codes, event_types, methods, topics,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::watch;

use crate::bus::{self, Bus, Cursor, Line, Topics};
use crate::log::{info, warning};
use crate::{claude, token};

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
    bus: Bus,
    stopping: AtomicBool,
    stop: watch::Sender<bool>,
}

/// What a connection was let in as.
struct Session {
    protocol_version: u32,
    _counted: Counted,
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
    /// The event stream, whose last frame is the reply.
    Stream(Subscription),
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
    /// An rlxd serving `home` with `token`, publishing on `bus`. `pipe_acl` is what
    /// `status` says of the pipe's DACL, on Windows.
    pub fn new(home: Home, token: String, pipe_acl: Option<&'static str>, bus: Bus) -> Arc<Self> {
        Arc::new(Self {
            home,
            token,
            pid: std::process::id(),
            pipe_acl,
            clients: Mutex::new(Clients::default()),
            claude: tokio::sync::Mutex::new(None),
            bus,
            stopping: AtomicBool::new(false),
            stop: watch::Sender::new(false),
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
        let data = object(&json!({ "reason": reason }));
        if let Err(e) = self
            .bus
            .publish(topics::DAEMON, event_types::DAEMON_STOPPING, data)
        {
            warning!("daemon.stopping was not published: {e}");
        }
        self.stop.send_replace(true);
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
    fn hello(self: &Arc<Self>, frame: &str) -> Result<(Session, Envelope), Envelope> {
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
        let hello = match HelloParams::deserialize(Value::Object(request.params)) {
            Ok(hello) => hello,
            Err(e) => return refuse(id, format!("hello's params are not valid: {e}")),
        };
        if !token::matches(&self.token, &hello.token) {
            info!("hello refused: a wrong token, from {}", hello.client);
            let hint = format!("rlxd's token is in {}.", self.home.token().display());
            let error = error(codes::UNAUTHORIZED, "the token is wrong").with_hint(hint);
            return Err(Envelope::err(error).with_optional_id(id));
        }
        if hello.role == Role::App {
            info!("hello refused: role app, from {} (not_gate)", hello.client);
            let error = error(
                codes::NOT_GATE,
                "this rlxd lets no connection be the gate: gate pairing is not available yet",
            )
            .with_hint("Connect with role tui, remote or cli.");
            return Err(Envelope::err(error).with_optional_id(id));
        }
        let Some(counted) = Counted::new(self, hello.role) else {
            let message = format!("role {} is not one this rlxd serves", role_name(hello.role));
            return refuse(id, message);
        };
        info!(
            "hello: {} {} as {}",
            hello.client,
            hello.client_version,
            role_name(hello.role)
        );
        let reply = HelloData::new(PROTOCOL_VERSION, VERSION, capabilities())
            .map(|data| Envelope::ok(object(&data)).with_id(request.id))
            .map_err(|e| reply_err(id, codes::INTERNAL, e.to_string()))?;
        let session = Session {
            protocol_version: hello.protocol_version,
            _counted: counted,
        };
        Ok((session, reply))
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
            methods::DEBUG_EMIT => self.debug_emit(&request.params),
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

    /// `debug.emit {count}`: `count` events, `daemon.debug {index, count}`, published one
    /// at a time. Test builds only.
    #[cfg(feature = "debug-methods")]
    fn debug_emit(&self, params: &Params) -> Result<Data, ErrorBody> {
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
        status.claude = self.claude().await;
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
        Role::App => Some(&mut clients.app),
        Role::Tui => Some(&mut clients.tui),
        Role::Remote => Some(&mut clients.remote),
        Role::Cli => Some(&mut clients.cli),
        _ => None,
    }
}

/// Serves one connection until the client closes it, it is refused, or rlxd stops.
pub async fn serve_connection<S>(daemon: Arc<Daemon>, stream: S)
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
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
    let session = match daemon.hello(&frame) {
        Ok((session, reply)) => {
            if send(&mut writer, &reply).await.is_err() {
                return;
            }
            session
        }
        Err(reply) => return refuse(&mut frames, &mut writer, &reply).await,
    };
    loop {
        let frame = tokio::select! {
            frame = frames.next() => frame,
            () = daemon.stopped() => return linger(&mut frames).await,
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
        match daemon.answer(&session, &frame, false).await {
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
            Answer::Stream(subscription) => {
                let flow =
                    send_stream(&daemon, &session, &mut frames, &mut writer, subscription).await;
                match flow {
                    Flow::GoOn => {}
                    Flow::Linger => return linger(&mut frames).await,
                    Flow::Close => return,
                }
            }
        }
    }
}

/// Sends `subscription`'s stream: the events after its place, a batch at a time, then
/// live ones while it follows, then its reply. Answers the client's requests meanwhile:
/// frames are read as they come, so a partial one holds up no event.
async fn send_stream<R, W>(
    daemon: &Arc<Daemon>,
    session: &Session,
    frames: &mut Frames<R>,
    writer: &mut W,
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
            frame = frames.next_line() => {
                if let Some(flow) = answer_in_stream(daemon, session, frames, writer, frame).await {
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
    frame: std::io::Result<Option<Frame>>,
) -> Option<Flow>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (reply, then_stop) = match frame {
        Ok(Some(Frame::Line(frame))) => match daemon.answer(session, &frame, true).await {
            Answer::Reply(reply) => (reply, false),
            Answer::Stop(reply) => (reply, true),
            Answer::Stream(_) => unreachable!("a streaming connection takes no second stream"),
        },
        Ok(Some(Frame::Blank)) => return None,
        Ok(Some(Frame::NotText)) => {
            let message = "the frame is not UTF-8 text".to_owned();
            (reply_err(None, codes::INVALID_REQUEST, message), false)
        }
        Ok(Some(Frame::TooLong)) => {
            let message = format!("the frame is longer than {MAX_FRAME} bytes");
            let reply = reply_err(None, codes::INVALID_REQUEST, message);
            refuse(frames, writer, &reply).await;
            return Some(Flow::Close);
        }
        Ok(None) | Err(_) => return Some(Flow::Close),
    };
    if send(writer, &reply).await.is_err() {
        return Some(Flow::Close);
    }
    if then_stop {
        daemon.stop(STOP_REQUESTED);
    }
    None
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

/// The methods this rlxd serves after `hello`.
fn served() -> Vec<&'static str> {
    let mut served = vec![methods::STATUS, methods::STOP, methods::EVENTS_SUBSCRIBE];
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
