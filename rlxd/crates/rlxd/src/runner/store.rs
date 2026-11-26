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
use std::io::{self, Write};
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
        options.open(self.dir.join("events.jsonl"))
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
