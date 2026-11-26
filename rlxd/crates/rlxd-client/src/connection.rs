//! One connection to rlxd: open it, say `hello`, send requests and read their replies.
//!
//! The wire is newline-delimited JSON (handoff 0.10.3, 7.2): each frame is one line. A
//! connection is blocking and does one thing at a time: a request, then its reply.
//!
//! `events.subscribe` (7.3) turns a connection into an event stream ([`Subscription`]):
//! event frames, then the subscribe's reply, which ends the stream. A frame with `ok` is
//! a reply, and any other is an event.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;

use rlxd_proto::{
    Data, Envelope, ErrorBody, Event, HelloParams, Outcome, Params, Request, RequestId,
    SubscribeParams, methods,
};

use crate::Home;

#[cfg(unix)]
type Stream = std::os::unix::net::UnixStream;
#[cfg(windows)]
type Stream = std::fs::File;

/// The longest line a connection reads: 16 MiB.
pub const MAX_LINE: usize = 16 << 20;

/// An open connection to an rlxd.
pub struct Connection {
    reader: BufReader<Stream>,
    writer: Stream,
    next_id: u32,
}

/// A connection could not be opened.
#[derive(Debug)]
pub enum ConnectError {
    /// No rlxd serves this home: there is no socket or pipe, or nothing listens on it.
    NotRunning {
        /// Where the connection was tried.
        endpoint: String,
        /// What the OS said.
        source: io::Error,
    },
    /// The socket or pipe is there but could not be opened.
    Io {
        /// Where the connection was tried.
        endpoint: String,
        /// What the OS said.
        source: io::Error,
    },
    /// The socket or pipe is another user's, so it is not this user's rlxd. Nothing was
    /// sent to it. On Windows, a process of another user serves the pipe. On macOS and
    /// Linux, the short socket ([`Home::short_socket`]) or its folder in `/tmp` is
    /// another user's, or others may enter the folder.
    NotOurs {
        /// The socket or pipe.
        endpoint: String,
    },
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRunning { endpoint, source } => {
                write!(f, "rlxd is not running ({endpoint}: {source})")
            }
            Self::Io { endpoint, source } => write!(f, "cannot connect to {endpoint}: {source}"),
            Self::NotOurs { endpoint } if cfg!(windows) => write!(
                f,
                "{endpoint} is served by another user's process, not this user's rlxd"
            ),
            Self::NotOurs { endpoint } => write!(
                f,
                "{endpoint} is not this user's: another user owns it or its folder, or \
                 others may enter its folder"
            ),
        }
    }
}

impl std::error::Error for ConnectError {}

/// A request got no reply.
#[derive(Debug)]
pub enum CallError {
    /// Reading or writing failed.
    Io(io::Error),
    /// rlxd closed the connection before it replied.
    Closed,
    /// rlxd sent a line longer than [`MAX_LINE`].
    TooLong,
    /// rlxd sent a line that is not a valid envelope.
    Malformed {
        /// The line, as it came.
        line: String,
        /// Why it is not an envelope.
        error: serde_json::Error,
    },
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "the connection to rlxd failed: {error}"),
            Self::Closed => f.write_str("rlxd closed the connection before it replied"),
            Self::TooLong => write!(f, "rlxd sent a line longer than {MAX_LINE} bytes"),
            Self::Malformed { error, .. } => {
                write!(f, "rlxd sent a reply that is not valid: {error}")
            }
        }
    }
}

impl std::error::Error for CallError {}

impl From<io::Error> for CallError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// One reply: the line as it came, and the envelope it holds.
#[derive(Clone, Debug)]
pub struct Reply {
    /// The reply's line, without its newline.
    pub line: String,
    /// The line, read.
    pub envelope: Envelope,
}

impl Reply {
    /// Whether the reply is `ok: true`.
    pub fn is_ok(&self) -> bool {
        matches!(self.envelope.outcome, Outcome::Ok { .. })
    }

    /// The reply's `data`, when it is `ok: true`.
    pub fn data(&self) -> Option<&Data> {
        match &self.envelope.outcome {
            Outcome::Ok { data, .. } => Some(data),
            _ => None,
        }
    }

