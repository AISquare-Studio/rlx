//! A run's durable log (handoff 0.10.3, 5.4): `<home>/rlxd/runs/<run_id>/`.
//!
//! - **`run.json`**, the run's record: its params, state, attempt, times, last `run_seq`,
//!   session, structured output, its process (pid and start time, for the sweep at the
//!   next start) and its call ledger (5.1 fills it). Each change rewrites it whole and
//!   durably: a 0600 sibling, fsynced, renamed over it, and the folder fsynced (Windows
//!   renames write-through), so after a crash it is the old record or the new.
//! - **`events.jsonl`**, the run's events, each the line subscribers got (the same `seq`
//!   and `run_seq`), appended in one write. Every event is fsynced as it is appended,
//!   except partial `run.text` chunks, which become durable with the next event that is.
//!   So after an OS crash at most the latest partial chunks are lost, never an event that
//!   changed the run's state. A reader skips a last line a crash left torn.
//! - **`<attempt>/settings.json`**, each attempt's own Claude Code settings (5.1).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use rlxd_client::Home;
use rlxd_proto::{RunId, RunKind, RunStartParams, RunState};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::durable;

/// `<home>/rlxd/runs/`.
pub fn runs_dir(home: &Home) -> PathBuf {
    home.rlxd_dir().join("runs")
}

/// The record's format.
pub const RECORD_VERSION: u32 = 1;

/// A run's record, `run.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub version: u32,
    pub run_id: RunId,
    pub kind: RunKind,
    pub params: RunStartParams,
    /// The `client` the starting connection's hello named.
    pub client: String,
    pub state: RunState,
    pub attempt: u64,
    pub started_at: String,
    #[serde(default)]
    pub ended_at: Option<String>,
    pub last_run_seq: u64,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub structured_output: Option<Value>,
    /// The attempt's process, while it may run: for the sweep at rlxd's next start.
    #[serde(default)]
    pub process: Option<Process>,
    /// The scheduler job that fired the run (7.6).
    #[serde(default)]
    pub job_id: Option<String>,
    /// That job's `loud`, echoed on the run's `run.done` and `run.needs_you`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loud: Option<bool>,
    /// The run's last state event, `{run_seq, type, data}`, written here before it was
    /// published: a crash before its line reached the log leaves it here.
    #[serde(default)]
    pub last_event: Option<Value>,
    /// The boot this record was written in ([`crate::runner`]'s `boot_id`): a run in
    /// flight across a reboot may have lost its last unsynced events.
    #[serde(default)]
    pub boot: Option<String>,
    /// Whether some attempt reached `run.started` with its session kept.
    #[serde(default)]
    pub session_started: bool,
    /// The call ledger: one entry per gated call (5.1).
    #[serde(default)]
    pub calls: Vec<rlxd_proto::RunCall>,
    /// Whether the run's pinned call was allowed once (5.1): every later call is denied.
    #[serde(default)]
    pub pinned_call_used: bool,
    /// The connectors a failed run's `needs_you {reason: "auth"}` waits on (5.5), while
    /// it is out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auth: Vec<String>,
    /// The `CLAUDE_CONFIG_DIR` the app's values gave its latest attempt (4.3): whose
    /// sign-in `auth` waits on. None when they gave none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    /// The account the run is on, and why (#15): its route, which every attempt keeps.
    /// Absent in a record from before routing: `default`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<rlxd_proto::RunAccount>,
    /// Whether the run is a `connectors.call` (5.3): it is never resumed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub connectors_call: bool,
}

/// A run's process, as the sweep finds it again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Process {
    pub pid: u32,
    /// Its start time, as `process::start_time` gave it: `None` where the OS needs none.
    pub started: Option<String>,
}

/// One run's folder.
#[derive(Clone, Debug)]
pub struct RunDir {
    dir: PathBuf,
}

impl RunDir {
    /// The folder of run `id` in `home`.
    pub fn of(home: &Home, id: &RunId) -> Self {
        Self {
            dir: runs_dir(home).join(id.as_str()),
        }
    }

    /// The folder.
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Whether the folder is there: the run is known, from now or an earlier rlxd.
    pub fn exists(&self) -> bool {
        self.dir.exists()
    }

    /// Makes the folder, private to the user.
    pub fn create(&self) -> io::Result<()> {
        private_dir(&self.dir)
    }

    /// `call/`: a `connectors.call` run's working folder (5.3), private, which rlxd writes
    /// nothing into: Claude Code finds no project settings there.
    pub fn create_call_dir(&self) -> io::Result<PathBuf> {
        let dir = self.dir.join("call");
        private_dir(&dir)?;
        Ok(dir)
    }

    /// `<attempt>/settings.json`.
    pub fn settings(&self, attempt: u64) -> PathBuf {
        self.dir.join(attempt.to_string()).join("settings.json")
    }

