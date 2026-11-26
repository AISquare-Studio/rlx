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
//!   connection goes on. An unknown method gets `unknown_method`.
//! - **The gate** (`gate`): `gate.pair`, `gate.pair_requests`, `gate.pair_answer`,
//!   `gate.devices` and `gate.unpair`. An app connection whose credential is replaced, or
//!   whose device is unpaired, gets a last `not_gate` envelope (with no `id`) and is
//!   closed.

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

use crate::gate::{self, Answer, AnswerError, Gate, Hold, Refused, Route, UnpairError};
use crate::log::{info, warning};
use crate::peer::Peer;
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
    gate: Arc<Gate>,
}

/// What a connection was let in as.
struct Session {
    protocol_version: u32,
    role: Role,
    peer: Arc<Peer>,
    /// For `role: app`: its hold on the gate. App connections are counted by the gate.
    app: Option<Hold>,
    _counted: Option<Counted>,
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
    /// The reply carries a new gate credential for `device`, whose SHA-256 is `sha256`.
    /// If it can't be delivered, the device is unpaired, so nobody is left paired with a
    /// credential nobody received. With `close`, the connection's own credential was the
    /// one replaced: it closes after the reply.
    Paired {
        device: String,
        sha256: String,
        close: bool,
    },
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
    pub fn new(home: Home, token: String, pipe_acl: Option<&'static str>, gate: Gate) -> Arc<Self> {
        Arc::new(Self {
            home,
            token,
            pid: std::process::id(),
            pipe_acl,
            clients: Mutex::new(Clients::default()),
            claude: tokio::sync::Mutex::new(None),
            stop: watch::Sender::new(false),
            gate: Arc::new(gate),
        })
    }

    /// Tells every task to stop: the accept loop and each connection.
    pub fn stop(&self, why: &str) {
        if !self.stop.send_replace(true) {
            info!("stopping: {why}");
        }
    }

