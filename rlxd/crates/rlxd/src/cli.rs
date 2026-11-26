//! The client commands, `rlxd call`, `rlxd status` and `rlxd stop` (handoff 0.10.3, "How
//! to read the checks", 1.3 and 7.2). Each opens a new connection to this home's rlxd,
//! says `hello`, sends one request and prints the reply envelope. For a streaming method,
//! `events.subscribe`, `rlxd call` prints each event as a JSON line instead, and exits 0
//! when the stream ends (`crate::events` is `rlxd events`).

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use rlxd_client::{CallError, ConnectError, Connection, Home, Incoming, Reply};
use rlxd_proto::{Data, PROTOCOL_VERSION, StatusData, VERSION, codes, methods};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{lock, out};

/// The `client` a `hello` from these commands names, unless `--client` says otherwise.
const CLIENT: &str = "rlxd-cli";

/// How long `rlxd stop` waits for rlxd to let its lock go.
const STOP_WAIT: Duration = Duration::from_secs(10);

/// The methods whose reply ends a stream of events, which `rlxd call` prints instead.
const STREAMING: &[&str] = &[methods::EVENTS_SUBSCRIBE];

/// `rlxd call`'s options, as the command line gave them.
#[derive(Debug, Default)]
pub struct CallArgs {
    /// Say `hello {role: "app"}`, with the test gate credential when there is one.
    pub as_app: bool,
    /// `hello`'s `role`, as written.
    pub role: Option<String>,
    /// `hello`'s `client`.
    pub client: Option<String>,
    /// `hello`'s `gate_credential`.
    pub gate_credential: Option<String>,
    /// `hello`'s `token`, instead of the token file's.
    pub token: Option<String>,
    /// Connect to this socket or pipe instead of the home's, such as an SSH forward's
    /// local end.
    pub socket: Option<PathBuf>,
    /// `hello`'s `protocol_version`, as JSON writes a number.
    pub protocol_version: Option<String>,
    /// `hello`'s `env`, a JSON object.
    pub env: Option<String>,
    /// Send stdin's frames as they are, with no `hello`.
    pub raw: bool,
    /// The method to call.
    pub method: Option<String>,
    /// Its params, a JSON object.
    pub params: Option<String>,
}

/// Why a request got no reply to print.
pub enum AskError {
    /// No rlxd runs for this home.
    NotRunning(String),
    /// `hello` was refused; the refusal.
    Refused(Box<Reply>),
    /// Anything else, said.
    Failed(String),
}

/// Says `hello` as `cli` to `home`'s rlxd, with the token file's token, then asks
/// `method` with no params. The reply may be `ok: false`.
pub fn ask(home: &Home, method: &str) -> Result<Reply, AskError> {
    open(home, false, &Target::default())?
        .request(method, Data::new())
        .map_err(|e| AskError::Failed(e.to_string()))
}

/// Where a client command connects and the token it says `hello` with: `--socket` and
/// `--token` when given, else `home`'s socket and token file.
#[derive(Debug, Default)]
pub struct Target {
    /// The socket or pipe, such as an SSH forward's local end.
    pub socket: Option<PathBuf>,
    /// The token.
    pub token: Option<String>,
}

/// A connection to `target`, else `home`'s rlxd, that has said `hello` with its token: as
/// `cli`, or with `as_app` as `app`, with the test gate credential when there is one.
pub fn open(home: &Home, as_app: bool, target: &Target) -> Result<Connection, AskError> {
    let mut connection = connect(home, target.socket.as_deref())?;
    let token = match &target.token {
        Some(token) => token.clone(),
        None => read_token(home).map_err(AskError::Failed)?,
    };
    let role = if as_app { "app" } else { "cli" };
    let mut hello = hello_params(&token, &json!(PROTOCOL_VERSION), CLIENT, role);
    if let Some(credential) = as_app.then(|| test_gate_credential(home)).flatten() {
        hello.insert("gate_credential".to_owned(), Value::String(credential));
    }
    let reply = connection
        .request(methods::HELLO, hello)
        .map_err(|e| AskError::Failed(e.to_string()))?;
    if !reply.is_ok() {
        return Err(AskError::Refused(Box::new(reply)));
    }
    Ok(connection)
}

