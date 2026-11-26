//! `rlxd events` (handoff 0.10.3, "How to read the checks" and 7.3): the home's event
//! stream as JSON lines on stdout, one event envelope each.
//!
//! `rlxd events [--as-app] [--topics a,b] [--since SEQ] [--follow] [--until TYPE[,TYPE…]]
//! [--limit N] [--timeout S] [--socket PATH] [--token T]`
//!
//! - **Where.** The home's rlxd with its token file's token, or with `--socket` and
//!   `--token` another socket and token, such as an SSH forward's local end and the host's
//!   token, as `rlxd call` takes them.
//! - **What it prints.** With `--since`, every event rlxd holds after that seq first, with
//!   a `daemon.gap` before them when rlxd holds none so old. With `--follow`, then every
//!   later event as it happens, until rlxd stops; a reader too slow to keep up gets a
//!   `daemon.gap` there too. `--until` implies `--follow`.
//! - **Exit codes.** 0 once an event of one of `--until`'s types has printed, once
//!   `--limit` events have, or when the stream ends and there is no `--until`. 1 when
//!   rlxd refuses (an unknown topic, `not_gate` for `--as-app`), or the stream ends before
//!   an `--until` type. 2 on a usage error, 3 when rlxd is not running, and 4 when
//!   `--timeout` runs out first.
//! - **stdout carries events only,** so `jq -s .` of it is an `event-list.json`. A refusal
//!   or a failure goes to stderr, as its envelope on one line.
//!
//! The connection, its `hello` and the stream are read on a thread of their own, so the
//! timeout holds from the start and on every OS, named pipes included.

use std::process::ExitCode;
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use rlxd_client::{EventLine, Home, Incoming, Reply};
use rlxd_proto::{Data, codes, methods};
use serde_json::Value;

use crate::cli::{self, AskError};
use crate::out;

/// How many events the reader thread may read ahead of stdout. Beyond that it stops
/// reading, and rlxd holds the rest for it (`crate::bus`).
const READ_AHEAD: usize = 256;

/// `rlxd events`' options, as the command line gave them.
#[derive(Debug, Default)]
pub struct EventsArgs {
    /// Say `hello {role: "app"}`, with the test gate credential when there is one.
    pub as_app: bool,
    /// The topics, comma-separated.
    pub topics: Option<String>,
    /// Replay the events after this seq first.
    pub since: Option<u64>,
    /// Go on with live events.
    pub follow: bool,
    /// The types to stop at, comma-separated.
    pub until: Option<String>,
    /// How many events to print at most.
    pub limit: Option<u64>,
    /// How many seconds to run at most.
    pub timeout: Option<f64>,
    /// Where to connect and the token to send, when not the home's.
    pub target: cli::Target,
}

/// What the reader thread passes on.
enum Message {
    Event(EventLine),
    End(Reply),
    Failed(AskError),
}

/// `rlxd events`.
pub fn events(args: EventsArgs) -> ExitCode {
    let topics = match list(args.topics.as_deref(), "--topics") {
        Ok(topics) => topics,
        Err(message) => return out::usage(&message),
    };
    let until = match list(args.until.as_deref(), "--until") {
        Ok(until) => until.unwrap_or_default(),
        Err(message) => return out::usage(&message),
    };
    let timeout = match args.timeout {
        None => None,
        Some(seconds) if seconds.is_finite() && seconds > 0.0 => {
            Some(Duration::from_secs_f64(seconds))
        }
        Some(seconds) => {
            return out::usage(&format!(
                "--timeout {seconds} is not a number of seconds above 0"
            ));
        }
    };
    let home = match Home::from_env() {
        Ok(home) => home,
        Err(e) => return cli::report_on_stderr(AskError::NotRunning(e.to_string())),
    };
    let mut params = Data::new();
    if let Some(topics) = topics {
        let topics = topics.into_iter().map(Value::String).collect();
        params.insert("topics".to_owned(), Value::Array(topics));
    }
    if let Some(since) = args.since {
        params.insert("since".to_owned(), Value::from(since));
    }
    let follow = args.follow || !until.is_empty();
    params.insert("follow".to_owned(), Value::Bool(follow));

    let deadline = timeout.map(|timeout| Instant::now() + timeout);
    let (to, from) = mpsc::sync_channel(READ_AHEAD);
    let as_app = args.as_app;
    let target = args.target;
    thread::spawn(move || read(&home, as_app, &target, params, &to));
    let mut printed = 0;
    loop {
        let message = match deadline {
            None => from.recv().map_err(|_| RecvTimeoutError::Disconnected),
            Some(deadline) => from.recv_timeout(deadline.saturating_duration_since(Instant::now())),
        };
        match message {
            Ok(Message::Event(event)) => {
                if !out::print_line(&event.line) {
                    return ExitCode::from(out::FAILED);
                }
                printed += 1;
                if until.contains(&event.event.r#type) || args.limit == Some(printed) {
                    return ExitCode::SUCCESS;
                }
            }
            Ok(Message::End(reply)) if reply.is_ok() => {
                if until.is_empty() {
                    return ExitCode::SUCCESS;
                }
                let message = format!("the stream ended before a {} event", until.join(" or "));
                return out::fail_on_stderr(out::FAILED, codes::STREAM_ENDED, &message, None);
            }
            Ok(Message::End(reply)) => return out::print_reply_on_stderr(&reply),
            Ok(Message::Failed(error)) => return cli::report_on_stderr(error),
            Err(RecvTimeoutError::Timeout) => {
                let seconds = timeout.unwrap_or_default().as_secs_f64();
                let message = if until.is_empty() {
                    format!("{seconds} s ran out")
                } else {
                    format!("no {} event came within {seconds} s", until.join(" or "))
                };
                return out::fail_on_stderr(out::TIMED_OUT, codes::TIMED_OUT, &message, None);
            }
            Err(RecvTimeoutError::Disconnected) => {
                let message = "the stream's reader stopped";
                return out::fail_on_stderr(out::FAILED, codes::INTERNAL, message, None);
            }
        }
    }
}

/// Connects, says `hello`, subscribes with `params`, and passes on the stream until its
/// reply, or a failure.
fn read(home: &Home, as_app: bool, target: &cli::Target, params: Data, to: &SyncSender<Message>) {
    let mut connection = match cli::open(home, as_app, target) {
        Ok(connection) => connection,
        Err(error) => {
            let _ = to.send(Message::Failed(error));
            return;
        }
    };
    let id = match connection.send(methods::EVENTS_SUBSCRIBE, params) {
        Ok(id) => id,
        Err(e) => {
            let _ = to.send(Message::Failed(AskError::Failed(e.to_string())));
            return;
        }
    };
    loop {
        let message = match connection.read_incoming() {
            Ok(Incoming::Event(event)) => Message::Event(event),
            Ok(Incoming::Reply(reply)) if reply.envelope.id.is_none_or(|i| i == id) => {
                Message::End(reply)
            }
            Ok(Incoming::Reply(_)) => continue,
            Err(e) => Message::Failed(AskError::Failed(e.to_string())),
        };
        let last = !matches!(message, Message::Event(_));
        if to.send(message).is_err() || last {
            return;
        }
    }
}

/// A comma-separated list, or `None` when not given. Refuses an empty item.
fn list(text: Option<&str>, what: &str) -> Result<Option<Vec<String>>, String> {
    let Some(text) = text else {
        return Ok(None);
    };
    let items: Vec<String> = text.split(',').map(|item| item.trim().to_owned()).collect();
    if items.iter().any(String::is_empty) {
        return Err(format!("{what} {text:?} has an empty item"));
    }
    Ok(Some(items))
}
