//! The event bus (handoff 0.10.3, 1.7 and 7.3): one numbered stream of events per home,
//! which every client can subscribe to.
//!
//! # Two sequences
//!
//! - **`seq`** numbers every event of the home, from 1, across every topic and across
//!   restarts: one rlxd's events come after the last one's. rlxd keeps a mark in
//!   `<home>/rlxd/seq`, at or above every seq it has given out, and gives out no seq the
//!   mark on disk does not cover:
//!   - **Ahead, off the lock.** Under the bus lock, a publisher only checks the mark; the
//!     mark is written ([`RESERVE`] seqs ahead) by the publisher that finds fewer than
//!     half of them left, after it has let the lock go. Publishing goes on meanwhile. A
//!     publisher waits only when the mark is used up, for the write in flight or its own.
//!   - **Durably.** A write goes to a sibling, is fsynced, then renamed over the mark, and
//!     the rename is made durable too (the folder fsynced on POSIX, `MOVEFILE_WRITE_THROUGH`
//!     on Windows). Only then does the bus count the new mark.
//!   - **When a write fails**, the mark stays where it was, the error is logged, and the
//!     write is tried again after [`RETRY_AFTER`]. Once the mark is used up, a publish is
//!     refused ([`Refused::Unnumbered`]) rather than give out a seq a crash could repeat.
//!   - **A clean stop** writes the last seq given out.
//!
//!   A new rlxd starts after the mark and holds nothing at or below it. So a client that
//!   resumes from an older seq gets a `daemon.gap` up to the mark: never an event twice,
//!   and never a silent hole. After a crash, that gap also covers the seqs that were
//!   reserved and never given out.
//!
//!   A mark rlxd cannot read stops it from starting, naming the file, rather than number
//!   from 1 again. Deleting the file does number from 1 again, and a client still holding
//!   a seq from before gets `since_ahead` until the new numbering passes it, then may miss
//!   events. A numbering epoch, an id made with the mark and carried in `status` and
//!   `daemon.started`, would let a client tell; it is a follow-up.
//! - **`run_seq`** numbers one run's events, from 1 ([`Bus::publish_run`]). The bus
//!   indexes the events it holds by run, so one run can be replayed from a `run_seq`, with
//!   the home seq a live follower then resumes after ([`Bus::run_events`]). Runs outlive the
//!   buffer: the runner (5.4) keeps each run's events on disk as well, tells the bus where
//!   a resumed run's numbering stands ([`Bus::seed_run`]), and says when a run has ended
//!   ([`Bus::end_run`]), so that its entry goes once the buffer holds none of its events.
//!
//! # The buffer
//!
//! The bus holds at most [`Limits::events`] events and [`Limits::bytes`] bytes of them,
//! none older than [`Limits::age`]: by default the last 10,000, at most 64 MiB, none
//! older than 24 hours ([`Limits::DEFAULT`]), each as the line it is sent as. That is what
//! `events.subscribe {since}` replays. One event's line may be at most [`MAX_EVENT`]
//! bytes, the longest line a client reads.
//!
//! **PTY output is kept apart** (handoff 1.4): `pty.output` events, which a busy terminal
//! publishes by the thousand, go to a partition of their own, with its own caps
//! ([`Limits::OUTPUT`]), so they never push a run, job or other topic's event out. Seqs
//! stay the home's, one numbering across both. A subscriber reads both in seq order, and
//! gets a `daemon.gap` for the output partition only when it asked for the `pty` topic.
//! `pty.exited` stays with the other events.
//!
//! # Subscribers
//!
//! A subscriber is a place in the buffer ([`Cursor`]), not a queue of its own. Publishing
//! appends an event and wakes the subscribers, and never waits for one. Each
//! subscriber's connection takes out what comes after its place, at most [`BATCH`] lines
//! and [`BATCH_BYTES`] bytes at a time (one line at least), and writes them at its
//! client's pace. So a client that stops reading holds up no one, and holds at most one
//! batch.
//!
//! When the buffer drops events that a subscriber has not read yet, the subscriber gets a
//! `daemon.gap {from, to}` for them, then the events after them: what a client
//! reconnecting from its last seq would get, without the reconnect. So a gap can come
//! anywhere in a stream, not only first, and whatever the subscriber's topics. A gap's own
//! `seq` is its `to`: the stream's seqs still strictly increase, and resuming from the gap
//! skips nothing it did not report.
//!
//! A cursor that does not follow ends at the last event published when it was made, so a
//! replay ends however fast events come.
//!
//! Publishing never waits on a subscriber, but it runs on one of rlxd's two workers: a
//! publisher that publishes many events in a loop lets other tasks run between batches
//! (`tokio::task::yield_now`), as `debug.emit` does, or followers starve meanwhile.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use rlxd_client::Home;
use rlxd_proto::{Data, Event, Invalid, RunScope, event_types, topics};
use serde_json::Value;
use tokio::sync::watch;

use crate::log::error;
use crate::{clock, durable};

/// What the buffer holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// At most this many events.
    pub events: usize,
    /// At most this many bytes of event lines; the newest event stays, whatever its size.
    pub bytes: usize,
    /// None older than this.
    pub age: Duration,
}

impl Limits {
    /// The last 10,000 events, at most 64 MiB of them, none older than 24 hours.
    pub const DEFAULT: Self = Self {
        events: 10_000,
        bytes: 64 << 20,
        age: Duration::from_secs(24 * 60 * 60),
    };

    /// The `pty.output` partition's: the last 20,000 events, at most 32 MiB of them, none
    /// older than 24 hours.
    pub const OUTPUT: Self = Self {
        events: 20_000,
        bytes: 32 << 20,
        age: Duration::from_secs(24 * 60 * 60),
    };
}

/// How many seqs ahead the mark is written.
pub const RESERVE: u64 = 1_000;

/// How long after a failed mark write the next is tried.
pub const RETRY_AFTER: Duration = Duration::from_secs(1);

/// How many lines a subscriber takes out of the buffer at a time.
pub const BATCH: usize = 256;

/// How many bytes of lines a subscriber takes out at a time, past its first line.
pub const BATCH_BYTES: usize = 1 << 20;

/// The longest line an event may have: the longest line a client reads.
pub const MAX_EVENT: usize = rlxd_client::MAX_LINE;

/// The mark's file, in `<home>/rlxd/`.
const MARK_FILE: &str = "seq";

const _: () = assert!(topics::ALL.len() < 32, "a topic set is a u32");

/// One event as it is sent: its JSON and a newline.
pub type Line = Arc<[u8]>;

/// A set of the topics the bus carries, [`topics::ALL`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Topics(u32);

impl Topics {
    /// Every topic.
    pub const ALL: Self = Self((1 << topics::ALL.len()) - 1);

    /// The topics `names`, or the first name the bus does not carry.
    pub fn of<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<Self, &'a str> {
        names
            .into_iter()
            .try_fold(0, |set, name| Ok(set | bit(name).ok_or(name)?))
            .map(Self)
    }

    fn has(self, bit: u32) -> bool {
        self.0 & bit != 0
    }
}

/// `topic`'s bit in a [`Topics`], if the bus carries it.
fn bit(topic: &str) -> Option<u32> {
    let at = topics::ALL.iter().position(|t| *t == topic)?;
    Some(1 << at)
}

/// Why an event was not published. A refused event takes no seq.
#[derive(Debug)]
pub enum Refused {
    /// The bus does not carry the topic, or it is `run`, whose events go through
    /// [`Bus::publish_run`].
    Topic(String),
    /// The event breaks a rule of `event.json`.
    Event(Invalid),
    /// The event's line is longer than [`MAX_EVENT`]: its length.
    TooLarge(usize),
    /// The mark is used up and could not be written, so no seq is free: why.
    Unnumbered(String),
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Topic(topic) if topic == topics::RUN => {
                f.write_str("a run event is published with publish_run, which numbers it")
            }
            Self::Topic(topic) => write!(f, "the bus carries no topic {topic:?}"),
            Self::Event(invalid) => write!(f, "the event is not valid: {invalid}"),
            Self::TooLarge(length) => write!(
                f,
                "the event is {length} bytes, more than a client reads ({MAX_EVENT})"
            ),
            Self::Unnumbered(why) => write!(f, "no seq is free: {why}"),
        }
    }
}