/// `rlxd call`.
pub fn call(args: CallArgs) -> ExitCode {
    let home = match Home::from_env() {
        Ok(home) => home,
        Err(e) => return report(AskError::NotRunning(e.to_string())),
    };
    if args.raw {
        return raw(&home, args.socket.as_deref());
    }
    let Some(method) = args.method.as_deref() else {
        return out::usage("call needs a method, or --raw");
    };
    let params = match parse_object(args.params.as_deref(), "<params-json>") {
        Ok(params) => params.unwrap_or_default(),
        Err(message) => return out::usage(&message),
    };
    let env = match parse_object(args.env.as_deref(), "--env") {
        Ok(env) => env,
        Err(message) => return out::usage(&message),
    };
    let version = match args.protocol_version.as_deref() {
        None => json!(PROTOCOL_VERSION),
        Some(text) => match serde_json::from_str::<Value>(text) {
            Ok(number @ Value::Number(_)) => number,
            _ => return out::usage(&format!("--protocol-version {text} is not a number")),
        },
    };
    let mut connection = match connect(&home, args.socket.as_deref()) {
        Ok(connection) => connection,
        Err(e) => return report(e),
    };
    let token = match args.token.clone().map_or_else(|| read_token(&home), Ok) {
        Ok(token) => token,
        Err(message) => return out::fail(codes::UNAUTHORIZED, &message, None),
    };
    let role = if args.as_app {
        "app"
    } else {
        args.role.as_deref().unwrap_or("cli")
    };
    let client = args.client.as_deref().unwrap_or(CLIENT);
    let mut hello = hello_params(&token, &version, client, role);
    let credential = args
        .gate_credential
        .clone()
        .or_else(|| args.as_app.then(|| test_gate_credential(&home)).flatten());
    if let Some(credential) = credential {
        hello.insert("gate_credential".to_owned(), Value::String(credential));
    }
    if let Some(env) = env {
        hello.insert("env".to_owned(), Value::Object(env));
    }
    let reply = match connection.request(methods::HELLO, hello) {
        Ok(reply) => reply,
        Err(e) => return report(call_error(e)),
    };
    if !reply.is_ok() {
        return out::print_reply(&reply);
    }
    if STREAMING.contains(&method) {
        return print_stream(connection, method, params);
    }
    match connection.request(method, params) {
        Ok(reply) => out::print_reply(&reply),
        Err(e) => report(call_error(e)),
    }
}

/// Sends a streaming method's request, and prints each event of its stream as it came.
/// Exits 0 when its reply ends the stream `ok`; a refusal is printed, and exits 1.
fn print_stream(mut connection: Connection, method: &str, params: Data) -> ExitCode {
    let id = match connection.send(method, params) {
        Ok(id) => id,
        Err(e) => return report(call_error(e)),
    };
    loop {
        match connection.read_incoming() {
            Ok(Incoming::Event(event)) => {
                if !out::print_line(&event.line) {
                    return ExitCode::from(out::FAILED);
                }
            }
            Ok(Incoming::Reply(reply)) if reply.envelope.id.is_none_or(|i| i == id) => {
                return if reply.is_ok() {
                    ExitCode::SUCCESS
                } else {
                    out::print_reply(&reply)
                };
            }
            Ok(Incoming::Reply(_)) => {}
            Err(e) => return report(call_error(e)),
        }
    }
}

