//! The event bus (handoff 0.10.3, 1.7 and 7.3): one numbered stream of events per home,
//! which every client can subscribe to.
//!
//! # Two sequences
//!
//! - **`seq`** numbers every event of the home, from 1, across every topic and across
//!   restarts: one rlxd's events come after the last one's. rlxd keeps a mark in
//!   `<home>/rlxd/seq`, at or above every seq it has given out. It writes the mark
//!   [`RESERVE`] seqs ahead (to a sibling, fsynced, then renamed over it) before it
//!   gives out the first of them, and on a clean stop it writes the last seq it gave
//!   out. A new rlxd starts after the mark and holds nothing at or below it, so a client
//!   that resumes from an older seq gets a `daemon.gap` up to the mark: never an event
//!   twice, and never a silent hole. After a crash, that gap also covers the seqs that
//!   were reserved and never given out.
//!
//!   A mark rlxd cannot read stops it from starting, naming the file, rather than number
//!   from 1 again. Deleting the file does number from 1 again, and a client still holding
//!   a seq from before may then miss events. A mark rlxd cannot write is logged as an
//!   error, and events go on: a crash before the next good write could then repeat seqs.
//! - **`run_seq`** numbers one run's events, from 1 ([`Bus::publish_run`]). The bus
//!   indexes the events it holds by run, so one run can be replayed from a `run_seq`
//!   ([`Bus::run_events`]). Runs outlive the buffer: the runner (5.4) keeps each run's
//!   events on disk as well, and after a restart tells the bus where a run's numbering
//!   stands ([`Bus::seed_run`]).
//!
//! # The buffer
//!
//! The bus holds the last [`CAPACITY`] events, none older than [`MAX_AGE`], each as the
//! line it is sent as. That is what `events.subscribe {since}` replays.
//!
//! # Subscribers
//!
//! A subscriber is a place in the buffer ([`Cursor`]), not a queue of its own.
//! Publishing appends an event and wakes the subscribers, and never waits for one. Each
//! subscriber's connection copies out what comes after its place, at most [`BATCH`]
//! lines at a time, and writes them at its client's pace. So a client that stops reading
//! holds up no one, and holds at most one batch. When the buffer drops events that a
//! subscriber has not read yet, the subscriber gets a `daemon.gap {from, to}` for them,
//! then the events after them: what a client reconnecting from its last seq would get,
//! without the reconnect.
//!
//! A gap's own `seq` is its `to`. The stream's seqs still strictly increase, and resuming
//! from the gap skips nothing it did not report.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use rlxd_client::Home;
use rlxd_proto::{Data, Event, Invalid, RunScope, event_types, topics};
use serde_json::Value;
use tokio::sync::watch;

use crate::clock;
use crate::log::error;
use crate::registry::{private_file, retry_refused};

/// How many events the buffer holds: the last 10,000.
pub const CAPACITY: usize = 10_000;

/// How old an event the buffer holds: 24 hours.
pub const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// How many seqs ahead the mark is written.
pub const RESERVE: u64 = 1_000;

/// How many lines a subscriber copies out of the buffer at a time.
pub const BATCH: usize = 256;

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

/// Why an event was not published.
#[derive(Debug)]
pub enum Refused {
    /// The bus does not carry the topic, or it is `run`, whose events go through
    /// [`Bus::publish_run`].
    Topic(String),
    /// The event breaks a rule of `event.json`.
    Event(Invalid),
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Topic(topic) if topic == topics::RUN => {
                f.write_str("a run event is published with publish_run, which numbers it")
            }
            Self::Topic(topic) => write!(f, "the bus carries no topic {topic:?}"),
            Self::Event(invalid) => write!(f, "the event is not valid: {invalid}"),
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

/// A subscriber's place in the stream: the last seq it was given, or passed over as
/// not one of its topics.
#[derive(Clone, Debug)]
pub struct Cursor {
    after: u64,
    topics: Topics,
}

impl Cursor {
    /// The last seq this subscriber was given or passed over: a `since` to resume from.
    pub fn after(&self) -> u64 {
        self.after
    }
}

/// The event bus of one home.
pub struct Bus {
    state: Mutex<State>,
    head: watch::Sender<u64>,
    mark: Mark,
    capacity: usize,
    max_age: Duration,
}

struct State {
    /// The events held, oldest first: seqs `floor + 1` to `head`, one each.
    held: VecDeque<Held>,
    /// Every seq at or below it is gone: dropped from the buffer, or an earlier rlxd's.
    floor: u64,
    /// The last seq given out.
    head: u64,
    /// The mark as last written: seqs up to it may be given out.
    reserved: u64,
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
}

impl Bus {
    /// The bus of `home`, numbering on from its mark, which it reserves ahead first.
    pub fn open(home: &Home) -> Result<Self, String> {
        Self::open_at(home.rlxd_dir().join(MARK_FILE), CAPACITY, MAX_AGE)
    }