    /// Writes `attempt`'s settings durably, and returns their path.
    pub fn write_settings(&self, attempt: u64, settings: &Value) -> io::Result<PathBuf> {
        let path = self.settings(attempt);
        if let Some(dir) = path.parent() {
            private_dir(dir)?;
        }
        let mut text = serde_json::to_vec_pretty(settings).map_err(io::Error::other)?;
        text.push(b'\n');
        durable::write(&path, &text)?;
        Ok(path)
    }

    /// `events.jsonl`.
    pub fn events(&self) -> PathBuf {
        self.dir.join("events.jsonl")
    }

    /// The record, as the last rlxd wrote it.
    pub fn read_record(&self) -> io::Result<Record> {
        let text = fs::read(self.dir.join("run.json"))?;
        serde_json::from_slice(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// Writes the record whole and durably.
    pub fn write_record(&self, record: &Record) -> io::Result<()> {
        let mut text = serde_json::to_vec(record).map_err(io::Error::other)?;
        text.push(b'\n');
        durable::write(&self.dir.join("run.json"), &text)
    }

    /// The event log, opened to append.
    pub fn open_events(&self) -> io::Result<File> {
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(self.events())
    }
}

/// The end of a run's event log, as a restarted rlxd reads it.
#[derive(Debug, Default)]
pub struct Tail {
    /// The last whole event's line, with its newline.
    pub last_line: Option<Vec<u8>>,
    /// Its `run_seq`.
    pub last_run_seq: u64,
}

/// The last whole line of the log at `path`, read back from its end, and its `run_seq`.
/// A last line a crash left without its newline is not one: [`repair`] cuts it.
pub fn tail(path: &Path) -> io::Result<Tail> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Tail::default()),
        Err(e) => return Err(e),
    };
    let len = file.metadata()?.len();
    let mut end = len;
    let mut kept: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 64 << 10];
    // Read backwards a chunk at a time until the kept bytes hold a newline before the
    // last whole line's own, or the file's start.
    loop {
        let start = end.saturating_sub(chunk.len() as u64);
        let size = usize::try_from(end - start).unwrap_or(0);
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut chunk[..size])?;
        let mut joined = chunk[..size].to_vec();
        joined.extend_from_slice(&kept);
        kept = joined;
        end = start;
        let Some(last_newline) = kept.iter().rposition(|&b| b == b'\n') else {
            if start == 0 {
                return Ok(Tail::default());
            }
            continue;
        };
        let body = &kept[..last_newline];
        match body.iter().rposition(|&b| b == b'\n') {
            Some(before) => {
                return Ok(tail_of(kept[before + 1..=last_newline].to_vec()));
            }
            None if start == 0 => return Ok(tail_of(kept[..=last_newline].to_vec())),
            None => {}
        }
    }
}

fn tail_of(line: Vec<u8>) -> Tail {
    let last_run_seq = serde_json::from_slice::<Value>(&line)
        .ok()
        .and_then(|event| event["data"]["run_seq"].as_u64())
        .unwrap_or_default();
    Tail {
        last_line: Some(line),
        last_run_seq,
    }
}

/// When the log's last event ends `attempt` (`run.done`, `run.failed` or
/// `run.needs_you`) and `record` doesn't say so, a crash came between the two: takes the
/// end into `record`, and names it.
pub fn adopt_end(tail: &Tail, attempt: u64, record: &mut Record) -> Option<&'static str> {
    let event: Value = serde_json::from_slice(tail.last_line.as_deref()?).ok()?;
    if event["data"]["attempt"].as_u64() != Some(attempt) {
        return None;
    }
    let (state, name) = match event["type"].as_str()? {
        "run.done" => (RunState::Done, "run.done"),
        "run.failed" => (RunState::Failed, "run.failed"),
        "run.needs_you" => (RunState::NeedsYou, "run.needs_you"),
        _ => return None,
    };
    record.state = state;
    record.ended_at = event["time"].as_str().map(str::to_owned);
    record.last_run_seq = record.last_run_seq.max(tail.last_run_seq);
    record.process = None;
    if state == RunState::Done {
        record.session = event["data"]["session"].as_str().map(str::to_owned);
        record.structured_output = event["data"]
            .get("structured_output")
            .filter(|v| !v.is_null())
            .cloned();
    }
    Some(name)
}