    /// The reply's `error`, when it is `ok: false`.
    pub fn error(&self) -> Option<&ErrorBody> {
        match &self.envelope.outcome {
            Outcome::Err { error, .. } => Some(error),
            _ => None,
        }
    }
}

/// One event as it came on a stream: its line, and the event it holds.
#[derive(Clone, Debug)]
pub struct EventLine {
    /// The event's line, without its newline.
    pub line: String,
    /// The line, read.
    pub event: Event,
}

/// One frame read from a connection that may carry a stream.
#[derive(Clone, Debug)]
pub enum Incoming {
    /// An event of the stream.
    Event(EventLine),
    /// A reply: to the request that opened the stream, which ends it, or to another.
    Reply(Reply),
}

/// What a [`Subscription`] reads next.
#[derive(Clone, Debug)]
pub enum Next {
    /// An event.
    Event(EventLine),
    /// The subscribe's reply, which ends the stream. `ok: false`, and before any event,
    /// when rlxd refused the subscribe; `{seq}`, the last seq the stream covered, when it
    /// ended after the replay (no `follow`) or because rlxd stopped.
    End(Reply),
}

/// An event stream on a connection, opened by [`Connection::subscribe`].
pub struct Subscription {
    connection: Connection,
    id: RequestId,
}

impl Subscription {
    /// The next event, or the reply that ends the stream. Read no further after that
    /// reply: the connection is free for requests again ([`Subscription::into_connection`]).
    pub fn next_frame(&mut self) -> Result<Next, CallError> {
        loop {
            match self.connection.read_incoming()? {
                Incoming::Event(event) => return Ok(Next::Event(event)),
                Incoming::Reply(reply) if reply.envelope.id.is_none_or(|id| id == self.id) => {
                    return Ok(Next::End(reply));
                }
                // A reply to some other request, which this connection did not send.
                Incoming::Reply(_) => {}
            }
        }
    }

    /// The connection, once the stream has ended.
    pub fn into_connection(self) -> Connection {
        self.connection
    }
}

impl Connection {
    /// A connection to the rlxd serving `home`, at [`Home::endpoint`]. Sends nothing: say
    /// [`Connection::hello`] first.
    ///
    /// A short socket ([`Home::uses_short_socket`]) sits in `/tmp`, so before it connects
    /// it checks that `/tmp` is root's and sticky, and that the socket and its folder are
    /// this user's and that no one else may enter the folder, else
    /// [`ConnectError::NotOurs`].
    ///
    /// It connects to the very path it checked, [`Home::socket`] as a path, never the
    /// display string [`Home::endpoint`] reports, so a home whose path isn't UTF-8 is
    /// reached too.
    pub fn open(home: &Home) -> Result<Self, ConnectError> {
        #[cfg(unix)]
        let endpoint = {
            let socket = home.socket();
            if socket.parent() == Some(Home::short_socket_dir().as_path()) {
                check_tmp(&socket)?;
                check_short_socket(&socket)?;
            }
            socket
        };
        #[cfg(windows)]
        let endpoint = std::path::PathBuf::from(home.pipe_name());
        Self::from_endpoint(&endpoint)
    }

    /// A connection to the rlxd at `endpoint`: a socket's path on macOS and Linux, a named
    /// pipe's name on Windows. Sends nothing: say [`Connection::hello`] first.
    ///
    /// Over SSH, `endpoint` is the local end of a direct-streamlocal forward to the host's
    /// socket (`ssh -N -L <endpoint>:<socket> host`), and the token is the host's
    /// (`rlxd here --show-token`, or its `auth.token_file` read over the session), never
    /// this home's. On macOS and Linux the endpoint must be a socket, not a link to one,
    /// that this user owns, as ssh makes the forward's local end, and the process
    /// listening on it must run as this user: else [`ConnectError::NotOurs`], before
    /// anything is sent. Keep it in a folder only this user may enter, so no one else can
    /// bind its path first. On Windows, the pipe's server must run as this user, as for
    /// [`Connection::open`].
    pub fn open_at(endpoint: impl AsRef<Path>) -> Result<Self, ConnectError> {
        let endpoint = endpoint.as_ref();
        #[cfg(unix)]
        check_own_socket(endpoint)?;
        Self::from_endpoint(endpoint)
    }