/// `rlxd call --raw`: stdin's frames, sent as they are with no `hello`, and each reply
/// printed as it came. Exits as the last reply says.
fn raw(home: &Home, socket: Option<&Path>) -> ExitCode {
    let mut input = String::new();
    if let Err(e) = io::stdin().read_to_string(&mut input) {
        return out::usage(&format!("cannot read stdin: {e}"));
    }
    let frames: Vec<&str> = input.lines().filter(|l| !l.trim().is_empty()).collect();
    if frames.is_empty() {
        return out::usage("--raw sends stdin's frames, and stdin has none");
    }
    let mut connection = match connect(home, socket) {
        Ok(connection) => connection,
        Err(e) => return report(e),
    };
    for frame in &frames {
        // rlxd closes the connection on a refused first frame: what it already sent
        // is still read below.
        if connection.send_line(frame).is_err() {
            break;
        }
    }
    let mut last = None;
    for _ in &frames {
        match connection.read_reply() {
            Ok(reply) => {
                out::print_line(&reply.line);
                last = Some(reply.is_ok());
            }
            Err(CallError::Malformed { line, .. }) => {
                out::print_line(&line);
                last = Some(false);
            }
            Err(_) => break,
        }
    }
    match last {
        Some(true) => ExitCode::SUCCESS,
        Some(false) => ExitCode::from(out::FAILED),
        None => report(AskError::Failed(
            "rlxd closed the connection before it replied".to_owned(),
        )),
    }
}

/// `rlxd status [--json]`.
pub fn status(json: bool) -> ExitCode {
    let home = match Home::from_env() {
        Ok(home) => home,
        Err(e) => return report(AskError::NotRunning(e.to_string())),
    };
    match ask(&home, methods::STATUS) {
        Ok(reply) if json => out::print_reply(&reply),
        Ok(reply) => describe(&reply),
        Err(error) if json => report(error),
        Err(error) => report_to_a_person(error),
    }
}

/// `rlxd stop`: asks rlxd to stop, then waits until it has let its lock go, which it
/// does last.
pub fn stop() -> ExitCode {
    let home = match Home::from_env() {
        Ok(home) => home,
        Err(e) => return report(AskError::NotRunning(e.to_string())),
    };
    let reply = match ask(&home, methods::STOP) {
        Ok(reply) => reply,
        Err(error) => return report(error),
    };
    if !reply.is_ok() {
        return out::print_reply(&reply);
    }
    let pid = reply.data().and_then(|data| data.get("pid")).cloned();
    let deadline = Instant::now() + STOP_WAIT;
    loop {
        match lock::is_held(&home.lock()) {
            Ok(false) => break,
            Ok(true) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Ok(true) => {
                let message = format!(
                    "rlxd took the request but still runs after {} s",
                    STOP_WAIT.as_secs()
                );
                let hint = format!("Its log is {}.", home.log_file().display());
                return out::fail(codes::STOP_TIMED_OUT, &message, Some(&hint));
            }
            Err(e) => {
                let message = format!("cannot read {}: {e}", home.lock().display());
                return out::fail(codes::INTERNAL, &message, None);
            }
        }
    }
    let mut data = Data::new();
    data.insert("stopped".to_owned(), Value::Bool(true));
    if let Some(pid) = pid {
        data.insert("pid".to_owned(), pid);
    }
    out::succeed(data)
}

