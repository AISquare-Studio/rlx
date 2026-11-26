//! The client commands, `rlxd call`, `rlxd status`, `rlxd stop` and `rlxd gate …`
//! (handoff 0.10.3, "How to read the checks", 1.3, 5.1 and 7.2). Each opens a new
//! connection to this home's rlxd, says `hello`, sends one request and prints the reply
//! envelope. For a streaming method, `events.subscribe`, `rlxd call` prints each event as
//! a JSON line instead, and exits 0 when the stream ends (`crate::events` is `rlxd
//! events`).
//!
//! `rlxd call --as-app` is the test client of "How to read the checks": with no
//! `--gate-credential`, it presents the one kept in `<home>/test-gate-credential`. When
//! that file is missing, it pairs first (`gate.pair`, which only gate dev mode lets an
//! unsigned client do) and keeps the credential there. Like the app, it never pairs again
//! on its own: once the file exists, a `not_gate` is the call's result. `rlxd events
//! --as-app` presents the kept credential too, but never pairs: without one, it gets
//! `not_gate`. `--gate-credential-file` keeps it elsewhere.
//!
//! `--socket` connects to another rlxd's socket, such as an SSH forward's local end.
//! Nothing of this home goes there, and nothing from there is kept in it: the token must
//! be given (`--token-file`, or `--token`), and `--as-app` needs `--gate-credential-file`
//! (or `--gate-credential`). Either missing is a usage error ([`Target::check`]).

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use rlxd_client::{CallError, ConnectError, Connection, Home, Incoming, Reply};
use rlxd_proto::{Data, PROTOCOL_VERSION, StatusData, VERSION, codes, methods};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::log::warning;
use crate::{files, gate, lock, log, out};

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
    /// Where to connect, the token, and where `--as-app` keeps its credential.
    pub target: Target,
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
    /// Nothing listens on `--socket`: a forward that is down, or no rlxd on the host.
    NotListening(String),
    /// `--socket` took the connection, then closed it without a whole reply to `hello`:
    /// through an SSH forward, most likely the host's rlxd isn't running, or its sshd
    /// doesn't allow socket forwarding.
    ForwardClosed(String),
    /// The token could not be read: `unauthorized`, whatever runs in this home.
    Unauthorized(String),
    /// `hello` was refused; the refusal.
    Refused(Box<Reply>),
    /// Anything else, said.
    Failed(String),
}

/// Says `hello` as `cli` to `home`'s rlxd, with the token file's token, then asks
/// `method` with no params. The reply may be `ok: false`.
pub fn ask(home: &Home, method: &str) -> Result<Reply, AskError> {
    ask_with(home, method, Data::new())
}

/// [`ask`], with `params`.
pub fn ask_with(home: &Home, method: &str, params: Data) -> Result<Reply, AskError> {
    open(home, false, &Target::default())?
        .request(method, params)
        .map_err(|e| AskError::Failed(e.to_string()))
}

/// Where a client command connects, and what it proves itself with: `--socket`,
/// `--token` or `--token-file`, and `--gate-credential-file`, else the home's socket,
/// token file and test credential.
///
/// With `--socket`, nothing of the home's goes there and nothing from there is kept in the
/// home: the token must be given, and `--as-app` needs a credential file of its own
/// ([`Target::check`]).
#[derive(Debug, Default)]
pub struct Target {
    /// The socket or pipe, such as an SSH forward's local end.
    pub socket: Option<PathBuf>,
    /// The token, from the command line.
    pub token: Option<String>,
    /// A file holding the token, so it is not in the command line.
    pub token_file: Option<PathBuf>,
    /// Where `--as-app` keeps its gate credential, instead of the home's
    /// `test-gate-credential`.
    pub gate_credential_file: Option<PathBuf>,
}

