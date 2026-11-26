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
//!   connection goes on. An unknown method gets `unknown_method`.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rlxd_client::Home;
use rlxd_proto::{
    Claude, Clients, Data, Envelope, ErrorBody, HelloData, HelloParams, PROTOCOL_VERSION,
    PROTOCOL_VERSIONS, Paths, Request, RequestId, Role, StatusData, VERSION, codes, methods,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::watch;

use crate::log::{info, warning};
use crate::{claude, token};

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

/// What the connection does after a reply.
#[derive(PartialEq, Eq)]
enum Then {
    GoOn,
    Stop,
}

/// One frame, as read.
enum Frame {
    Line(String),
    NotText,
    TooLong,
}

impl Daemon {
    /// An rlxd serving `home` with `token`. `pipe_acl` is what `status` says of the
    /// pipe's DACL, on Windows.
    pub fn new(home: Home, token: String, pipe_acl: Option<&'static str>) -> Arc<Self> {
        Arc::new(Self {
            home,
            token,
            pid: std::process::id(),
            pipe_acl,
            clients: Mutex::new(Clients::default()),
            claude: tokio::sync::Mutex::new(None),
            stop: watch::Sender::new(false),
        })
    }

    /// Tells every task to stop: the accept loop and each connection.
    pub fn stop(&self, why: &str) {
        if !self.stop.send_replace(true) {
            info!("stopping: {why}");
        }
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

    /// The reply to one request frame after `hello`.
    async fn answer(&self, session: &Session, frame: &str) -> (Envelope, Then) {
        let value = match serde_json::from_str::<Value>(frame) {
            Ok(value) => value,
            Err(e) => {
                let message = format!("the frame is not JSON: {e}");
                return (reply_err(None, codes::INVALID_REQUEST, message), Then::GoOn);
            }
        };
        let id = request_id(&value);
        let request = match Request::deserialize(value) {
            Ok(request) => request,
            Err(e) => {
                let message = format!("the frame is not a request: {e}");
                return (reply_err(id, codes::INVALID_REQUEST, message), Then::GoOn);
            }
        };
        let id = Some(request.id);
        match request.method.as_str() {
            methods::STATUS => match self.status(session).await {
                Ok(data) => (Envelope::ok(data).with_optional_id(id), Then::GoOn),
                Err(error) => (Envelope::err(error).with_optional_id(id), Then::GoOn),
            },
            methods::STOP => {
                let data = object(&json!({"pid": self.pid}));
                (Envelope::ok(data).with_optional_id(id), Then::Stop)
            }
            methods::HELLO => {
                let message = "hello was already said on this connection".to_owned();
                (reply_err(id, codes::INVALID_REQUEST, message), Then::GoOn)
            }
            method => {
                let error = error(
                    codes::UNKNOWN_METHOD,
                    &format!("rlxd has no method {method:?}"),
                )
                .with_hint(format!("The methods it serves: {}.", served().join(", ")));
                (Envelope::err(error).with_optional_id(id), Then::GoOn)
            }
        }
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
    };
    let first = tokio::select! {
        first = tokio::time::timeout(HELLO_TIMEOUT, frames.next()) => first,
        () = daemon.stopped() => return,
    };
    let frame = match first {
        Ok(Ok(Some(Frame::Line(frame)))) => frame,
        Ok(Ok(None) | Err(_)) => return,
        Ok(Ok(Some(Frame::NotText | Frame::TooLong))) => {
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
        let (reply, then) = daemon.answer(&session, &frame).await;
        if send(&mut writer, &reply).await.is_err() {
            return;
        }
        if then == Then::Stop {
            daemon.stop("a client asked");
        }
    }
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
}

impl<R: AsyncRead + Unpin> Frames<R> {
    /// The next frame, skipping blank lines, or `None` once the client has closed.
    async fn next(&mut self) -> std::io::Result<Option<Frame>> {
        let mut line = Vec::new();
        loop {
            let chunk = self.reader.fill_buf().await?;
            if chunk.is_empty() {
                return Ok((!is_blank(&line)).then(|| text(line)));
            }
            let (part, used, ended) = match chunk.iter().position(|&b| b == b'\n') {
                Some(at) => (&chunk[..at], at + 1, true),
                None => (chunk, chunk.len(), false),
            };
            if line.len() + part.len() > MAX_FRAME {
                self.reader.consume(used);
                return Ok(Some(Frame::TooLong));
            }
            line.extend_from_slice(part);
            self.reader.consume(used);
            if ended {
                if is_blank(&line) {
                    line.clear();
                    continue;
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
    vec![methods::STATUS, methods::STOP]
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