impl std::error::Error for Refused {}

/// The run a run event belongs to: [`Bus::publish_run`] gives it its `run_seq`.
#[derive(Clone, Copy, Debug)]
pub struct Run<'a> {
    /// The run's id.
    pub id: &'a str,
    /// The run's kind, such as `task`.
    pub kind: &'a str,
    /// Which attempt of the run, from 1.
    pub attempt: u64,
    /// The scheduler job that fired the run, if one did.
    pub job_id: Option<&'a str>,
}

/// What [`Bus::publish_run`] numbered an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Numbered {
    /// Its place in the home's stream.
    pub seq: u64,
    /// Its place in its run's.
    pub run_seq: u64,
}

/// One run's events from the buffer ([`Bus::run_events`]).
#[derive(Clone, Debug)]
pub struct RunReplay {
    /// The run's events from the `run_seq` asked for, oldest first.
    pub lines: Vec<Line>,
    /// The home's last seq when they were taken: every event of the run up to it is in
    /// `lines`, so a follower that resumes after it (`Bus::cursor` with this `since`) gets
    /// the run's later events, none twice.
    pub through: u64,
}

/// A subscriber's place in the stream: the last seq it was given, or passed over as not
/// one of its topics, and where it ends, if it does not follow.
#[derive(Clone, Debug)]
pub struct Cursor {
    after: u64,
    /// The same in the `pty.output` partition, which can be ahead of `after`: up to a
    /// gap of that partition's, which says what it dropped, before the main ring's events
    /// below the gap's end are read. Never behind `after`.
    output_after: u64,
    topics: Topics,
    until: Option<u64>,
}

impl Cursor {
    /// The last seq this subscriber was given or passed over: a `since` to resume from.
    pub fn after(&self) -> u64 {
        self.after
    }

    /// Whether it has reached its end: never, while it follows.
    pub fn is_done(&self) -> bool {
        self.until.is_some_and(|until| self.after >= until)
    }
}

/// The event bus of one home.
pub struct Bus {
    state: Mutex<State>,
    /// Signalled when a mark write ends.
    written: Condvar,
    head: watch::Sender<u64>,
    mark: Mark,
    limits: Limits,
    output_limits: Limits,
    #[cfg(test)]
    hooks: tests::Hooks,
}

struct State {
    /// The events held but `pty.output`, oldest first, in seq order.
    held: VecDeque<Held>,
    /// The bytes of their lines.
    bytes: usize,
    /// Every one of them at or below it is gone: dropped from the buffer, or an earlier
    /// rlxd's.
    floor: u64,
    /// The `pty.output` events held, oldest first, in seq order.
    output: VecDeque<Held>,
    /// The bytes of their lines.
    output_bytes: usize,
    /// Every `pty.output` event at or below it is gone.
    output_floor: u64,
    /// The last seq given out.
    head: u64,
    /// The mark on disk: seqs up to it may be given out.
    reserved: u64,
    /// A mark write is in flight.
    writing: bool,
    /// When the last mark write failed, and why, until one succeeds.
    failed: Option<(Instant, String)>,
    /// Each run's numbering, and where its events are in the buffer.
    runs: HashMap<Arc<str>, RunLog>,
}

struct Held {
    seq: u64,
    topic: u32,
    at: SystemTime,
    run: Option<Arc<str>>,
    line: Line,
}

#[derive(Default)]
struct RunLog {
    /// The last `run_seq` given out.
    last: u64,
    /// The run's events the buffer holds, oldest first: `(run_seq, seq)`.
    held: VecDeque<(u64, u64)>,
    /// The runner said it ended: its entry goes once `held` is empty.
    ended: bool,
}

impl Bus {
    /// The bus of `home`, numbering on from its mark, which it reserves ahead first.
    pub fn open(home: &Home) -> Result<Self, String> {
        Self::open_at(home.rlxd_dir().join(MARK_FILE), Limits::DEFAULT)
    }

    /// A bus whose mark is `path`, holding what `limits` allow, and `pty.output` what
    /// [`Limits::OUTPUT`] does.
    pub fn open_at(path: PathBuf, limits: Limits) -> Result<Self, String> {
        Self::open_with(path, limits, Limits::OUTPUT)
    }

    /// A bus whose mark is `path`, holding what `limits` allow, and `pty.output` what
    /// `output` does.
    pub fn open_with(path: PathBuf, limits: Limits, output: Limits) -> Result<Self, String> {
        let mark = Mark::new(path);
        let last = mark.read()?;
        if last > u64::MAX - RESERVE {
            return Err(format!(
                "{} holds {last}: so few seqs are left that rlxd will not start. Remove the \
                 file to number events from 1 again, and clients may miss events numbered \
                 before",
                mark.path.display()
            ));
        }
        let reserved = last + RESERVE;
        mark.write(reserved)
            .map_err(|e| format!("cannot write {}: {e}", mark.path.display()))?;
        Ok(Self {
            state: Mutex::new(State {
                held: VecDeque::new(),
                bytes: 0,
                floor: last,
                output: VecDeque::new(),
                output_bytes: 0,
                output_floor: last,
                head: last,
                reserved,
                writing: false,
                failed: None,
                runs: HashMap::new(),
            }),
            written: Condvar::new(),
            head: watch::Sender::new(last),
            mark,
            limits: Limits {
                events: limits.events.max(1),
                ..limits
            },
            output_limits: Limits {
                events: output.events.max(1),
                ..output
            },
            #[cfg(test)]
            hooks: tests::Hooks::default(),
        })
    }

    /// Writes the last seq given out as the mark, so the next rlxd numbers on from it.
    pub fn close(&self) {
        let mut state = self.state();
        while state.writing {
            state = self.wait(state);
        }
        let head = state.head;
        // From here, a publish waits for this write, then reserves again.
        state.reserved = head;
        state.writing = true;
        drop(state);
        let result = self.mark.write(head);
        self.state().writing = false;
        self.written.notify_all();
        if let Err(e) = result {
            // The mark on disk is still the reservation, at or above every seq given out.
            error!("cannot write {}: {e}", self.mark.path.display());
        }
    }