impl Target {
    /// Refuses, as a usage error, what would send this home's token or test credential to
    /// another socket, or keep another rlxd's credential in this home. `as_app` is
    /// `--as-app` with no `--gate-credential`.
    pub fn check(&self, as_app: bool) -> Result<(), String> {
        if self.socket.is_none() {
            return Ok(());
        }
        if self.token.is_none() && self.token_file.is_none() {
            return Err(
                "--socket needs --token-file (or --token): this home's token is \
                        never sent to another socket"
                    .to_owned(),
            );
        }
        if as_app && self.gate_credential_file.is_none() {
            return Err(
                "--as-app with --socket needs --gate-credential-file, where this \
                        client keeps its credential for the rlxd at --socket (or \
                        --gate-credential): this home's test credential is never sent \
                        there, nor that rlxd's kept here"
                    .to_owned(),
            );
        }
        Ok(())
    }

    /// Whether the token is the home's token file's: no `--token`, `--token-file` or
    /// `--socket`.
    pub fn token_is_the_homes(&self) -> bool {
        self.token.is_none() && self.token_file.is_none() && self.socket.is_none()
    }

    /// The token: `--token`, else `--token-file`'s, else, without `--socket`, the home's.
    pub fn token(&self, home: &Home) -> Result<String, String> {
        match (&self.token, &self.token_file, &self.socket) {
            (Some(token), _, _) => Ok(token.clone()),
            (None, Some(file), _) => read_secret(file, "token"),
            (None, None, None) => read_token(home),
            (None, None, Some(_)) => Err("--socket needs --token-file or --token".to_owned()),
        }
    }

    /// Where `--as-app` keeps its gate credential: `--gate-credential-file`, else, without
    /// `--socket`, `<home>/test-gate-credential`. None with `--socket` and no file.
    pub fn credential_file(&self, home: &Home) -> Option<PathBuf> {
        match (&self.gate_credential_file, &self.socket) {
            (Some(file), _) => Some(file.clone()),
            (None, None) => Some(home.test_gate_credential()),
            (None, Some(_)) => None,
        }
    }
}

/// A connection to `target`, else `home`'s rlxd, that has said `hello` with its token: as
/// `cli`, or with `as_app` as `app`, with the test gate credential when there is one.
pub fn open(home: &Home, as_app: bool, target: &Target) -> Result<Connection, AskError> {
    open_as(home, if as_app { "app" } else { "cli" }, target)
}