    /// Connects to `endpoint`. On macOS and Linux, the process listening there must run
    /// as this user, read from the connected socket itself (`getpeereid`, `SO_PEERCRED`),
    /// so no swap of the path between a check and the connect gets the token. Root, which
    /// may reach any user's rlxd, needs the socket's owner instead: whoever rlxd runs as.
    fn from_endpoint(endpoint: &Path) -> Result<Self, ConnectError> {
        let stream = open_stream(endpoint)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsFd;
            use std::os::unix::fs::MetadataExt;
            let shown = || endpoint.display().to_string();
            let io = |source| ConnectError::Io {
                endpoint: shown(),
                source,
            };
            let peer = crate::unix::peer_uid(stream.as_fd()).map_err(io)?;
            let me = crate::unix::uid();
            let expected = if me == 0 {
                std::fs::symlink_metadata(endpoint).map_err(io)?.uid()
            } else {
                me
            };
            if peer != expected {
                return Err(ConnectError::NotOurs { endpoint: shown() });
            }
        }
        let writer = stream.try_clone().map_err(|source| ConnectError::Io {
            endpoint: endpoint.display().to_string(),
            source,
        })?;
        Ok(Self {
            reader: BufReader::new(stream),
            writer,
            next_id: 1,
        })
    }

    /// Sends `line`, which must hold one frame and no newline, as a frame.
    pub fn send_line(&mut self, line: &str) -> io::Result<()> {
        let mut frame = Vec::with_capacity(line.len() + 1);
        frame.extend_from_slice(line.as_bytes());
        frame.push(b'\n');
        self.writer.write_all(&frame)?;
        self.writer.flush()
    }

    /// The next line rlxd sends, without its newline, or `None` once it has closed the
    /// connection.
    pub fn read_line(&mut self) -> Result<Option<String>, CallError> {
        let mut line = Vec::new();
        let limit = u64::try_from(MAX_LINE).unwrap_or(u64::MAX) + 1;
        (&mut self.reader)
            .take(limit)
            .read_until(b'\n', &mut line)?;
        if line.is_empty() {
            return Ok(None);
        }
        if line.last() == Some(&b'\n') {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
        } else if line.len() > MAX_LINE {
            return Err(CallError::TooLong);
        }
        String::from_utf8(line)
            .map(Some)
            .map_err(|e| CallError::Io(io::Error::new(io::ErrorKind::InvalidData, e)))
    }

    /// The next reply rlxd sends.
    pub fn read_reply(&mut self) -> Result<Reply, CallError> {
        let line = self.read_line()?.ok_or(CallError::Closed)?;
        match serde_json::from_str(&line) {
            Ok(envelope) => Ok(Reply { line, envelope }),
            Err(error) => Err(CallError::Malformed { line, error }),
        }
    }

    /// The next frame rlxd sends, a reply or an event: one with `ok` is a reply.
    pub fn read_incoming(&mut self) -> Result<Incoming, CallError> {
        let line = self.read_line()?.ok_or(CallError::Closed)?;
        let value: serde_json::Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => return Err(CallError::Malformed { line, error }),
        };
        if value.get("ok").is_some() {
            return match serde_json::from_value::<Envelope>(value) {
                Ok(envelope) => Ok(Incoming::Reply(Reply { line, envelope })),
                Err(error) => Err(CallError::Malformed { line, error }),
            };
        }
        match serde_json::from_value::<Event>(value) {
            Ok(event) => Ok(Incoming::Event(EventLine { line, event })),
            Err(error) => Err(CallError::Malformed { line, error }),
        }
    }

    /// Sends `hello` and reads its reply. A refused hello's reply is `ok: false`, and rlxd
    /// then closes the connection.
    pub fn hello(&mut self, hello: &HelloParams) -> Result<Reply, CallError> {
        let params = match serde_json::to_value(hello) {
            Ok(serde_json::Value::Object(params)) => params,
            Ok(_) => unreachable!("a hello prints as an object"),
            Err(error) => {
                return Err(CallError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    error,
                )));
            }
        };
        self.request(methods::HELLO, params)
    }

    /// Sends a request for `method` with `params`, and reads its reply.
    pub fn request(&mut self, method: &str, params: Params) -> Result<Reply, CallError> {
        self.send(method, params)?;
        self.read_reply()
    }

    /// Sends a request for `method` with `params` and reads nothing, returning its id.
    /// Its reply, and a streaming method's events before it, come through
    /// [`Connection::read_incoming`].
    pub fn send(&mut self, method: &str, params: Params) -> Result<RequestId, CallError> {
        let id = RequestId::from(self.next_id);
        self.next_id += 1;
        let request = Request::new(id, method, params)
            .map_err(|e| CallError::Io(io::Error::new(io::ErrorKind::InvalidInput, e)))?;
        let line = serde_json::to_string(&request)
            .map_err(|e| CallError::Io(io::Error::new(io::ErrorKind::InvalidInput, e)))?;
        self.send_line(&line)?;
        Ok(id)
    }

    /// Opens the event stream (7.3) after `hello`: sends `events.subscribe` with `params`.
    /// Read the stream with [`Subscription::next_frame`]; a refusal is its first frame.
    pub fn subscribe(mut self, params: &SubscribeParams) -> Result<Subscription, CallError> {
        let params = match serde_json::to_value(params) {
            Ok(serde_json::Value::Object(params)) => params,
            Ok(_) => unreachable!("subscribe params print as an object"),
            Err(error) => {
                return Err(CallError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    error,
                )));
            }
        };
        let id = self.send(methods::EVENTS_SUBSCRIBE, params)?;
        Ok(Subscription {
            connection: self,
            id,
        })
    }
}