    /// A bus whose mark is `path`, holding `capacity` events none older than `max_age`.
    pub fn open_at(path: PathBuf, capacity: usize, max_age: Duration) -> Result<Self, String> {
        let mark = Mark { path };
        let last = mark.read()?;
        let reserved = last.saturating_add(RESERVE);
        mark.write(reserved)
            .map_err(|e| format!("cannot write {}: {e}", mark.path.display()))?;
        Ok(Self {
            state: Mutex::new(State {
                held: VecDeque::new(),
                floor: last,
                head: last,
                reserved,
                runs: HashMap::new(),
            }),
            head: watch::Sender::new(last),
            mark,
            capacity: capacity.max(1),
            max_age,
        })
    }

    /// Writes the last seq given out as the mark, so the next rlxd numbers on from it.
    pub fn close(&self) {
        let mut state = self.state();
        match self.mark.write(state.head) {
            Ok(()) => state.reserved = state.head,
            Err(e) => error!("cannot write {}: {e}", self.mark.path.display()),
        }
    }

    /// Publishes an event on `topic`, which is not `run`, and returns its seq.
    pub fn publish(&self, topic: &str, r#type: &str, data: Data) -> Result<u64, Refused> {
        let bit = bit(topic)
            .filter(|_| topic != topics::RUN)
            .ok_or_else(|| Refused::Topic(topic.to_owned()))?;
        let mut state = self.state();
        let seq = state.head + 1;
        let event = Event::new(seq, clock::now(), topic, r#type, data).map_err(Refused::Event)?;
        self.append(&mut state, &event, bit, None);
        drop(state);
        self.head.send_replace(seq);
        Ok(seq)
    }

    /// Publishes an event of `run`, giving it the run's next `run_seq`.
    pub fn publish_run(&self, run: Run<'_>, r#type: &str, data: Data) -> Result<Numbered, Refused> {
        let bit = bit(topics::RUN).expect("the bus carries run events");
        let mut state = self.state();
        let seq = state.head + 1;
        let run_seq = state.runs.get(run.id).map_or(0, |log| log.last) + 1;
        let mut scope = RunScope::new(run.id, run_seq, run.kind, run.attempt);
        if let Some(job_id) = run.job_id {
            scope = scope.with_job(job_id);
        }
        let event =
            Event::for_run(seq, clock::now(), r#type, scope, data).map_err(Refused::Event)?;
        let id: Arc<str> = match state.runs.get_key_value(run.id) {
            Some((id, _)) => Arc::clone(id),
            None => Arc::from(run.id),
        };
        let log = state.runs.entry(Arc::clone(&id)).or_default();
        log.last = run_seq;
        log.held.push_back((run_seq, seq));
        self.append(&mut state, &event, bit, Some(id));
        drop(state);
        self.head.send_replace(seq);
        Ok(Numbered { seq, run_seq })
    }

    /// Says that run `id` has numbered its events up to `last_run_seq` already, as its
    /// log on disk says after a restart, so its next event gets the number after.
    pub fn seed_run(&self, id: &str, last_run_seq: u64) {
        let mut state = self.state();
        let log = state.runs.entry(Arc::from(id)).or_default();
        log.last = log.last.max(last_run_seq);
    }

    /// Run `id`'s events from `run_seq` `from` on, oldest first, when the buffer still
    /// holds every one of them: none when `from` is past the run's last. `None` when the
    /// buffer has dropped some of them, or never had the run; its log on disk has them.
    pub fn run_events(&self, id: &str, from: u64) -> Option<Vec<Line>> {
        let state = self.state();
        let log = state.runs.get(id)?;
        let from = from.max(1);
        if from > log.last {
            return Some(Vec::new());
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
        Some(lines)
    }

    /// The last seq given out: `status`'s `seq`, 0 before the first event of a new home.
    pub fn head(&self) -> u64 {
        self.state().head
    }

    /// Changes to [`Bus::head`], to wait for new events on.
    pub fn watch(&self) -> watch::Receiver<u64> {
        self.head.subscribe()
    }

    /// A subscriber to `topics` after `since`, or after the last event when `since` is
    /// `None`. A `since` past the last event is refused, with the last event's seq: it
    /// comes from another numbering.
    pub fn cursor(&self, topics: Topics, since: Option<u64>) -> Result<Cursor, u64> {
        let head = self.state().head;
        match since {
            Some(since) if since > head => Err(head),
            since => Ok(Cursor {
                after: since.unwrap_or(head),
                topics,
            }),
        }
    }

    /// The next lines for `cursor`, at most `max`, moving it past them: a `daemon.gap`
    /// first when the buffer has dropped events after it. None when it is up to date.
    pub fn read(&self, cursor: &mut Cursor, max: usize) -> Vec<Line> {
        let mut state = self.state();
        state.drop_old(self.capacity, self.max_age);
        let mut lines = Vec::new();
        if cursor.after < state.floor {
            lines.push(gap(cursor.after + 1, state.floor));
            cursor.after = state.floor;
        }
        let start = usize::try_from(cursor.after - state.floor).unwrap_or(usize::MAX);
        for held in state.held.range(start.min(state.held.len())..) {
            if lines.len() >= max.max(1) {
                break;
            }
            cursor.after = held.seq;
            if cursor.topics.has(held.topic) {
                lines.push(Arc::clone(&held.line));
            }
        }
        lines
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Appends `event`, numbered `state.head + 1`, reserving its seq first.
    fn append(&self, state: &mut State, event: &Event, topic: u32, run: Option<Arc<str>>) {
        if event.seq > state.reserved {
            let reserved = event.seq.saturating_add(RESERVE - 1);
            if let Err(e) = self.mark.write(reserved) {
                error!(
                    "cannot write {} (seqs after a crash may repeat): {e}",
                    self.mark.path.display()
                );
            }
            state.reserved = reserved;
        }
        state.held.push_back(Held {
            seq: event.seq,
            topic,
            at: SystemTime::now(),
            run,
            line: line(event),
        });
        state.head = event.seq;
        state.drop_old(self.capacity, self.max_age);
    }
}

impl State {
    /// Drops the oldest events while there are more than `capacity`, or they are older
    /// than `max_age`.
    fn drop_old(&mut self, capacity: usize, max_age: Duration) {
        let now = SystemTime::now();
        while let Some(oldest) = self.held.front() {
            let old = now.duration_since(oldest.at).unwrap_or_default() > max_age;
            if self.held.len() <= capacity && !old {
                break;
            }
            let Some(gone) = self.held.pop_front() else {
                break;
            };
            self.floor = gone.seq;
            if let Some(run) = gone.run
                && let Some(log) = self.runs.get_mut(&*run)
            {
                log.held.pop_front();
            }
        }
    }

    /// The line of held event `seq`.
    fn line(&self, seq: u64) -> Line {
        let at = usize::try_from(seq - self.floor - 1).expect("a held seq");
        Arc::clone(&self.held[at].line)
    }
}

/// `event` as it is sent.
fn line(event: &Event) -> Line {
    let mut text = serde_json::to_vec(event).expect("a validated event prints");
    text.push(b'\n');
    Arc::from(text)
}

/// `daemon.gap {from, to}`, numbered `to`.
fn gap(from: u64, to: u64) -> Line {
    let mut data = Data::new();
    data.insert("from".to_owned(), Value::from(from));
    data.insert("to".to_owned(), Value::from(to));
    let event = Event::new(
        to,
        clock::now(),
        topics::DAEMON,
        event_types::DAEMON_GAP,
        data,
    )
    .expect("a gap is a valid event");
    line(&event)
}

/// `<home>/rlxd/seq`: a seq at or above every seq this home's rlxds have given out.
struct Mark {
    path: PathBuf,
}

impl Mark {
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

    /// Writes `seq` whole: a sibling written and fsynced, then renamed over the mark.
    fn write(&self, seq: u64) -> io::Result<()> {
        let temporary = sibling(&self.path, "tmp");
        let mut file = private_file(&temporary)?;
        file.write_all(format!("{seq}\n").as_bytes())?;
        file.sync_all()?;
        drop(file);
        retry_refused(|| fs::rename(&temporary, &self.path))
    }
}

fn sibling(path: &Path, extension: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(extension);
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use serde_json::json;

    use super::*;

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

        fn bus(&self, capacity: usize) -> Bus {
            Bus::open_at(self.mark(), capacity, MAX_AGE).unwrap()
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

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

    fn read_all(bus: &Bus, cursor: &mut Cursor) -> Vec<Line> {
        let mut all = Vec::new();
        loop {
            let batch = bus.read(cursor, BATCH);
            if batch.is_empty() {
                return all;
            }
            all.extend(batch);
        }
    }

    #[test]
    fn seqs_start_at_1_in_a_new_home_and_strictly_increase() {
        let dir = Dir::new();
        let bus = dir.bus(CAPACITY);
        assert_eq!(bus.head(), 0);
        assert_eq!(emit(&bus, 3), 3);
        let mut cursor = bus.cursor(Topics::ALL, Some(0)).unwrap();
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
        let mut live = bus.cursor(Topics::ALL, None).unwrap();
        assert!(bus.read(&mut live, BATCH).is_empty());
        emit(&bus, 1);
        assert_eq!(seqs(&bus.read(&mut live, BATCH)), [3]);
        assert_eq!(bus.cursor(Topics::ALL, Some(4)).unwrap_err(), 3);
        assert!(bus.cursor(Topics::ALL, Some(3)).is_ok());
    }

    #[test]
    fn a_since_older_than_the_buffer_gets_a_gap_numbered_its_to_first() {
        let dir = Dir::new();
        let bus = dir.bus(5);
        emit(&bus, 8);
        let mut cursor = bus.cursor(Topics::ALL, Some(0)).unwrap();
        let lines = read_all(&bus, &mut cursor);
        let events = events(&lines);
        assert_eq!(events[0]["type"], "daemon.gap");
        assert_eq!(events[0]["data"], json!({"from": 1, "to": 3}));
        assert_eq!(events[0]["seq"], 3);
        assert_eq!(seqs(&lines), [3, 4, 5, 6, 7, 8]);
        // Resuming from the gap, or from inside the buffer, repeats nothing.
        let mut again = bus.cursor(Topics::ALL, Some(3)).unwrap();
        assert_eq!(seqs(&read_all(&bus, &mut again)), [4, 5, 6, 7, 8]);
        let mut again = bus.cursor(Topics::ALL, Some(7)).unwrap();
        assert_eq!(seqs(&read_all(&bus, &mut again)), [8]);
    }

    #[test]
    fn a_subscriber_that_falls_behind_the_buffer_gets_a_gap_then_goes_on() {
        let dir = Dir::new();
        let bus = dir.bus(4);
        emit(&bus, 2);
        let mut slow = bus.cursor(Topics::ALL, Some(0)).unwrap();
        assert_eq!(seqs(&bus.read(&mut slow, 1)), [1]);
        emit(&bus, 6);
        let lines = read_all(&bus, &mut slow);
        assert_eq!(events(&lines)[0]["data"], json!({"from": 2, "to": 4}));
        assert_eq!(seqs(&lines), [4, 5, 6, 7, 8]);
    }

    #[test]
    fn events_older_than_the_age_limit_are_dropped() {
        let dir = Dir::new();
        let bus = Bus::open_at(dir.mark(), CAPACITY, Duration::from_millis(50)).unwrap();
        emit(&bus, 3);
        std::thread::sleep(Duration::from_millis(120));
        emit(&bus, 1);
        let mut cursor = bus.cursor(Topics::ALL, Some(0)).unwrap();
        let lines = read_all(&bus, &mut cursor);
        assert_eq!(events(&lines)[0]["data"], json!({"from": 1, "to": 3}));
        assert_eq!(seqs(&lines), [3, 4]);
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
        let mut cursor = bus.cursor(topics, Some(0)).unwrap();
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
    fn a_batch_holds_at_most_its_maximum() {
        let dir = Dir::new();
        let bus = dir.bus(CAPACITY);
        emit(&bus, 10);
        let mut cursor = bus.cursor(Topics::ALL, Some(0)).unwrap();
        assert_eq!(seqs(&bus.read(&mut cursor, 4)), [1, 2, 3, 4]);
        assert_eq!(cursor.after(), 4);
        assert_eq!(seqs(&bus.read(&mut cursor, 4)), [5, 6, 7, 8]);
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

    fn run(id: &str) -> Run<'_> {
        Run {
            id,
            kind: "task",
            attempt: 1,
            job_id: None,
        }
    }

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

        let a = events(&bus.run_events("a", 1).unwrap());
        assert_eq!(a.len(), 2);
        assert_eq!(a[1]["data"]["run_id"], "a");
        assert_eq!(a[1]["data"]["run_seq"], 2);
        assert_eq!(a[1]["data"]["kind"], "task");
        assert_eq!(a[1]["data"]["attempt"], 1);
        assert_eq!(events(&bus.run_events("a", 2).unwrap()).len(), 1);
        assert!(bus.run_events("a", 3).unwrap().is_empty());
        assert!(bus.run_events("c", 1).is_none(), "a run the bus never had");
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
        let event = &events(&bus.run_events("r", 1).unwrap())[0];
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
        let held = events(&bus.run_events("r", 3).unwrap());
        assert_eq!(held.len(), 1);
        assert_eq!(held[0]["data"]["run_seq"], 3);
        emit(&bus, 1);
        assert!(bus.run_events("r", 3).is_none(), "none of it is held now");
        assert!(bus.run_events("r", 4).unwrap().is_empty());
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
        assert_eq!(events(&bus.run_events("r", 42).unwrap()).len(), 2);
    }

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
        let mut caught_up = next.cursor(Topics::ALL, Some(5)).unwrap();
        emit(&next, 1);
        assert_eq!(seqs(&read_all(&next, &mut caught_up)), [6]);
        // One that saw less is told what the last rlxd took with it.
        let mut behind = next.cursor(Topics::ALL, Some(3)).unwrap();
        let lines = read_all(&next, &mut behind);
        assert_eq!(events(&lines)[0]["data"], json!({"from": 4, "to": 5}));
        assert_eq!(seqs(&lines), [5, 6]);
    }

    #[test]
    fn after_a_crash_the_numbering_starts_past_every_reserved_seq() {
        let dir = Dir::new();
        let bus = dir.bus(CAPACITY);
        emit(&bus, 3);
        assert_eq!(
            fs::read_to_string(dir.mark()).unwrap(),
            format!("{RESERVE}\n"),
            "the mark is written ahead"
        );
        let past = RESERVE + 2;
        emit(&bus, past - 3);
        assert_eq!(
            fs::read_to_string(dir.mark()).unwrap(),
            format!("{}\n", RESERVE + 1 + RESERVE - 1),
            "and again before the first seq past it"
        );
        drop(bus); // no close: a crash
        let next = dir.bus(CAPACITY);
        assert_eq!(next.head(), 2 * RESERVE);
        let first = emit(&next, 1);
        assert!(first > past, "{first} repeats none of the {past} given out");
        let mut resumed = next.cursor(Topics::ALL, Some(past)).unwrap();
        let lines = read_all(&next, &mut resumed);
        assert_eq!(
            events(&lines)[0]["data"],
            json!({"from": past + 1, "to": 2 * RESERVE})
        );
    }

    #[test]
    fn a_mark_that_is_not_a_seq_stops_the_bus_from_opening() {
        let dir = Dir::new();
        fs::write(dir.mark(), "twelve\n").unwrap();
        let error = Bus::open_at(dir.mark(), CAPACITY, MAX_AGE).err().unwrap();
        assert!(
            error.contains("twelve") && error.contains("Remove the file"),
            "{error}"
        );
        fs::write(dir.mark(), "12\n").unwrap();
        assert_eq!(dir.bus(CAPACITY).head(), 12);
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
}
