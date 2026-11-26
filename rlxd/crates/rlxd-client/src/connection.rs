//! One connection to rlxd: open it, say `hello`, send requests and read their replies.
//!
//! The wire is newline-delimited JSON (handoff 0.10.3, 7.2): each frame is one line. A
//! connection is blocking and does one thing at a time: a request, then its reply.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};

use rlxd_proto::{
    Data, Envelope, ErrorBody, HelloParams, Outcome, Params, Request, RequestId, methods,
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
    /// Windows: the pipe is served by a process of another user, so it is not this
    /// user's rlxd. Nothing was sent to it.
    NotOurs {
        /// The pipe.
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
            Self::NotOurs { endpoint } => write!(
                f,
                "{endpoint} is served by another user's process, not this user's rlxd"
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

impl Connection {
    /// A connection to the rlxd serving `home`. Sends nothing: say [`Connection::hello`]
    /// first.
    pub fn open(home: &Home) -> Result<Self, ConnectError> {
        let stream = open_stream(home)?;
        let writer = stream.try_clone().map_err(|source| ConnectError::Io {
            endpoint: home.endpoint(),
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
        let id = RequestId::from(self.next_id);
        self.next_id += 1;
        let request = Request::new(id, method, params)
            .map_err(|e| CallError::Io(io::Error::new(io::ErrorKind::InvalidInput, e)))?;
        let line = serde_json::to_string(&request)
            .map_err(|e| CallError::Io(io::Error::new(io::ErrorKind::InvalidInput, e)))?;
        self.send_line(&line)?;
        self.read_reply()
    }
}

#[cfg(unix)]
fn open_stream(home: &Home) -> Result<Stream, ConnectError> {
    let path = home.socket();
    Stream::connect(&path).map_err(|source| {
        let endpoint = path.display().to_string();
        match source.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
                ConnectError::NotRunning { endpoint, source }
            }
            _ => ConnectError::Io { endpoint, source },
        }
    })
}

#[cfg(windows)]
fn open_stream(home: &Home) -> Result<Stream, ConnectError> {
    use std::time::{Duration, Instant};

    /// Every instance of the pipe is busy: wait for one to come free.
    const ERROR_PIPE_BUSY: i32 = 231;
    let endpoint = home.pipe_name();
    let deadline = Instant::now() + Duration::from_secs(5);
    let pipe = loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&endpoint)
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