#[cfg(unix)]
fn open_stream(path: &Path) -> Result<Stream, ConnectError> {
    Stream::connect(path).map_err(|source| {
        let endpoint = path.display().to_string();
        match source.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
                ConnectError::NotRunning { endpoint, source }
            }
            _ => ConnectError::Io { endpoint, source },
        }
    })
}

/// Checks that `socket`, the endpoint a caller named, is a socket that this user owns,
/// read without following a link. A missing one is [`ConnectError::NotRunning`].
#[cfg(unix)]
fn check_own_socket(socket: &Path) -> Result<(), ConnectError> {
    use std::os::unix::fs::FileTypeExt;

    let endpoint = socket.display().to_string();
    let found = match std::fs::symlink_metadata(socket) {
        Ok(found) => found,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(ConnectError::NotRunning { endpoint, source });
        }
        Err(source) => return Err(ConnectError::Io { endpoint, source }),
    };
    if !found.file_type().is_socket() {
        let source = io::Error::other("it is not a socket");
        return Err(ConnectError::Io { endpoint, source });
    }
    if !crate::unix::is_owned(&found) {
        return Err(ConnectError::NotOurs { endpoint });
    }
    Ok(())
}

/// Checks that the folder above the short socket's folder, `/tmp`, is root's and sticky,
/// so the checks of [`check_short_socket`] still hold when it connects by path.
#[cfg(unix)]
fn check_tmp(socket: &Path) -> Result<(), ConnectError> {
    let tmp = socket
        .parent()
        .and_then(Path::parent)
        .unwrap_or(Path::new("/tmp"));
    crate::unix::check_sticky(tmp).map_err(|source| ConnectError::Io {
        endpoint: socket.display().to_string(),
        source,
    })
}

/// Checks that the short socket `socket` and its folder are this user's, and that no one
/// else may enter the folder, before the token goes to it. Neither is followed through a
/// link. A missing folder or socket is [`ConnectError::NotRunning`].
#[cfg(unix)]
fn check_short_socket(socket: &Path) -> Result<(), ConnectError> {
    use crate::unix::{is_owned, is_private_dir};

    let endpoint = || socket.display().to_string();
    let read = |path: &Path| {
        std::fs::symlink_metadata(path).map_err(|source| match source.kind() {
            io::ErrorKind::NotFound => ConnectError::NotRunning {
                endpoint: endpoint(),
                source,
            },
            _ => ConnectError::Io {
                endpoint: endpoint(),
                source,
            },
        })
    };
    let dir = socket.parent().unwrap_or(Path::new("/"));
    if !is_private_dir(&read(dir)?) || !is_owned(&read(socket)?) {
        return Err(ConnectError::NotOurs {
            endpoint: endpoint(),
        });
    }
    Ok(())
}