/// Cuts a last line that a crash left without its newline from the log at `path`, so the
/// next event starts a line of its own. It reads back from the end only as far as the
/// last newline.
pub fn repair(path: &Path) -> io::Result<()> {
    let mut file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let len = file.metadata()?.len();
    let mut end = len;
    let mut chunk = vec![0u8; 64 << 10];
    while end > 0 {
        let start = end.saturating_sub(chunk.len() as u64);
        let size = usize::try_from(end - start).unwrap_or(0);
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut chunk[..size])?;
        if let Some(at) = chunk[..size].iter().rposition(|&b| b == b'\n') {
            let keep = start + at as u64 + 1;
            if keep == len {
                return Ok(());
            }
            file.set_len(keep)?;
            return file.sync_all();
        }
        end = start;
    }
    if len > 0 {
        file.set_len(0)?;
        file.sync_all()?;
    }
    Ok(())
}

/// The text of `attempt`'s whole assistant messages in the log at `path`: what a run in
/// flight when rlxd stopped had got out. Read a line at a time.
pub fn attempt_text(path: &Path, attempt: u64) -> String {
    use std::io::BufRead;
    let Ok(file) = File::open(path) else {
        return String::new();
    };
    let mut said = String::new();
    for line in io::BufReader::new(file).split(b'\n').map_while(Result::ok) {
        let Ok(event) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if event["type"] == "run.text"
            && event["data"]["partial"] == false
            && event["data"]["attempt"] == attempt
            && let Some(piece) = event["data"]["text"].as_str()
        {
            said.push_str(piece);
        }
    }
    said
}

/// A reader of a run's log from a place in it, for `run.events`: each whole line once.
#[derive(Debug)]
pub struct Reader {
    path: PathBuf,
    offset: u64,
    partial: Vec<u8>,
    /// The log was not there at the last read.
    gone: bool,
}

impl Reader {
    /// A reader of the log at `path` from its start.
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            partial: Vec::new(),
            gone: false,
        }
    }

    /// Whether the log was not there at the last read: its run was erased.
    pub fn gone(&self) -> bool {
        self.gone
    }

    /// The whole lines written since the last read, up to byte `limit` of the log (the end
    /// of its last append that succeeded, so nothing of an append that failed and was cut
    /// back is ever read), each with its newline: a few MiB of them at a time, so a long
    /// log is sent in turns. None once it has read every whole line up to `limit`.
    pub fn read(&mut self, limit: u64) -> io::Result<Vec<Vec<u8>>> {
        /// How much is read at a time.
        const CHUNK: u64 = 4 << 20;
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.gone = true;
                return Ok(Vec::new());
            }
            Err(e) => return Err(e),
        };
        self.gone = false;
        let len = file.metadata()?.len();
        if len < self.offset {
            // The log was cut back under this reader: a failed append is cut off
            // ([`append`]). What it had read of that append goes, so it is never joined
            // to the next line.
            let held = self.offset - self.partial.len() as u64;
            match usize::try_from(len.saturating_sub(held)) {
                Ok(keep) if len >= held => self.partial.truncate(keep),
                _ => self.partial.clear(),
            }
            self.offset = len;
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let end = loop {
            let room = limit.saturating_sub(self.offset).min(CHUNK);
            let mut fresh = Vec::new();
            let read = (&mut file).take(room).read_to_end(&mut fresh)?;
            self.offset += read as u64;
            self.partial.extend_from_slice(&fresh);
            match self.partial.iter().rposition(|&b| b == b'\n') {
                Some(end) => break end,
                // A line longer than a chunk: read on until it ends, or the log does.
                None if read > 0 => {}
                None => return Ok(Vec::new()),
            }
        };
        let rest = self.partial.split_off(end + 1);
        let whole = std::mem::replace(&mut self.partial, rest);
        Ok(whole
            .split_inclusive(|&b| b == b'\n')
            .map(<[u8]>::to_vec)
            .collect())
    }
}

/// Appends `line`, one event and its newline, in one write, then fsyncs it when `sync`:
/// where the log ends then. A write that fails part way is cut back off, so no torn line
/// is left in the log.
pub fn append(file: &mut File, line: &[u8], sync: bool) -> io::Result<u64> {
    let before = file.metadata()?.len();
    if let Err(e) = file.write_all(line) {
        // Whatever of the line was written goes: the next append starts a line of its own.
        let _ = file.set_len(before);
        return Err(e);
    }
    if sync {
        file.sync_data()?;
    }
    Ok(before + line.len() as u64)
}

