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
    /// The call ledger: one entry per gated call (5.1).
    #[serde(default)]
    pub calls: Vec<Value>,
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

/// Cuts a last line that a crash left without its newline from the log at `path`, so the
/// next event starts a line of its own.
pub fn repair(path: &Path) -> io::Result<()> {
    let mut file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(());
    }
    let mut last = [0u8; 1];
    file.seek(SeekFrom::Start(len - 1))?;
    file.read_exact(&mut last)?;
    if last[0] == b'\n' {
        return Ok(());
    }
    let mut text = Vec::new();
    file.seek(SeekFrom::Start(0))?;
    file.read_to_end(&mut text)?;
    let keep = text
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |at| at + 1);
    file.set_len(keep as u64)?;
    file.sync_all()
}

/// The text of `attempt`'s whole assistant messages in the log at `path`: what a run in
/// flight when rlxd stopped had got out.
pub fn attempt_text(path: &Path, attempt: u64) -> String {
    let Ok(text) = fs::read(path) else {
        return String::new();
    };
    let mut said = String::new();
    for line in text.split(|&b| b == b'\n') {
        let Ok(event) = serde_json::from_slice::<Value>(line) else {
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
}

impl Reader {
    /// A reader of the log at `path` from its start.
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            partial: Vec::new(),
        }
    }

    /// The whole lines written since the last read, each with its newline: a few MiB of
    /// them at a time, so a long log is sent in turns. None once it has read every whole
    /// line written so far.
    pub fn read(&mut self) -> io::Result<Vec<Vec<u8>>> {
        /// How much is read at a time.
        const CHUNK: u64 = 4 << 20;
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        file.seek(SeekFrom::Start(self.offset))?;
        let end = loop {
            let mut fresh = Vec::new();
            let read = (&mut file).take(CHUNK).read_to_end(&mut fresh)?;
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

/// Appends `line`, one event and its newline, in one write, then fsyncs it when `sync`.
pub fn append(file: &mut File, line: &[u8], sync: bool) -> io::Result<()> {
    file.write_all(line)?;
    if sync {
        file.sync_data()?;
    }
    Ok(())
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