#[cfg(windows)]
fn open_stream(path: &Path) -> Result<Stream, ConnectError> {
    use std::time::{Duration, Instant};

    /// Every instance of the pipe is busy: wait for one to come free.
    const ERROR_PIPE_BUSY: i32 = 231;
    let endpoint = path.display().to_string();
    let deadline = Instant::now() + Duration::from_secs(5);
    let pipe = loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
        {
            Ok(pipe) => break pipe,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Err(ConnectError::NotRunning { endpoint, source });
            }
            Err(source)
                if source.raw_os_error() == Some(ERROR_PIPE_BUSY) && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(source) => return Err(ConnectError::Io { endpoint, source }),
        }
    };
    // Anyone can create a pipe of this name while no rlxd serves it, so before the token
    // goes over it, check that this user's process serves it.
    let ours = crate::windows::Sid::current_user()
        .and_then(|me| crate::windows::pipe_server_user(&pipe).map(|server| server == me));
    match ours {
        Ok(true) => Ok(pipe),
        Ok(false) => Err(ConnectError::NotOurs { endpoint }),
        Err(source) => Err(ConnectError::Io { endpoint, source }),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;

    use super::*;

    /// A fresh folder, `mode`, with a socket listening in it.
    fn socket_in(name: &str, mode: u32) -> (PathBuf, UnixListener) {
        let dir = std::env::temp_dir().join(format!("rlxd-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(mode)).unwrap();
        let socket = dir.join("s.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        (socket, listener)
    }

    #[test]
    fn a_short_socket_in_a_private_folder_of_ours_passes() {
        let (socket, _listener) = socket_in("private", 0o700);
        assert!(check_short_socket(&socket).is_ok());
        fs::remove_dir_all(socket.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_short_socket_in_a_folder_others_may_enter_is_refused() {
        for mode in [0o755, 0o710, 0o701, 0o777] {
            let (socket, _listener) = socket_in(&format!("open-{mode:o}"), mode);
            let refused = check_short_socket(&socket);
            assert!(
                matches!(refused, Err(ConnectError::NotOurs { .. })),
                "{mode:o}: {refused:?}"
            );
            fs::remove_dir_all(socket.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn a_short_socket_behind_a_link_is_refused() {
        let (socket, _listener) = socket_in("linked", 0o700);
        let dir = socket.parent().unwrap();
        let link = dir.with_extension("link");
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(dir, &link).unwrap();
        let refused = check_short_socket(&link.join("s.sock"));
        assert!(
            matches!(refused, Err(ConnectError::NotOurs { .. })),
            "{refused:?}"
        );
        fs::remove_file(&link).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_short_socket_that_is_not_there_is_not_running() {
        let (socket, listener) = socket_in("gone", 0o700);
        drop(listener);
        fs::remove_file(&socket).unwrap();
        let missing = check_short_socket(&socket);
        assert!(
            matches!(missing, Err(ConnectError::NotRunning { .. })),
            "{missing:?}"
        );
        let dir = socket.parent().unwrap().to_path_buf();
        fs::remove_dir(&dir).unwrap();
        let missing = check_short_socket(&socket);
        assert!(
            matches!(missing, Err(ConnectError::NotRunning { .. })),
            "{missing:?}"
        );
    }

    #[test]
    fn open_at_refuses_a_file_or_a_link_where_a_socket_should_be() {
        let (socket, _listener) = socket_in("open-at-not", 0o700);
        let dir = socket.parent().unwrap();
        let file = dir.join("file");
        fs::write(&file, "").unwrap();
        let link = dir.join("link.sock");
        std::os::unix::fs::symlink(&socket, &link).unwrap();
        for path in [&file, &link] {
            let refused = Connection::open_at(path);
            assert!(
                matches!(&refused, Err(ConnectError::Io { source, .. }) if source.to_string() == "it is not a socket"),
                "{}: {:?}",
                path.display(),
                refused.err()
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn open_at_connects_to_the_socket_it_is_given() {
        let (socket, listener) = socket_in("open-at", 0o700);
        let connection = Connection::open_at(&socket);
        assert!(connection.is_ok());
        assert!(listener.accept().is_ok());
        fs::remove_dir_all(socket.parent().unwrap()).unwrap();
        let gone = Connection::open_at(&socket);
        assert!(matches!(gone, Err(ConnectError::NotRunning { .. })));
    }
}