    /// Publishes an event on `topic`, which is not `run`, and returns its seq.
    pub fn publish(&self, topic: &str, r#type: &str, data: Data) -> Result<u64, Refused> {
        let bit = bit(topic)
            .filter(|_| topic != topics::RUN)
            .ok_or_else(|| Refused::Topic(topic.to_owned()))?;
        let mut state = self.lock_for_a_seq()?;
        let seq = state.head + 1;
        let event = Event::new(seq, clock::now(), topic, r#type, data).map_err(Refused::Event)?;
        let line = line(&event)?;
        let output = topic == topics::PTY && r#type == event_types::PTY_OUTPUT;
        let write = self.append_to(&mut state, seq, bit, None, line, output);
        drop(state);
        self.after_unlock(seq, write);
        Ok(seq)
    }

    /// Publishes an event of `run`, giving it the run's next `run_seq`.
    pub fn publish_run(&self, run: Run<'_>, r#type: &str, data: Data) -> Result<Numbered, Refused> {
        let bit = bit(topics::RUN).expect("the bus carries run events");
        let mut state = self.lock_for_a_seq()?;
        let seq = state.head + 1;
        let run_seq = state.runs.get(run.id).map_or(0, |log| log.last) + 1;
        let mut scope = RunScope::new(run.id, run_seq, run.kind, run.attempt);
        if let Some(job_id) = run.job_id {
            scope = scope.with_job(job_id);
        }
        let event =
            Event::for_run(seq, clock::now(), r#type, scope, data).map_err(Refused::Event)?;
        let line = line(&event)?;
        let id: Arc<str> = match state.runs.get_key_value(run.id) {
            Some((id, _)) => Arc::clone(id),
            None => Arc::from(run.id),
        };
        let log = state.runs.entry(Arc::clone(&id)).or_default();
        log.last = run_seq;
        log.held.push_back((run_seq, seq));
        let write = self.append_to(&mut state, seq, bit, Some(id), line, false);
        drop(state);
        self.after_unlock(seq, write);
        Ok(Numbered { seq, run_seq })
    }

    /// Says that run `id` has numbered its events up to `last_run_seq` already, as its
    /// log on disk says after a restart, so its next event gets the number after. Clears
    /// an earlier [`Bus::end_run`]: the run goes on.
    pub fn seed_run(&self, id: &str, last_run_seq: u64) {
        let mut state = self.state();
        let log = state.runs.entry(Arc::from(id)).or_default();
        log.last = log.last.max(last_run_seq);
        log.ended = false;
    }

    /// Says that run `id` has ended. Its entry, and its numbering, go once the buffer
    /// holds none of its events; a resumed run is seeded again ([`Bus::seed_run`]).
    pub fn end_run(&self, id: &str) {
        let mut state = self.state();
        let gone = match state.runs.get_mut(id) {
            Some(log) => {
                log.ended = true;
                log.held.is_empty()
            }
            None => false,
        };
        if gone {
            state.runs.remove(id);
        }
    }

    /// Run `id`'s events from `run_seq` `from` on, oldest first, when the buffer still
    /// holds every one of them: none when `from` is past the run's last. `None` when the
    /// buffer has dropped some of them, or has no entry for the run; its log on disk has
    /// them.
    pub fn run_events(&self, id: &str, from: u64) -> Option<RunReplay> {
        let state = self.state();
        let log = state.runs.get(id)?;
        let from = from.max(1);
        let through = state.head;
        if from > log.last {
            return Some(RunReplay {
                lines: Vec::new(),
                through,
            });
        }
        let &(first, _) = log.held.front()?;
        if first > from {
            return None;
        }
        let lines = log
            .held
            .iter()
            .filter(|(run_seq, _)| *run_seq >= from)
            .map(|(_, seq)| state.line(*seq))
            .collect();
        Some(RunReplay { lines, through })
    }

    /// The last seq given out: `status`'s `seq`, 0 before the first event of a new home.
    pub fn head(&self) -> u64 {
        self.state().head
    }

    /// Changes to [`Bus::head`], to wait for new events on. It never goes back, and it
    /// changes before a publish returns.
    pub fn watch(&self) -> watch::Receiver<u64> {
        self.head.subscribe()
    }

    /// A subscriber to `topics` after `since`, or after the last event when `since` is
    /// `None`. One that does not `follow` ends at the last event published now. A
    /// `since` past the last event is refused, with the last event's seq: it comes from
    /// another numbering.
    pub fn cursor(&self, topics: Topics, since: Option<u64>, follow: bool) -> Result<Cursor, u64> {
        let head = self.state().head;
        match since {
            Some(since) if since > head => Err(head),
            since => Ok(Cursor {
                after: since.unwrap_or(head),
                output_after: since.unwrap_or(head),
                topics,
                until: (!follow).then_some(head),
            }),
        }
    }

    /// The next lines for `cursor`, at most `lines` of them and, past the first, at most
    /// `bytes`, moving it past them: a `daemon.gap` first when the buffer has dropped
    /// events after it. None when it is up to date, or done.
    pub fn read(&self, cursor: &mut Cursor, lines: usize, bytes: usize) -> Vec<Line> {
        let mut state = self.state();
        state.drop_old(&self.limits, &self.output_limits);
        let end = cursor.until.unwrap_or(state.head).min(state.head);
        let mut batch = Vec::new();
        if cursor.after >= end {
            return batch;
        }
        let output = bit(topics::PTY).is_some_and(|pty| cursor.topics.has(pty));
        let mut taken = 0;
        // The main ring's loss first: what it dropped, of every topic, as a gap.
        if cursor.after < state.floor {
            let to = state.floor.min(end);
            let gap = gap(cursor.after + 1, to, false);
            taken += gap.len();
            batch.push(gap);
            cursor.after = to;
        }
        cursor.output_after = cursor.output_after.max(cursor.after);
        // The output partition's loss, for a cursor that asked for `pty`: a gap that
        // says it is `pty`'s only, in its place in seq order, so the main ring's events
        // below its end still come, before it.
        let mut output_lost = (output && cursor.output_after < state.output_floor)
            .then(|| state.output_floor.min(end))
            .filter(|&to| to > cursor.output_after);
        let mut main = state.held.partition_point(|held| held.seq <= cursor.after);
        let mut out = if output {
            state
                .output
                .partition_point(|held| held.seq <= cursor.output_after)
        } else {
            state.output.len()
        };
        loop {
            let a = state.held.get(main).filter(|held| held.seq <= end);
            let b = state.output.get(out).filter(|held| held.seq <= end);
            let next = [a.map(|a| a.seq), b.map(|b| b.seq), output_lost]
                .into_iter()
                .flatten()
                .min();
            let Some(seq) = next else {
                // Every event up to the end is read or not this cursor's.
                cursor.after = end;
                cursor.output_after = end;
                break;
            };
            let full = batch.len() >= lines.max(1) || (!batch.is_empty() && taken >= bytes);
            if full {
                break;
            }
            if output_lost == Some(seq) {
                let gap = gap(cursor.output_after + 1, seq, true);
                taken += gap.len();
                batch.push(gap);
                output_lost = None;
            } else {
                let held = if a.is_some_and(|a| a.seq == seq) {
                    main += 1;
                    a
                } else {
                    out += 1;
                    b
                };
                let held = held.expect("the next seq is one of the held events'");
                if cursor.topics.has(held.topic) {
                    taken += held.line.len();
                    batch.push(Arc::clone(&held.line));
                }
            }
            // Read in seq order: every held event before `seq`, in either partition, is
            // read or passed over.
            cursor.after = seq;
            cursor.output_after = cursor.output_after.max(seq);
        }
        batch
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn wait<'a>(&self, state: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        self.written
            .wait(state)
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The bus lock, once the mark on disk covers the next seq: at once, unless the mark
    /// is used up. Then it waits for the write in flight, or writes the mark itself, off
    /// the lock. Refused when the mark cannot be written.
    fn lock_for_a_seq(&self) -> Result<MutexGuard<'_, State>, Refused> {
        let mut state = self.state();
        if state.head == u64::MAX {
            return Err(Refused::Unnumbered("every seq is used".to_owned()));
        }
        while state.head >= state.reserved {
            if state.writing {
                state = self.wait(state);
                continue;
            }
            if let Some((at, why)) = &state.failed
                && at.elapsed() < RETRY_AFTER
            {
                return Err(Refused::Unnumbered(why.clone()));
            }
            let target = state.head.saturating_add(RESERVE);
            state.writing = true;
            drop(state);
            self.write_mark(target);
            state = self.state();
        }
        Ok(state)
    }

    /// Appends event `seq`, the next, to the `output` partition or the other, and moves
    /// the head watch on while the lock is still held, so it never goes back. The mark to
    /// write ahead, when this publisher is to.
    fn append_to(
        &self,
        state: &mut State,
        seq: u64,
        topic: u32,
        run: Option<Arc<str>>,
        line: Line,
        output: bool,
    ) -> Option<u64> {
        let held = Held {
            seq,
            topic,
            at: SystemTime::now(),
            run,
            line,
        };
        if output {
            state.output_bytes += held.line.len();
            state.output.push_back(held);
        } else {
            state.bytes += held.line.len();
            state.held.push_back(held);
        }
        state.head = seq;
        state.drop_old(&self.limits, &self.output_limits);
        self.head.send_replace(seq);
        let retry = state
            .failed
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() >= RETRY_AFTER);
        let low = seq.saturating_add(RESERVE / 2) > state.reserved;
        if state.writing || !retry || !low {
            return None;
        }
        state.writing = true;
        Some(seq.saturating_add(RESERVE))
    }

    /// What a publisher does once it has let the lock go: write the mark ahead, if it is
    /// the one to.
    fn after_unlock(&self, seq: u64, write: Option<u64>) {
        #[cfg(test)]
        self.hooks.after_unlock(seq);
        #[cfg(not(test))]
        let _ = seq;
        if let Some(target) = write {
            self.write_mark(target);
        }
    }

    /// Writes `target` as the mark, off the lock, then counts it, or the failure.
    fn write_mark(&self, target: u64) {
        #[cfg(test)]
        self.hooks.before_write();
        let result = self.mark.write(target);
        let mut state = self.state();
        state.writing = false;
        match result {
            Ok(()) => {
                state.reserved = state.reserved.max(target);
                state.failed = None;
            }
            Err(e) => {
                let why = format!("cannot write {}: {e}", self.mark.path.display());
                error!("{why} (tried again in {} s)", RETRY_AFTER.as_secs());
                state.failed = Some((Instant::now(), why));
            }
        }
        drop(state);
        self.written.notify_all();
    }
}

impl State {
    /// Drops the oldest events of each partition while it holds more than its limits
    /// allow, or they are too old. Each partition's newest event stays.
    fn drop_old(&mut self, limits: &Limits, output: &Limits) {
        let now = SystemTime::now();
        while let Some(oldest) = self.output.front() {
            let old = now.duration_since(oldest.at).unwrap_or_default() > output.age;
            let over = self.output.len() > output.events
                || (self.output_bytes > output.bytes && self.output.len() > 1);
            if !over && !old {
                break;
            }
            let Some(gone) = self.output.pop_front() else {
                break;
            };
            self.output_floor = gone.seq;
            self.output_bytes -= gone.line.len();
        }
        while let Some(oldest) = self.held.front() {
            let old = now.duration_since(oldest.at).unwrap_or_default() > limits.age;
            let over = self.held.len() > limits.events
                || (self.bytes > limits.bytes && self.held.len() > 1);
            if !over && !old {
                break;
            }
            let Some(gone) = self.held.pop_front() else {
                break;
            };
            self.floor = gone.seq;
            self.bytes -= gone.line.len();
            if let Some(run) = gone.run {
                let ended = self.runs.get_mut(&*run).is_some_and(|log| {
                    log.held.pop_front();
                    log.ended && log.held.is_empty()
                });
                if ended {
                    self.runs.remove(&*run);
                }
            }
        }
    }