/// Makes `dir` and its parents; on POSIX, `dir` itself 0700.
fn private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(dir)?.permissions().mode() & 0o777 != 0o700 {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test's own folder, removed when the test ends, passed or failed.
    struct Temp(PathBuf);

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A test's folder, and the log in it.
    fn temp(name: &str) -> (Temp, PathBuf) {
        let dir = std::env::temp_dir().join(format!("rlxd-store-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let log = dir.join("events.jsonl");
        (Temp(dir), log)
    }

    fn line(run_seq: u64, pad: usize) -> String {
        format!(
            "{{\"type\":\"run.text\",\"data\":{{\"run_seq\":{run_seq},\"pad\":\"{}\"}}}}\n",
            "x".repeat(pad)
        )
    }

    #[test]
    fn an_empty_or_missing_log_has_no_tail() {
        let (_temp, path) = temp("empty");
        assert!(tail(&path).unwrap().last_line.is_none(), "missing");
        fs::write(&path, "").unwrap();
        let empty = tail(&path).unwrap();
        assert!(empty.last_line.is_none());
        assert_eq!(empty.last_run_seq, 0);
        repair(&path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"");
    }

    #[test]
    fn the_tail_is_the_last_whole_line_even_past_a_chunk() {
        let (_temp, path) = temp("long");
        // A last line longer than one 64 KiB chunk, after a short one.
        let text = line(1, 10) + &line(2, 200_000);
        fs::write(&path, &text).unwrap();
        let found = tail(&path).unwrap();
        assert_eq!(found.last_run_seq, 2);
        assert_eq!(found.last_line.unwrap().len(), line(2, 200_000).len());
        // One line only.
        fs::write(&path, line(7, 5)).unwrap();
        assert_eq!(tail(&path).unwrap().last_run_seq, 7);
    }

    #[test]
    fn a_torn_last_line_is_no_tail_and_repair_cuts_it() {
        let (_temp, path) = temp("torn");
        let whole = line(1, 10) + &line(2, 70_000);
        let torn = whole.clone() + "{\"type\":\"run.done\",\"data\":{\"run_seq\":3";
        fs::write(&path, &torn).unwrap();
        assert_eq!(
            tail(&path).unwrap().last_run_seq,
            2,
            "a torn line is not one"
        );
        repair(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), whole);
        repair(&path).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            whole,
            "a whole log is left as it is"
        );
        // A log that is all torn.
        fs::write(&path, "{\"type\":").unwrap();
        repair(&path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"");
    }

    #[test]
    fn the_reader_gives_each_whole_line_once_and_says_when_the_log_is_gone() {
        let (_temp, path) = temp("reader");
        let mut reader = Reader::new(path.clone());
        assert!(reader.read(u64::MAX).unwrap().is_empty());
        assert!(reader.gone());
        fs::write(&path, line(1, 3) + "{\"partial").unwrap();
        assert_eq!(reader.read(u64::MAX).unwrap().len(), 1);
        assert!(!reader.gone());
        assert!(
            reader.read(u64::MAX).unwrap().is_empty(),
            "the partial line waits"
        );
        let mut rest = fs::OpenOptions::new().append(true).open(&path).unwrap();
        rest.write_all(b"\":1}\n").unwrap();
        assert_eq!(
            reader.read(u64::MAX).unwrap(),
            vec![b"{\"partial\":1}\n".to_vec()]
        );
    }

    #[test]
    fn a_reader_drops_what_it_read_of_an_append_cut_back() {
        let (_temp, path) = temp("cut");
        let mut reader = Reader::new(path.clone());
        fs::write(&path, line(1, 3) + "{\"type\":\"run.te").unwrap();
        assert_eq!(
            reader.read(u64::MAX).unwrap(),
            vec![line(1, 3).into_bytes()]
        );
        // The append failed part way and was cut back off; then the next one came.
        let log = fs::OpenOptions::new().write(true).open(&path).unwrap();
        log.set_len(line(1, 3).len() as u64).unwrap();
        assert!(reader.read(u64::MAX).unwrap().is_empty());
        let mut log = fs::OpenOptions::new().append(true).open(&path).unwrap();
        log.write_all(line(2, 3).as_bytes()).unwrap();
        assert_eq!(
            reader.read(u64::MAX).unwrap(),
            vec![line(2, 3).into_bytes()]
        );
    }

    #[test]
    fn a_follower_reads_only_what_appends_that_succeeded_wrote() {
        let (_temp, path) = temp("follow");
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let mut reader = Reader::new(path.clone());
        let first = append(&mut log, line(1, 3).as_bytes(), false).unwrap();
        // An append fails part way: what it wrote is there until it is cut back, past the
        // end of the last append that succeeded, which is where the follower stops.
        log.write_all(b"{\"type\":\"run.te").unwrap();
        assert_eq!(reader.read(first).unwrap(), vec![line(1, 3).into_bytes()]);
        let cut = fs::OpenOptions::new().write(true).open(&path).unwrap();
        cut.set_len(first).unwrap();
        // The next append is longer than what failed, and comes before the next read.
        let next = line(2, 40);
        let second = append(&mut log, next.as_bytes(), false).unwrap();
        assert_eq!(second, first + next.len() as u64);
        assert_eq!(reader.read(second).unwrap(), vec![next.into_bytes()]);
        assert!(reader.read(second).unwrap().is_empty());
    }
}