    /// Writes what only memory holds, before rlxd exits: when each gate device was last
    /// seen.
    pub fn flush(&self) {
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
        let params = request.params;
        let reply = |result: Result<Data, ErrorBody>, then| match result {
            Ok(data) => (Envelope::ok(data).with_optional_id(id), then),
            Err(error) => (Envelope::err(error).with_optional_id(id), Then::GoOn),
        };
        match request.method.as_str() {
            methods::STATUS => reply(self.status(session).await, Then::GoOn),
            methods::GATE_PAIR => {
                let (result, then) = self.gate_pair(session, &params).await;
                reply(result, then)
            }
            methods::GATE_PAIR_REQUESTS => reply(self.gate_pair_requests(session), Then::GoOn),
            methods::GATE_PAIR_ANSWER => reply(self.gate_pair_answer(session, &params), Then::GoOn),
            methods::GATE_DEVICES => {
                let data = json!({"devices": self.gate.devices()});
                reply(Ok(object(&data)), Then::GoOn)
            }
            methods::GATE_UNPAIR => reply(self.gate_unpair(session, &params), Then::GoOn),
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
        status.clients.app = self.gate.app_connections();
        if self.gate.dev_mode() {
            StatusData::GATE_DEV_MODE.clone_into(&mut status.gate);
        }
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
    async fn gate_pair(&self, session: &Session, params: &Data) -> (Result<Data, ErrorBody>, Then) {
        let params: PairParams = match parse(params) {
            Ok(params) => params,
            Err(error) => return (Err(error), Then::GoOn),
        };
        let refuse = |message: String| {
            info!("gate.pair refused ({}): {message} (not_gate)", session.peer);
            (
                Err(error(codes::NOT_GATE, &message).with_hint(NOT_GATE_HINT)),
                Then::GoOn,
            )
        };
        if let Some(route) = params.route.as_deref()
            && route != Route::Local.name()
        {
            return refuse(format!(
                "a connection to rlxd's socket pairs only route local, not {route:?}: an \
                 SSH device pairs from inside its SSH session, never through the socket"
            ));
        }
        if let Some(device) = params.device.as_deref()
            && let Err(message) = gate::valid_device(device)
        {
            return (Err(error(codes::INVALID_REQUEST, &message)), Then::GoOn);
        }
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
        let result = self.gate.pair(
            params.device.as_deref(),
            params.gate_credential.as_deref(),
            caller,
        );
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
                return (Err(error(codes::INTERNAL, &message)), Then::GoOn);
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
        let then = Then::Paired {
            device: paired.device,
            sha256: paired.sha256,
            close: paired.drop_caller,
        };
        (Ok(object(&data)), then)
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
            Some(Ok(Ok(Answer::Approved(by)))) => {
                info!("gate: pairing request {id} for {device:?} approved by {by}");
                Ok(())
            }
            Some(Ok(Ok(Answer::Denied(by)))) => {
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

/// A method's params, as `T`, or `invalid_request` saying why not.
fn parse<T: serde::de::DeserializeOwned>(params: &Data) -> Result<T, ErrorBody> {
    T::deserialize(Value::Object(params.clone())).map_err(|e| {
        error(
            codes::INVALID_REQUEST,
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
pub async fn serve_connection<S>(daemon: Arc<Daemon>, stream: S, peer: Peer)
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
    let session = match daemon.hello(&frame, Arc::new(peer)).await {
        Ok((session, reply)) => {
            if send(&mut writer, &reply).await.is_err() {
                return;
            }
            session
        }
        Err(reply) => return refuse(&mut frames, &mut writer, &reply).await,
    };
    // A frame the client sent while its last request was still being answered.
    let mut early = None;
    loop {
        let frame = match early.take() {
            Some(frame) => frame,
            None => tokio::select! {
                frame = frames.next() => frame,
                () = daemon.stopped() => return linger(&mut frames).await,
                () = kicked(&session) => return dropped(&session, &mut frames, &mut writer).await,
            },
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
        if session.app.as_ref().is_some_and(Hold::is_kicked) {
            return dropped(&session, &mut frames, &mut writer).await;
        }
        // While the request is answered, which for a new device's gate.pair can take two
        // minutes, the connection is still watched. A kick aborts the request: anything
        // that acts as the gate (the gate task's run.decide) must still check its hold
        // when it acts. A client that leaves aborts it too, so a waiting pairing request
        // is withdrawn and never pairs a device nobody is there to receive.
        let outcome = {
            let answering = daemon.answer(&session, &frame);
            tokio::pin!(answering);
            loop {
                tokio::select! {
                    answered = &mut answering => break Outcome::Answered(answered),
                    () = kicked(&session) => break Outcome::Kicked,
                    next = frames.next(), if early.is_none() => match next {
                        Ok(None) | Err(_) => break Outcome::Left,
                        next => early = Some(next),
                    },
                }
            }
        };
        let (reply, then) = match outcome {
            Outcome::Answered(answered) => answered,
            Outcome::Kicked => return dropped(&session, &mut frames, &mut writer).await,
            Outcome::Left => {
                info!("a client left before its request was answered; the request is dropped");
                return;
            }
        };
        if send(&mut writer, &reply).await.is_err() {
            if let Then::Paired { device, sha256, .. } = &then {
                daemon.gate.revoke_undelivered(device, sha256);
            }
            return;
        }
        match then {
            Then::GoOn | Then::Paired { close: false, .. } => {}
            Then::Stop => daemon.stop("a client asked"),
            Then::Paired { close: true, .. } => {
                return dropped(&session, &mut frames, &mut writer).await;
            }
        }
    }
}

/// How an in-flight request ended.
enum Outcome {
    /// It was answered: the reply, and what then.
    Answered((Envelope, Then)),
    /// The connection was dropped from the gate meanwhile.
    Kicked,
    /// The client closed the connection meanwhile.
    Left,
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
    /// The frame read so far, kept here so that `next` can be cancelled and called again
    /// without losing it.
    line: Vec<u8>,
}

impl<R: AsyncRead + Unpin> Frames<R> {
    /// The next frame, skipping blank lines, or `None` once the client has closed.
    /// Cancel-safe: a frame read in part is kept for the next call.
    async fn next(&mut self) -> std::io::Result<Option<Frame>> {
        let line = &mut self.line;
        loop {
            let chunk = self.reader.fill_buf().await?;
            if chunk.is_empty() {
                let line = std::mem::take(line);
                return Ok((!is_blank(&line)).then(|| text(line)));
            }
            let (part, used, ended) = match chunk.iter().position(|&b| b == b'\n') {
                Some(at) => (&chunk[..at], at + 1, true),
                None => (chunk, chunk.len(), false),
            };
            if line.len() + part.len() > MAX_FRAME {
                self.reader.consume(used);
                line.clear();
                return Ok(Some(Frame::TooLong));
            }
            line.extend_from_slice(part);
            self.reader.consume(used);
            if ended {
                if is_blank(line) {
                    line.clear();
                    continue;
                }
                return Ok(Some(text(std::mem::take(line))));
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
    vec![
        methods::STATUS,
        methods::STOP,
        methods::GATE_PAIR,
        methods::GATE_PAIR_REQUESTS,
        methods::GATE_PAIR_ANSWER,
        methods::GATE_DEVICES,
        methods::GATE_UNPAIR,
    ]
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