    /// The line of held event `seq`, which is not `pty.output`.
    fn line(&self, seq: u64) -> Line {
        let at = self
            .held
            .binary_search_by_key(&seq, |held| held.seq)
            .expect("a held seq");
        Arc::clone(&self.held[at].line)
    }
}

/// `event` as it is sent, if a client can read it.
fn line(event: &Event) -> Result<Line, Refused> {
    let mut text = serde_json::to_vec(event).expect("a validated event prints");
    if text.len() > MAX_EVENT {
        return Err(Refused::TooLarge(text.len()));
    }
    text.push(b'\n');
    Ok(Arc::from(text))
}

/// `daemon.gap {from, to}`, numbered `to`.
/// `daemon.gap {from, to}`, or for the `pty.output` partition `{from, to, topics: ["pty"]}`:
/// only `pty` events went there, and the other topics' events in that span still come.
fn gap(from: u64, to: u64, output: bool) -> Line {
    let mut data = Data::new();
    data.insert("from".to_owned(), Value::from(from));
    data.insert("to".to_owned(), Value::from(to));
    if output {
        data.insert("topics".to_owned(), Value::from(vec![topics::PTY]));
    }
    let event = Event::new(
        to,
        clock::now(),
        topics::DAEMON,
        event_types::DAEMON_GAP,
        data,
    )
    .expect("a gap is a valid event");
    line(&event).expect("a gap is small")
}

/// `<home>/rlxd/seq`: a seq at or above every seq this home's rlxds have given out.
struct Mark {
    path: PathBuf,
    #[cfg(test)]
    fail: std::sync::atomic::AtomicBool,
}

impl Mark {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            #[cfg(test)]
            fail: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// The mark; 0 when there is none yet.
    fn read(&self) -> Result<u64, String> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(format!("cannot read {}: {e}", self.path.display())),
        };
        text.trim().parse().map_err(|_| {
            format!(
                "{} holds {:?}, not the last event seq: rlxd will not number events from 1 \
                 again on its own. Remove the file to do so, and clients may miss events \
                 numbered before",
                self.path.display(),
                text.trim()
            )
        })
    }

    /// Writes `seq` whole and durably (`crate::durable`).
    fn write(&self, seq: u64) -> io::Result<()> {
        #[cfg(test)]
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(io::Error::other("a test made the mark unwritable"));
        }
        durable::write(&self.path, format!("{seq}\n").as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc;
    use std::thread;

    use serde_json::json;

    use super::*;

    /// Where a test holds a publisher, or a mark write, until it says to go on.
    #[derive(Default)]
    pub(super) struct Hooks {
        after_unlock: Mutex<Option<(u64, Gate)>>,
        before_write: Mutex<Option<Gate>>,
    }

    struct Gate {
        reached: mpsc::Sender<()>,
        go_on: mpsc::Receiver<()>,
    }

    /// The test's ends of a [`Gate`]: wait until it is reached, then let it go on.
    struct Holding {
        reached: mpsc::Receiver<()>,
        go_on: mpsc::Sender<()>,
    }

    fn gate() -> (Gate, Holding) {
        let (reached, reached_rx) = mpsc::channel();
        let (go_on_tx, go_on) = mpsc::channel();
        (
            Gate { reached, go_on },
            Holding {
                reached: reached_rx,
                go_on: go_on_tx,
            },
        )
    }

    impl Hooks {
        pub(super) fn after_unlock(&self, seq: u64) {
            let mut slot = self.after_unlock.lock().unwrap();
            if slot.as_ref().is_some_and(|(at, _)| *at == seq) {
                let (_, gate) = slot.take().unwrap();
                drop(slot);
                let _ = gate.reached.send(());
                let _ = gate.go_on.recv();
            }
        }

        pub(super) fn before_write(&self) {
            let gate = self.before_write.lock().unwrap().take();
            if let Some(gate) = gate {
                let _ = gate.reached.send(());
                let _ = gate.go_on.recv();
            }
        }

        /// Holds the publisher of `seq` once it has let the lock go.
        fn hold_after_unlock(&self, seq: u64) -> Holding {
            let (gate, holding) = gate();
            *self.after_unlock.lock().unwrap() = Some((seq, gate));
            holding
        }

        /// Holds the next mark write before it starts.
        fn hold_next_write(&self) -> Holding {
            let (gate, holding) = gate();
            *self.before_write.lock().unwrap() = Some(gate);
            holding
        }
    }

    /// A fresh folder for a mark, removed on drop.
    struct Dir(PathBuf);

    impl Dir {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("rlxd-bus-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn mark(&self) -> PathBuf {
            self.0.join(MARK_FILE)
        }

        fn bus(&self, events: usize) -> Bus {
            self.bus_with(Limits {
                events,
                ..Limits::DEFAULT
            })
        }

        fn bus_with(&self, limits: Limits) -> Bus {
            Bus::open_at(self.mark(), limits).unwrap()
        }

        fn on_disk(&self) -> u64 {
            fs::read_to_string(self.mark())
                .unwrap()
                .trim()
                .parse()
                .unwrap()
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const CAPACITY: usize = Limits::DEFAULT.events;

    fn data(value: Value) -> Data {
        match value {
            Value::Object(data) => data,
            _ => panic!("not an object"),
        }
    }

    fn emit(bus: &Bus, n: u64) -> u64 {
        let mut last = 0;
        for i in 1..=n {
            last = bus
                .publish(topics::DAEMON, "daemon.debug", data(json!({"index": i})))
                .unwrap();
        }
        last
    }

    fn events(lines: &[Line]) -> Vec<Value> {
        lines
            .iter()
            .map(|line| {
                assert_eq!(line.last(), Some(&b'\n'), "a line ends with a newline");
                serde_json::from_slice(line).unwrap()
            })
            .collect()
    }

    fn seqs(lines: &[Line]) -> Vec<u64> {
        events(lines)
            .iter()
            .map(|e| e["seq"].as_u64().unwrap())
            .collect()
    }

    fn types(lines: &[Line]) -> Vec<String> {
        events(lines)
            .iter()
            .map(|e| e["type"].as_str().unwrap().to_owned())
            .collect()
    }

    fn read(bus: &Bus, cursor: &mut Cursor, lines: usize) -> Vec<Line> {
        bus.read(cursor, lines, BATCH_BYTES)
    }

    fn read_all(bus: &Bus, cursor: &mut Cursor) -> Vec<Line> {
        let mut all = Vec::new();
        loop {
            let batch = read(bus, cursor, BATCH);
            if batch.is_empty() {
                return all;
            }
            all.extend(batch);
        }
    }

    fn follower(bus: &Bus, since: Option<u64>) -> Cursor {
        bus.cursor(Topics::ALL, since, true).unwrap()
    }

    fn run(id: &str) -> Run<'_> {
        Run {
            id,
            kind: "task",
            attempt: 1,
            job_id: None,
        }
    }

    // --- numbering and the buffer ------------------------------------------------------

    #[test]
    fn seqs_start_at_1_in_a_new_home_and_strictly_increase() {
        let dir = Dir::new();
        let bus = dir.bus(CAPACITY);
        assert_eq!(bus.head(), 0);
        assert_eq!(emit(&bus, 3), 3);
        let mut cursor = follower(&bus, Some(0));
        let lines = read_all(&bus, &mut cursor);
        assert_eq!(seqs(&lines), [1, 2, 3]);
        assert_eq!(cursor.after(), 3);
        for event in events(&lines) {
            <Event as serde::Deserialize>::deserialize(event.clone())
                .unwrap_or_else(|e| panic!("{event}: {e}"));
        }
    }

    #[test]
    fn a_cursor_with_no_since_starts_after_the_last_event_and_a_later_since_is_refused() {
        let dir = Dir::new();
        let bus = dir.bus(CAPACITY);
        emit(&bus, 2);
        let mut live = follower(&bus, None);
        assert!(read(&bus, &mut live, BATCH).is_empty());
        emit(&bus, 1);
        assert_eq!(seqs(&read(&bus, &mut live, BATCH)), [3]);
        assert_eq!(bus.cursor(Topics::ALL, Some(4), true).unwrap_err(), 3);
        assert!(bus.cursor(Topics::ALL, Some(3), true).is_ok());
    }

    #[test]
    fn a_since_older_than_the_buffer_gets_a_gap_numbered_its_to_first() {
        let dir = Dir::new();
        let bus = dir.bus(5);
        emit(&bus, 8);
        let mut cursor = follower(&bus, Some(0));
        let lines = read_all(&bus, &mut cursor);
        let events = events(&lines);
        assert_eq!(events[0]["type"], "daemon.gap");
        assert_eq!(events[0]["data"], json!({"from": 1, "to": 3}));
        assert_eq!(events[0]["seq"], 3);
        assert_eq!(seqs(&lines), [3, 4, 5, 6, 7, 8]);
        // Resuming from the gap, or from inside the buffer, repeats nothing.
        let mut again = follower(&bus, Some(3));
        assert_eq!(seqs(&read_all(&bus, &mut again)), [4, 5, 6, 7, 8]);
        let mut again = follower(&bus, Some(7));
        assert_eq!(seqs(&read_all(&bus, &mut again)), [8]);
    }

    #[test]
    fn a_subscriber_that_falls_behind_the_buffer_gets_a_gap_mid_stream_then_goes_on() {
        let dir = Dir::new();
        let bus = dir.bus(4);
        emit(&bus, 2);
        let mut slow = follower(&bus, Some(0));
        assert_eq!(seqs(&read(&bus, &mut slow, 1)), [1]);
        emit(&bus, 6);
        let lines = read_all(&bus, &mut slow);
        assert_eq!(events(&lines)[0]["data"], json!({"from": 2, "to": 4}));
        assert_eq!(seqs(&lines), [4, 5, 6, 7, 8]);
    }

    #[test]
    fn a_gap_comes_whatever_the_topics() {
        let dir = Dir::new();
        let bus = dir.bus(3);
        emit(&bus, 6);
        let mut runs_only = bus
            .cursor(Topics::of(["run"]).unwrap(), Some(0), true)
            .unwrap();
        let lines = read_all(&bus, &mut runs_only);
        assert_eq!(types(&lines), ["daemon.gap"]);
        assert_eq!(
            runs_only.after(),
            6,
            "and passes the daemon events after it"
        );
    }

    #[test]
    fn events_older_than_the_age_limit_are_dropped() {
        let dir = Dir::new();
        let bus = dir.bus_with(Limits {
            age: Duration::from_millis(50),
            ..Limits::DEFAULT
        });
        emit(&bus, 3);
        thread::sleep(Duration::from_millis(120));
        emit(&bus, 1);
        let mut cursor = follower(&bus, Some(0));
        let lines = read_all(&bus, &mut cursor);
        assert_eq!(events(&lines)[0]["data"], json!({"from": 1, "to": 3}));
        assert_eq!(seqs(&lines), [3, 4]);
    }

    #[test]
    fn the_buffer_holds_at_most_its_bytes_and_the_newest_event_whatever_its_size() {
        // daemon.debug {index: 1} to {index: 9}: lines of one length.
        let probe = Dir::new();
        let one = {
            let bus = probe.bus(CAPACITY);
            emit(&bus, 1);
            read_all(&bus, &mut follower(&bus, Some(0)))[0].len()
        };
        let dir = Dir::new();
        let bus = dir.bus_with(Limits {
            bytes: 3 * one + one / 2,
            ..Limits::DEFAULT
        });
        emit(&bus, 9);
        let lines = read_all(&bus, &mut follower(&bus, Some(0)));
        assert_eq!(events(&lines)[0]["data"], json!({"from": 1, "to": 6}));
        assert_eq!(seqs(&lines), [6, 7, 8, 9]);

        let tiny = Dir::new();
        let bus = tiny.bus_with(Limits {
            bytes: 1,
            ..Limits::DEFAULT
        });
        emit(&bus, 3);
        let mut cursor = follower(&bus, Some(2));
        assert_eq!(seqs(&read_all(&bus, &mut cursor)), [3], "the newest stays");
    }

    #[test]
    fn an_event_longer_than_a_client_reads_is_refused_and_takes_no_seq() {
        let dir = Dir::new();
        let bus = dir.bus(CAPACITY);
        let huge = "x".repeat(MAX_EVENT);
        let refused = bus.publish(
            topics::DAEMON,
            "daemon.debug",
            data(json!({ "huge": huge })),
        );
        assert!(matches!(refused, Err(Refused::TooLarge(n)) if n > MAX_EVENT));
        assert_eq!(bus.head(), 0);
    }

    #[test]
    fn a_reader_gets_only_its_topics_and_its_cursor_passes_the_others() {
        let dir = Dir::new();
        let bus = dir.bus(CAPACITY);
        bus.publish(topics::HOOK, "hook.stop", Data::new()).unwrap();
        bus.publish(topics::DAEMON, "daemon.debug", Data::new())
            .unwrap();
        bus.publish(topics::PTY, "pty.exited", Data::new()).unwrap();
        let topics = Topics::of(["daemon", "pty"]).unwrap();
        let mut cursor = bus.cursor(topics, Some(0), true).unwrap();
        let lines = read_all(&bus, &mut cursor);
        assert_eq!(seqs(&lines), [2, 3]);
        assert_eq!(cursor.after(), 3);
        assert_eq!(Topics::of(["daemon", "nope"]).unwrap_err(), "nope");
        assert_eq!(
            Topics::of(topics::ALL.iter().copied()).unwrap(),
            Topics::ALL
        );
    }

    #[test]
    fn a_batch_holds_at_most_its_lines_and_past_the_first_its_bytes() {
        let dir = Dir::new();
        let bus = dir.bus(CAPACITY);
        emit(&bus, 10);
        let mut cursor = follower(&bus, Some(0));
        assert_eq!(seqs(&read(&bus, &mut cursor, 4)), [1, 2, 3, 4]);
        assert_eq!(cursor.after(), 4);
        assert_eq!(seqs(&read(&bus, &mut cursor, 4)), [5, 6, 7, 8]);
        let mut by_bytes = follower(&bus, Some(0));
        assert_eq!(
            seqs(&bus.read(&mut by_bytes, BATCH, 1)),
            [1],
            "one at least"
        );
        assert_eq!(seqs(&bus.read(&mut by_bytes, BATCH, 1)), [2]);
    }

    #[test]
    fn a_cursor_that_does_not_follow_ends_where_the_stream_was_when_it_was_made() {
        let dir = Dir::new();
        let bus = dir.bus(CAPACITY);
        emit(&bus, 5);
        let mut replay = bus.cursor(Topics::ALL, Some(0), false).unwrap();
        assert_eq!(seqs(&read(&bus, &mut replay, 2)), [1, 2]);
        emit(&bus, 5);
        assert_eq!(seqs(&read_all(&bus, &mut replay)), [3, 4, 5]);
        assert!(replay.is_done());
        assert_eq!(replay.after(), 5);
        assert!(!follower(&bus, Some(0)).is_done(), "a follower never is");

        // Ending inside a gap: the gap stops at the end too.
        let small = Dir::new();
        let bus = small.bus(2);
        emit(&bus, 3);
        let mut replay = bus.cursor(Topics::ALL, Some(0), false).unwrap();
        emit(&bus, 4);
        let lines = read_all(&bus, &mut replay);
        assert_eq!(events(&lines)[0]["data"], json!({"from": 1, "to": 3}));
        assert_eq!(seqs(&lines), [3]);
        assert!(replay.is_done());
    }

    #[test]
    fn the_bus_refuses_a_topic_it_does_not_carry_and_unnumbered_run_events() {
        let dir = Dir::new();
        let bus = dir.bus(CAPACITY);
        assert!(matches!(
            bus.publish("later", "later.thing", Data::new()),
            Err(Refused::Topic(_))
        ));
        assert!(matches!(
            bus.publish(topics::RUN, "run.text", Data::new()),
            Err(Refused::Topic(_))
        ));
        assert!(matches!(
            bus.publish(topics::DAEMON, "run.text", Data::new()),
            Err(Refused::Event(_))
        ));
        assert_eq!(bus.head(), 0, "nothing refused took a seq");
    }

    // --- the head watch ----------------------------------------------------------------

    #[test]
    fn publishing_wakes_a_watcher() {
        let dir = Dir::new();
        let bus = dir.bus(CAPACITY);
        let mut watcher = bus.watch();
        assert_eq!(*watcher.borrow_and_update(), 0);
        emit(&bus, 2);
        assert!(watcher.has_changed().unwrap());
        assert_eq!(*watcher.borrow_and_update(), 2);
    }

    /// Whether a follower that has read up to `after` and waits for more is woken, on a
    /// bus that publishes nothing more, within half a second.
    fn wakes_a_follower_after(bus: &Bus, after: u64) -> bool {
        let mut watcher = bus.watch();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let woken = watcher.wait_for(|&seq| seq > after);
            tokio::time::timeout(Duration::from_millis(500), woken)
                .await
                .is_ok()
        })
    }

    #[test]
    fn a_publisher_that_lets_the_lock_go_late_never_moves_the_watch_back() {
        // A publishes 1 and lets the lock go; before A goes on, B publishes 2. On a bus
        // that told the watch after letting the lock go, A would then set it back to 1,
        // and a follower that had read 1 would wait with 2 unread until some later event.
        let dir = Dir::new();
        let bus = Arc::new(dir.bus(CAPACITY));
        let holding = bus.hooks.hold_after_unlock(1);
        let a = {
            let bus = Arc::clone(&bus);
            thread::spawn(move || emit(&bus, 1))
        };
        holding.reached.recv().unwrap();
        assert_eq!(emit(&bus, 1), 2, "B");
        holding.go_on.send(()).unwrap();
        assert_eq!(a.join().unwrap(), 1, "A");
        assert_eq!(bus.head(), 2);
        assert_eq!(*bus.watch().borrow(), 2, "the watch is at the head");
        assert!(
            wakes_a_follower_after(&bus, 1),
            "a follower that read 1 finds 2 with nothing more published"
        );
    }

    #[test]
    fn under_many_racing_publishers_the_watch_ends_at_the_head() {
        let dir = Dir::new();
        let bus = Arc::new(dir.bus(CAPACITY));
        let publishers: Vec<_> = (0..8)
            .map(|_| {
                let bus = Arc::clone(&bus);
                thread::spawn(move || emit(&bus, 500))
            })
            .collect();
        for publisher in publishers {
            publisher.join().unwrap();
        }
        assert_eq!(bus.head(), 4_000);
        assert_eq!(*bus.watch().borrow(), 4_000);
        assert!(wakes_a_follower_after(&bus, 3_999));
        let got = seqs(&read_all(&bus, &mut follower(&bus, Some(0))));
        assert_eq!(
            got,
            (1..=4_000).collect::<Vec<_>>(),
            "every seq once, in order"
        );
    }

    // --- the mark ----------------------------------------------------------------------

    #[test]
    fn the_numbering_survives_a_clean_stop_with_no_gap() {
        let dir = Dir::new();
        let bus = dir.bus(CAPACITY);
        emit(&bus, 5);
        bus.close();
        drop(bus);
        assert_eq!(fs::read_to_string(dir.mark()).unwrap(), "5\n");
        let next = dir.bus(CAPACITY);
        assert_eq!(next.head(), 5);
        // A client that saw everything resumes with nothing missed.
        let mut caught_up = follower(&next, Some(5));
        emit(&next, 1);
        assert_eq!(seqs(&read_all(&next, &mut caught_up)), [6]);
        // One that saw less is told what the last rlxd took with it.
        let mut behind = follower(&next, Some(3));
        let lines = read_all(&next, &mut behind);
        assert_eq!(events(&lines)[0]["data"], json!({"from": 4, "to": 5}));
        assert_eq!(seqs(&lines), [5, 6]);
    }

    #[test]
    fn the_mark_is_written_ahead_before_it_is_used_up() {
        let dir = Dir::new();
        let bus = dir.bus(CAPACITY);
        assert_eq!(dir.on_disk(), RESERVE, "reserved at open");
        emit(&bus, RESERVE / 2);
        assert_eq!(dir.on_disk(), RESERVE, "half is left");
        emit(&bus, 1);
        assert_eq!(dir.on_disk(), RESERVE / 2 + 1 + RESERVE, "written ahead");
        emit(&bus, 2 * RESERVE);
        assert!(dir.on_disk() >= bus.head() + RESERVE / 2);
    }

    #[test]
    fn a_slow_mark_write_holds_up_no_other_publisher() {
        let dir = Dir::new();
        let bus = Arc::new(dir.bus(CAPACITY));
        emit(&bus, RESERVE / 2);
        let holding = bus.hooks.hold_next_write();
        let writer = {
            let bus = Arc::clone(&bus);
            thread::spawn(move || emit(&bus, 1))
        };
        holding.reached.recv().unwrap();
        // The write ahead is held. Others publish meanwhile, within what is reserved.
        let (done, finished) = mpsc::channel();
        let others = {
            let bus = Arc::clone(&bus);
            thread::spawn(move || {
                emit(&bus, RESERVE / 4);
                done.send(()).unwrap();
            })
        };
        let published = finished.recv_timeout(Duration::from_secs(10)).is_ok();
        holding.go_on.send(()).unwrap();
        writer.join().unwrap();
        others.join().unwrap();
        assert!(published, "the other publishers waited for the mark write");
        assert_eq!(bus.head(), RESERVE / 2 + 1 + RESERVE / 4);
    }

    #[test]
    fn a_failed_mark_write_moves_nothing_and_a_used_up_mark_refuses_rather_than_repeat() {
        let dir = Dir::new();
        let bus = dir.bus(CAPACITY);
        bus.mark.fail.store(true, Ordering::SeqCst);
        // Every write ahead fails: the mark on disk and the bus's both stay at RESERVE.
        emit(&bus, RESERVE);
        assert_eq!(dir.on_disk(), RESERVE);
        assert_eq!(bus.state().reserved, RESERVE);
        let refused = bus.publish(topics::DAEMON, "daemon.debug", Data::new());
        assert!(
            matches!(&refused, Err(Refused::Unnumbered(why)) if why.contains("unwritable")),
            "{refused:?}"
        );
        assert_eq!(bus.head(), RESERVE, "no seq past the mark on disk");
        // Once it can be written again, and the retry is due, publishing goes on.
        bus.mark.fail.store(false, Ordering::SeqCst);
        thread::sleep(RETRY_AFTER);
        assert_eq!(emit(&bus, 1), RESERVE + 1);
        assert!(dir.on_disk() > RESERVE + 1);
    }

    #[test]
    fn after_a_crash_the_numbering_starts_past_every_reserved_seq() {
        let dir = Dir::new();
        let bus = dir.bus(CAPACITY);
        let past = 3 * RESERVE + 7;
        emit(&bus, past);
        let reserved = dir.on_disk();
        assert!(reserved >= past, "{reserved} covers every seq given out");
        drop(bus); // no close: a crash
        let next = dir.bus(CAPACITY);
        assert_eq!(next.head(), reserved);
        let first = emit(&next, 1);
        assert!(first > past, "{first} repeats none of the {past} given out");
        let lines = read_all(&next, &mut follower(&next, Some(past)));
        assert_eq!(
            events(&lines)[0]["data"],
            json!({"from": past + 1, "to": reserved})
        );
    }

    #[test]
    fn close_waits_for_a_write_in_flight_then_writes_the_last_seq() {
        let dir = Dir::new();
        let bus = Arc::new(dir.bus(CAPACITY));
        emit(&bus, RESERVE / 2);
        let holding = bus.hooks.hold_next_write();
        let writer = {
            let bus = Arc::clone(&bus);
            thread::spawn(move || emit(&bus, 1))
        };
        holding.reached.recv().unwrap();
        let closer = {
            let bus = Arc::clone(&bus);
            thread::spawn(move || bus.close())
        };
        thread::sleep(Duration::from_millis(50));
        holding.go_on.send(()).unwrap();
        writer.join().unwrap();
        closer.join().unwrap();
        assert_eq!(
            dir.on_disk(),
            RESERVE / 2 + 1,
            "the last seq, not the reservation"
        );
    }

    #[test]
    fn a_mark_that_is_not_a_seq_stops_the_bus_from_opening() {
        let dir = Dir::new();
        fs::write(dir.mark(), "twelve\n").unwrap();
        let error = Bus::open_at(dir.mark(), Limits::DEFAULT).err().unwrap();
        assert!(
            error.contains("twelve") && error.contains("Remove the file"),
            "{error}"
        );
        fs::write(dir.mark(), "12\n").unwrap();
        assert_eq!(dir.bus(CAPACITY).head(), 12);
    }

    #[test]
    fn a_used_up_numbering_refuses_rather_than_overflow() {
        let dir = Dir::new();
        for nearly in [u64::MAX - RESERVE + 1, u64::MAX - 3, u64::MAX] {
            fs::write(dir.mark(), format!("{nearly}\n")).unwrap();
            let error = Bus::open_at(dir.mark(), Limits::DEFAULT).err().unwrap();
            assert!(error.contains("so few seqs are left"), "{nearly}: {error}");
        }
        fs::write(dir.mark(), format!("{}\n", u64::MAX - RESERVE)).unwrap();
        let bus = dir.bus(CAPACITY);
        assert_eq!(emit(&bus, RESERVE), u64::MAX, "the last seq is given out");
        let refused = bus.publish(topics::DAEMON, "daemon.debug", Data::new());
        assert!(
            matches!(&refused, Err(Refused::Unnumbered(why)) if why.contains("every seq")),
            "{refused:?}"
        );
        assert_eq!(bus.head(), u64::MAX);
    }

    #[cfg(unix)]
    #[test]
    fn the_mark_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Dir::new();
        let _bus = dir.bus(CAPACITY);
        let mode = fs::metadata(dir.mark()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // --- runs --------------------------------------------------------------------------

    #[test]
    fn each_run_numbers_its_own_events_from_1() {
        let dir = Dir::new();
        let bus = dir.bus(CAPACITY);
        let a1 = bus
            .publish_run(run("a"), "run.started", Data::new())
            .unwrap();
        let b1 = bus
            .publish_run(run("b"), "run.started", Data::new())
            .unwrap();
        emit(&bus, 1);
        let a2 = bus.publish_run(run("a"), "run.text", Data::new()).unwrap();
        assert_eq!(a1, Numbered { seq: 1, run_seq: 1 });
        assert_eq!(b1, Numbered { seq: 2, run_seq: 1 });
        assert_eq!(a2, Numbered { seq: 4, run_seq: 2 });

        let replay = bus.run_events("a", 1).unwrap();
        let a = events(&replay.lines);
        assert_eq!(a.len(), 2);
        assert_eq!(a[1]["data"]["run_id"], "a");
        assert_eq!(a[1]["data"]["run_seq"], 2);
        assert_eq!(a[1]["data"]["kind"], "task");
        assert_eq!(a[1]["data"]["attempt"], 1);
        assert_eq!(replay.through, 4);
        assert_eq!(events(&bus.run_events("a", 2).unwrap().lines).len(), 1);
        assert!(bus.run_events("a", 3).unwrap().lines.is_empty());
        assert!(bus.run_events("c", 1).is_none(), "a run the bus never had");
    }

    #[test]
    fn a_run_replay_hands_off_to_a_follower_with_nothing_missed_or_twice() {
        let dir = Dir::new();
        let bus = dir.bus(CAPACITY);
        bus.publish_run(run("r"), "run.started", Data::new())
            .unwrap();
        bus.publish_run(run("r"), "run.text", Data::new()).unwrap();
        let replay = bus.run_events("r", 1).unwrap();
        bus.publish_run(run("r"), "run.text", Data::new()).unwrap();
        let mut live = bus
            .cursor(Topics::of(["run"]).unwrap(), Some(replay.through), true)
            .unwrap();
        let run_seq = |e: &Value| e["data"]["run_seq"].as_u64().unwrap();
        let mut run_seqs: Vec<u64> = events(&replay.lines).iter().map(run_seq).collect();
        run_seqs.extend(events(&read_all(&bus, &mut live)).iter().map(run_seq));
        assert_eq!(run_seqs, [1, 2, 3]);
    }

    #[test]
    fn a_job_fired_run_carries_its_job() {
        let dir = Dir::new();
        let bus = dir.bus(CAPACITY);
        let fired = Run {
            job_id: Some("j1"),
            attempt: 2,
            ..run("r")
        };
        bus.publish_run(fired, "run.started", Data::new()).unwrap();
        let event = &events(&bus.run_events("r", 1).unwrap().lines)[0];
        assert_eq!(event["data"]["job_id"], "j1");
        assert_eq!(event["data"]["attempt"], 2);
    }

    #[test]
    fn a_run_whose_early_events_were_dropped_is_replayed_from_disk_not_the_buffer() {
        let dir = Dir::new();
        let bus = dir.bus(3);
        for _ in 0..3 {
            bus.publish_run(run("r"), "run.text", Data::new()).unwrap();
        }
        emit(&bus, 2);
        assert!(bus.run_events("r", 1).is_none(), "run_seq 1 and 2 are gone");
        assert!(bus.run_events("r", 2).is_none());
        let held = events(&bus.run_events("r", 3).unwrap().lines);
        assert_eq!(held.len(), 1);
        assert_eq!(held[0]["data"]["run_seq"], 3);
        emit(&bus, 1);
        assert!(bus.run_events("r", 3).is_none(), "none of it is held now");
        assert!(bus.run_events("r", 4).unwrap().lines.is_empty());
        // A run that goes on keeps its numbering, held or not.
        let next = bus.publish_run(run("r"), "run.text", Data::new()).unwrap();
        assert_eq!(next.run_seq, 4);
    }

    #[test]
    fn a_seeded_run_numbers_on_from_its_log() {
        let dir = Dir::new();
        let bus = dir.bus(CAPACITY);
        bus.seed_run("r", 41);
        let next = bus.publish_run(run("r"), "run.text", Data::new()).unwrap();
        assert_eq!(next.run_seq, 42);
        bus.seed_run("r", 7);
        let after = bus.publish_run(run("r"), "run.text", Data::new()).unwrap();
        assert_eq!(after.run_seq, 43, "a lower seed never numbers back");
        assert!(bus.run_events("r", 41).is_none(), "41 is on disk only");
        assert_eq!(events(&bus.run_events("r", 42).unwrap().lines).len(), 2);
    }

    #[test]
    fn an_ended_runs_entry_goes_with_its_last_held_event() {
        let dir = Dir::new();
        let bus = dir.bus(2);
        bus.publish_run(run("done"), "run.done", Data::new())
            .unwrap();
        bus.publish_run(run("live"), "run.text", Data::new())
            .unwrap();
        bus.end_run("done");
        assert!(
            bus.state().runs.contains_key("done"),
            "its event is still held"
        );
        emit(&bus, 2);
        let state = bus.state();
        assert!(!state.runs.contains_key("done"), "gone with its event");
        assert!(
            state.runs.contains_key("live"),
            "a run that has not ended stays"
        );
        drop(state);
        bus.seed_run("idle", 3);
        bus.end_run("idle");
        assert!(
            !bus.state().runs.contains_key("idle"),
            "nothing held: gone at once"
        );
    }

    const PTY_ID: &str = "6f1c2e0a9b8d4f3e2a1b0c9d8e7f6a5b";

    fn output(bus: &Bus, n: u64) -> u64 {
        let mut last = 0;
        for _ in 0..n {
            let data = rlxd_proto::PtyOutput::new(PTY_ID, "cmVhZHkNCg==")
                .unwrap()
                .to_data()
                .unwrap();
            last = bus
                .publish(topics::PTY, event_types::PTY_OUTPUT, data)
                .unwrap();
        }
        last
    }

    #[test]
    fn pty_output_never_pushes_another_topics_event_out() {
        let dir = Dir::new();
        let few = Limits {
            events: 4,
            ..Limits::DEFAULT
        };
        let bus = Bus::open_with(
            dir.mark(),
            few,
            Limits {
                events: 3,
                ..Limits::OUTPUT
            },
        )
        .unwrap();
        for _ in 0..3 {
            bus.publish_run(run("r1"), "run.text", Data::new()).unwrap();
        }
        let flood = output(&bus, 500);
        // The run's events are all still here, for a replay and for a since.
        let replay = bus.run_events("r1", 1).unwrap();
        assert_eq!(seqs(&replay.lines), [1, 2, 3]);
        let mut runs = bus
            .cursor(Topics::of([topics::RUN]).unwrap(), Some(0), false)
            .unwrap();
        let read = bus.read(&mut runs, BATCH, BATCH_BYTES);
        assert_eq!(
            seqs(&read),
            [1, 2, 3],
            "no gap for a topic that lost nothing"
        );
        assert!(
            runs.is_done(),
            "a replay ends at the head, an output event's"
        );
        assert_eq!(runs.after(), flood);
        // The output partition kept its last three.
        let mut pty = bus
            .cursor(Topics::of([topics::PTY]).unwrap(), Some(0), false)
            .unwrap();
        let read = events(&bus.read(&mut pty, BATCH, BATCH_BYTES));
        assert_eq!(read[0]["type"], "daemon.gap");
        // From 4: the run's events, 1 to 3, are still here, only not this cursor's.
        assert_eq!(
            read[0]["data"],
            json!({"from": 4, "to": flood - 3, "topics": ["pty"]})
        );
        let after: Vec<u64> = read[1..]
            .iter()
            .map(|e| e["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(after, [flood - 2, flood - 1, flood]);
    }

    /// #2574 major 1: an output flood costs no cursor the main ring's events it still
    /// holds: a cursor of every topic, one of run and pty, and one of pty alone, whose
    /// held pty.exited comes before the output's gap.
    #[test]
    fn an_output_flood_costs_no_cursor_the_main_rings_events() {
        let dir = Dir::new();
        let bus = Bus::open_with(
            dir.mark(),
            Limits::DEFAULT,
            Limits {
                events: 3,
                ..Limits::OUTPUT
            },
        )
        .unwrap();
        let exited = rlxd_proto::PtyExited::new(PTY_ID, 3, None)
            .unwrap()
            .to_data()
            .unwrap();
        let mut main = Vec::new();
        main.push(
            bus.publish(topics::PTY, event_types::PTY_EXITED, exited)
                .unwrap(),
        );
        for _ in 0..2 {
            main.push(
                bus.publish_run(run("r1"), "run.text", Data::new())
                    .unwrap()
                    .seq,
            );
        }
        main.push(emit(&bus, 3));
        let flood = output(&bus, 500);
        let after = bus
            .publish_run(run("r1"), "run.text", Data::new())
            .unwrap()
            .seq;
        for (topics, wanted) in [
            (Topics::ALL, vec![1, 2, 3, 4, 5, 6, after]),
            (
                Topics::of([topics::RUN, topics::PTY]).unwrap(),
                vec![1, 2, 3, after],
            ),
            (Topics::of([topics::PTY]).unwrap(), vec![1]),
        ] {
            let mut cursor = bus.cursor(topics, Some(0), false).unwrap();
            let mut read = Vec::new();
            loop {
                let batch = bus.read(&mut cursor, 7, BATCH_BYTES);
                if batch.is_empty() {
                    break;
                }
                read.extend(events(&batch));
            }
            let seqs: Vec<u64> = read.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
            assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{topics:?}: {seqs:?}");
            let gaps: Vec<&Value> = read.iter().filter(|e| e["type"] == "daemon.gap").collect();
            assert_eq!(gaps.len(), 1, "{topics:?}: {read:?}");
            assert_eq!(
                gaps[0]["data"],
                json!({"from": main[3] + 1, "to": flood - 3, "topics": ["pty"]}),
                "{topics:?}"
            );
            let held: Vec<u64> = read
                .iter()
                .filter(|e| e["type"] != "daemon.gap" && e["type"] != "pty.output")
                .map(|e| e["seq"].as_u64().unwrap())
                .collect();
            assert_eq!(held, wanted, "{topics:?}");
            let output: Vec<u64> = read
                .iter()
                .filter(|e| e["type"] == "pty.output")
                .map(|e| e["seq"].as_u64().unwrap())
                .collect();
            assert_eq!(output, [flood - 2, flood - 1, flood], "{topics:?}");
        }
        // A cursor without pty never hears of the output's loss.
        let mut runs = bus
            .cursor(Topics::of([topics::RUN]).unwrap(), Some(0), false)
            .unwrap();
        let read = events(&bus.read(&mut runs, BATCH, BATCH_BYTES));
        assert!(read.iter().all(|e| e["type"] == "run.text"), "{read:?}");
        assert_eq!(read.len(), 3);
    }

    #[test]
    fn both_partitions_read_in_seq_order() {
        let dir = Dir::new();
        let bus = dir.bus(CAPACITY);
        let mut expected = Vec::new();
        for i in 0..20 {
            expected.push(if i % 3 == 0 {
                emit(&bus, 1)
            } else {
                output(&bus, 1)
            });
        }
        let pty_exited = rlxd_proto::PtyExited::new(PTY_ID, 3, None)
            .unwrap()
            .to_data()
            .unwrap();
        expected.push(
            bus.publish(topics::PTY, event_types::PTY_EXITED, pty_exited)
                .unwrap(),
        );
        let mut all = bus.cursor(Topics::ALL, Some(0), false).unwrap();
        let mut read = Vec::new();
        loop {
            let batch = bus.read(&mut all, 4, BATCH_BYTES);
            if batch.is_empty() {
                break;
            }
            read.extend(seqs(&batch));
        }
        assert_eq!(read, expected);
        // A cursor that didn't ask for pty passes the output over, and still ends.
        let mut daemon = bus
            .cursor(Topics::of([topics::DAEMON]).unwrap(), Some(0), false)
            .unwrap();
        let read = seqs(&bus.read(&mut daemon, BATCH, BATCH_BYTES));
        assert_eq!(read, [1, 4, 7, 10, 13, 16, 19]);
        assert!(daemon.is_done());
    }
}