/// A connection to `target`, else `home`'s rlxd, that has said `hello` as `role` with its
/// token, and with the test gate credential when the role is `app` and there is one.
pub fn open_as(home: &Home, role: &str, target: &Target) -> Result<Connection, AskError> {
    let as_app = role == "app";
    // The token first, as `rlxd call` reads it: one the caller named (--token-file, or
    // for --socket) is unauthorized when it can't be read, whatever runs here. Only the
    // home's own, missing where no rlxd ever ran, says not running when none does.
    let token = match target.token(home) {
        Ok(token) => token,
        Err(message) if target.token_is_the_homes() => {
            connect(home, None)?;
            return Err(AskError::Unauthorized(message));
        }
        Err(message) => return Err(AskError::Unauthorized(message)),
    };
    let mut connection = connect(home, target.socket.as_deref())?;
    let mut hello = hello_params(&token, &json!(PROTOCOL_VERSION), CLIENT, role);
    let kept = as_app.then(|| target.credential_file(home)).flatten();
    if let Some(credential) = kept.as_deref().and_then(read_credential) {
        hello.insert("gate_credential".to_owned(), Value::String(credential));
    }
    let reply = connection
        .request(methods::HELLO, hello)
        .map_err(|e| socket_call_error(e, target.socket.as_deref()))?;
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
        return raw(&home, args.target.socket.as_deref());
    }
    if let Err(message) = args
        .target
        .check(args.as_app && args.gate_credential.is_none())
    {
        return out::usage(&message);
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
    let token = match args.target.token(&home) {
        Ok(token) => token,
        // No token file in the home, most likely as no rlxd ever ran here: not running,
        // if it isn't. Only for the home's own token: one --token-file names, or one for
        // --socket, is the caller's to give, whatever this home runs.
        Err(message) if args.target.token_is_the_homes() => {
            return match Connection::open(&home) {
                Err(e) => report(connect_error(&home, e)),
                Ok(_) => out::fail(codes::UNAUTHORIZED, &message, None),
            };
        }
        Err(message) => return out::fail(codes::UNAUTHORIZED, &message, None),
    };
    let socket = args.target.socket.as_deref();
    let credential = match (args.gate_credential.clone(), args.as_app) {
        (Some(credential), _) => Some(credential),
        (None, true) => {
            let Some(file) = args.target.credential_file(&home) else {
                return out::usage("--as-app with --socket needs --gate-credential-file");
            };
            match read_credential(&file) {
                Some(credential) => Some(credential),
                None => match pair_test_client(&home, socket, &token, &file) {
                    Ok(credential) => Some(credential),
                    Err(exit) => return exit,
                },
            }
        }
        (None, false) => None,
    };
    let mut connection = match connect(&home, socket) {
        Ok(connection) => connection,
        Err(e) => return report(e),
    };
    let role = if args.as_app {
        "app"
    } else {
        args.role.as_deref().unwrap_or("cli")
    };
    let client = args.client.as_deref().unwrap_or(CLIENT);
    let mut hello = hello_params(&token, &version, client, role);
    if let Some(credential) = credential {
        hello.insert("gate_credential".to_owned(), Value::String(credential));
    }
    if let Some(env) = env {
        hello.insert("env".to_owned(), Value::Object(env));
    }
    let reply = match connection.request(methods::HELLO, hello) {
        Ok(reply) => reply,
        Err(e) => return report(socket_call_error(e, socket)),
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

/// The test client's first pairing: `gate.pair` as `cli`, keeping the credential in
/// `file` (0600): `<home>/test-gate-credential`, or `--gate-credential-file`. On a
/// refusal, prints it and gives the exit. With `socket` (`--socket`), it pairs with the
/// rlxd there, whose gate checks it as any other connection: through an SSH forward its
/// peer is sshd.
fn pair_test_client(
    home: &Home,
    socket: Option<&Path>,
    token: &str,
    file: &Path,
) -> Result<String, ExitCode> {
    let mut connection = connect(home, socket).map_err(report)?;
    let hello = hello_params(token, &json!(PROTOCOL_VERSION), CLIENT, "cli");
    let reply = connection
        .request(methods::HELLO, hello)
        .map_err(|e| report(socket_call_error(e, socket)))?;
    if !reply.is_ok() {
        return Err(out::print_reply(&reply));
    }
    let reply = connection
        .request(methods::GATE_PAIR, Data::new())
        .map_err(|e| report(call_error(e)))?;
    let credential = reply
        .data()
        .and_then(|data| data.get("gate_credential"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let Some(credential) = credential else {
        return Err(out::print_reply(&reply));
    };
    files::write_private(file, &credential).map_err(|e| {
        let message = format!(
            "paired, but cannot keep the credential in {}: {e}",
            file.display()
        );
        out::fail(codes::INTERNAL, &message, None)
    })?;
    Ok(credential)
}

/// `rlxd gate devices`: the paired devices, from rlxd, or from gate.json when it is
/// stopped.
pub fn gate_devices() -> ExitCode {
    let home = match Home::from_env() {
        Ok(home) => home,
        Err(e) => return report(AskError::NotRunning(e.to_string())),
    };
    match ask(&home, methods::GATE_DEVICES) {
        Ok(reply) => out::print_reply(&reply),
        Err(AskError::NotRunning(_)) => match gate::devices_offline(&home.gate()) {
            Ok(devices) => {
                let mut data = Data::new();
                data.insert("devices".to_owned(), Value::Array(devices));
                out::succeed(data)
            }
            Err(message) => out::fail(codes::INTERNAL, &message, None),
        },
        Err(error) => report(error),
    }
}

/// `rlxd gate pair [--request-id ID [--deny | --force]]`, as the user on this host:
/// without an id, the waiting requests, each with the process that asked; with one, its
/// answer. A request is never approved without its id.
pub fn gate_pair(request_id: Option<String>, approve: bool, force: bool) -> ExitCode {
    let home = match Home::from_env() {
        Ok(home) => home,
        Err(e) => return report(AskError::NotRunning(e.to_string())),
    };
    let Some(request_id) = request_id else {
        return match ask(&home, methods::GATE_PAIR_REQUESTS) {
            Ok(reply) => out::print_reply(&reply),
            Err(error) => report(error),
        };
    };
    let mut params = Data::new();
    params.insert("request_id".to_owned(), Value::String(request_id));
    params.insert("approve".to_owned(), Value::Bool(approve));
    params.insert("force".to_owned(), Value::Bool(force));
    match ask_with(&home, methods::GATE_PAIR_ANSWER, params) {
        Ok(reply) => out::print_reply(&reply),
        Err(error) => report(error),
    }
}

/// `rlxd gate unpair [<device>]`: unpairs through rlxd, which drops the device's app
/// connections; with rlxd stopped, edits gate.json under rlxd's lock.
pub fn gate_unpair(device: Option<String>) -> ExitCode {
    let home = match Home::from_env() {
        Ok(home) => home,
        Err(e) => return report(AskError::NotRunning(e.to_string())),
    };
    let mut params = Data::new();
    if let Some(device) = &device {
        params.insert("device".to_owned(), Value::String(device.clone()));
    }
    match ask_with(&home, methods::GATE_UNPAIR, params) {
        Ok(reply) => out::print_reply(&reply),
        Err(AskError::NotRunning(_)) => unpair_offline(&home, device.as_deref()),
        Err(error) => report(error),
    }
}

fn unpair_offline(home: &Home, device: Option<&str>) -> ExitCode {
    let _held = if home.run_dir().is_dir() {
        match lock::acquire(&home.lock(), Duration::ZERO) {
            Ok(Some(held)) => Some(held),
            Ok(None) => {
                let message = "rlxd is starting or stopping; try again".to_owned();
                return out::fail(codes::ALREADY_RUNNING, &message, None);
            }
            Err(e) => {
                let message = format!("cannot take {}: {e}", home.lock().display());
                return out::fail(codes::INTERNAL, &message, None);
            }
        }
    } else {
        None
    };
    let names = match gate::unpair_offline(&home.gate(), device) {
        Ok(names) => names,
        Err(gate::UnpairError::Unknown(name)) => {
            let message = format!("no device {name:?} is paired");
            let hint = "`rlxd gate devices` lists the paired devices.";
            return out::fail(codes::UNKNOWN_DEVICE, &message, Some(hint));
        }
        Err(gate::UnpairError::Failed(message)) => {
            return out::fail(codes::INTERNAL, &message, None);
        }
    };
    if home.logs().is_dir() && log::open(&home.log_file(), false).is_ok() {
        let which = device.map_or_else(
            || format!("every device ({})", names.join(", ")),
            |name| format!("device {name:?}"),
        );
        warning!("gate unpaired: {which}, by the user, with rlxd stopped");
    }
    let mut data = Data::new();
    data.insert("unpaired".to_owned(), json!(names));
    data.insert("dropped".to_owned(), json!(0));
    out::succeed(data)
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
         remote {}, cli {}\ngate: {}\nclaude: {claude}",
        status.version,
        status.pid,
        status.platform,
        status.paths.home,
        status.paths.socket,
        c.app,
        c.tui,
        c.remote,
        c.cli,
        status.gate
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
    read_secret(&home.token(), "token")
}

/// The file `path`, trimmed: a token or a credential, which `what` names in the error.
fn read_secret(path: &Path, what: &str) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map(|text| text.trim().to_owned())
        .map_err(|e| format!("cannot read the {what} in {}: {e}", path.display()))
}

/// The gate credential kept in `file`, if there is one: `--as-app`'s, from its first call.
fn read_credential(file: &Path) -> Option<String> {
    let credential = read_secret(file, "gate credential").ok()?;
    (!credential.is_empty()).then_some(credential)
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
                AskError::NotListening(format!("no rlxd listens on {}", socket.display()))
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

/// [`call_error`] for `hello`'s request, but a connection through `--socket` that closed
/// without a whole reply to it is [`AskError::ForwardClosed`]: through an SSH forward, ssh
/// takes every connection, and closes it once sshd fails to reach the host's socket, as
/// when no rlxd runs there. Only for `hello`: after its reply, rlxd has answered on this
/// connection, so a later close is a plain `disconnected`.
fn socket_call_error(error: CallError, socket: Option<&Path>) -> AskError {
    let closed = match &error {
        CallError::Closed => true,
        CallError::Io(error) => matches!(
            error.kind(),
            io::ErrorKind::ConnectionReset
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::BrokenPipe
                | io::ErrorKind::UnexpectedEof
        ),
        _ => false,
    };
    match socket {
        Some(socket) if closed => AskError::ForwardClosed(format!(
            "the connection through {} closed without a whole reply to hello",
            socket.display()
        )),
        _ => call_error(error),
    }
}

/// The hint for a connection through `--socket` that closed before rlxd replied.
const FORWARD_CLOSED_HINT: &str = "Through an SSH forward, this most likely means the \
                                   host's rlxd isn't running (start it there with `rlxd here \
                                   --start`, over ssh), or the host's sshd doesn't allow socket \
                                   forwarding (AllowStreamLocalForwarding and \
                                   AllowTcpForwarding must allow local).";

/// The hint for nothing listening on `--socket`: not this home's rlxd to start.
const NOT_LISTENING_HINT: &str = "Check that the forward to it is up (ssh -L), and run \
                                  `rlxd here --start` on the host it leads to.";

/// Prints the envelope for `error`, with its exit code.
fn report(error: AskError) -> ExitCode {
    match error {
        AskError::NotRunning(message) => out::fail_with(
            out::NOT_RUNNING,
            codes::NOT_RUNNING,
            &message,
            Some("Start it with `rlxd serve --detach`."),
        ),
        AskError::NotListening(message) => out::fail_with(
            out::NOT_RUNNING,
            codes::NOT_RUNNING,
            &message,
            Some(NOT_LISTENING_HINT),
        ),
        AskError::Refused(reply) => out::print_reply(&reply),
        AskError::ForwardClosed(message) => {
            out::fail(codes::DISCONNECTED, &message, Some(FORWARD_CLOSED_HINT))
        }
        AskError::Unauthorized(message) => out::fail(codes::UNAUTHORIZED, &message, None),
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
        AskError::NotListening(message) => out::fail_on_stderr(
            out::NOT_RUNNING,
            codes::NOT_RUNNING,
            &message,
            Some(NOT_LISTENING_HINT),
        ),
        AskError::Refused(reply) => out::print_reply_on_stderr(&reply),
        AskError::ForwardClosed(message) => out::fail_on_stderr(
            out::FAILED,
            codes::DISCONNECTED,
            &message,
            Some(FORWARD_CLOSED_HINT),
        ),
        AskError::Unauthorized(message) => {
            out::fail_on_stderr(out::FAILED, codes::UNAUTHORIZED, &message, None)
        }
        AskError::Failed(message) => {
            out::fail_on_stderr(out::FAILED, codes::DISCONNECTED, &message, None)
        }
    }
}

/// `error`, on stderr, for a person, with its exit code.
fn report_to_a_person(error: AskError) -> ExitCode {
    let (message, status) = match error {
        AskError::NotRunning(message) | AskError::NotListening(message) => {
            (message, out::NOT_RUNNING)
        }
        AskError::Refused(reply) => {
            let message = reply.error().map_or_else(
                || reply.line.clone(),
                |e| format!("{}: {}", e.code, e.message),
            );
            (message, out::FAILED)
        }
        AskError::Failed(message)
        | AskError::ForwardClosed(message)
        | AskError::Unauthorized(message) => (message, out::FAILED),
    };
    eprintln!("rlxd: {message}");
    ExitCode::from(status)
}