/// A status, written for a person.
fn describe(reply: &Reply) -> ExitCode {
    let status = reply
        .data()
        .and_then(|data| StatusData::deserialize(Value::Object(data.clone())).ok());
    let Some(status) = status else {
        return out::print_reply(reply);
    };
    let claude = &status.claude;
    let claude = match (claude.installed, claude.signed_in) {
        (false, _) => "not found".to_owned(),
        (true, signed_in) => {
            let version = claude.version.as_deref().unwrap_or("version unknown");
            let account = claude.account.as_deref().unwrap_or("an account");
            if signed_in {
                format!("{version}, signed in as {account}")
            } else {
                format!("{version}, not signed in")
            }
        }
    };
    let c = status.clients;
    let text = format!(
        "rlxd {} running, pid {}, on {}\nhome: {}\nsocket: {}\nclients: app {}, tui {}, \
         remote {}, cli {}\nclaude: {claude}",
        status.version,
        status.pid,
        status.platform,
        status.paths.home,
        status.paths.socket,
        c.app,
        c.tui,
        c.remote,
        c.cli
    );
    if out::print_line(&text) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// `hello`'s params, as `rlxd call` sends them: written out, not built through
/// `HelloParams`, so a test can send what rlxd must refuse.
fn hello_params(token: &str, version: &Value, client: &str, role: &str) -> Data {
    let hello = json!({
        "token": token,
        "protocol_version": version,
        "client": client,
        "client_version": VERSION,
        "role": role,
        "capabilities": [],
    });
    match hello {
        Value::Object(hello) => hello,
        _ => unreachable!("an object literal"),
    }
}

fn read_token(home: &Home) -> Result<String, String> {
    let path = home.token();
    std::fs::read_to_string(&path)
        .map(|token| token.trim().to_owned())
        .map_err(|e| format!("cannot read the token in {}: {e}", path.display()))
}

/// The gate credential a test client's first `--as-app` call kept, if there is one.
fn test_gate_credential(home: &Home) -> Option<String> {
    let text = std::fs::read_to_string(home.test_gate_credential()).ok()?;
    let credential = text.trim();
    (!credential.is_empty()).then(|| credential.to_owned())
}

/// `text`, a JSON object, or `None` when not given.
fn parse_object(text: Option<&str>, what: &str) -> Result<Option<Data>, String> {
    let Some(text) = text else {
        return Ok(None);
    };
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(object)) => Ok(Some(object)),
        Ok(_) => Err(format!("{what} must be a JSON object")),
        Err(e) => Err(format!("{what} is not JSON: {e}")),
    }
}

/// A connection to `socket` when given (`--socket`), else to `home`'s rlxd.
pub fn connect(home: &Home, socket: Option<&Path>) -> Result<Connection, AskError> {
    match socket {
        Some(socket) => Connection::open_at(socket).map_err(|error| match error {
            ConnectError::NotRunning { .. } => {
                AskError::NotRunning(format!("no rlxd listens on {}", socket.display()))
            }
            other => AskError::Failed(other.to_string()),
        }),
        None => Connection::open(home).map_err(|error| connect_error(home, error)),
    }
}

fn connect_error(home: &Home, error: ConnectError) -> AskError {
    match error {
        ConnectError::NotRunning { .. } => {
            AskError::NotRunning(format!("rlxd is not running for {}", home.root().display()))
        }
        other => AskError::Failed(other.to_string()),
    }
}

fn call_error(error: CallError) -> AskError {
    AskError::Failed(error.to_string())
}

/// Prints the envelope for `error`, with its exit code.
fn report(error: AskError) -> ExitCode {
    match error {
        AskError::NotRunning(message) => out::fail_with(
            out::NOT_RUNNING,
            codes::NOT_RUNNING,
            &message,
            Some("Start it with `rlxd serve --detach`."),
        ),
        AskError::Refused(reply) => out::print_reply(&reply),
        AskError::Failed(message) => out::fail(codes::DISCONNECTED, &message, None),
    }
}

/// Prints the envelope for `error` on stderr, with its exit code: for `rlxd events`, whose
/// stdout carries events only.
pub fn report_on_stderr(error: AskError) -> ExitCode {
    match error {
        AskError::NotRunning(message) => out::fail_on_stderr(
            out::NOT_RUNNING,
            codes::NOT_RUNNING,
            &message,
            Some("Start it with `rlxd serve --detach`."),
        ),
        AskError::Refused(reply) => out::print_reply_on_stderr(&reply),
        AskError::Failed(message) => {
            out::fail_on_stderr(out::FAILED, codes::DISCONNECTED, &message, None)
        }
    }
}

/// `error`, on stderr, for a person, with its exit code.
fn report_to_a_person(error: AskError) -> ExitCode {
    let (message, status) = match error {
        AskError::NotRunning(message) => (message, out::NOT_RUNNING),
        AskError::Refused(reply) => {
            let message = reply.error().map_or_else(
                || reply.line.clone(),
                |e| format!("{}: {}", e.code, e.message),
            );
            (message, out::FAILED)
        }
        AskError::Failed(message) => (message, out::FAILED),
    };
    eprintln!("rlxd: {message}");
    ExitCode::from(status)
}
