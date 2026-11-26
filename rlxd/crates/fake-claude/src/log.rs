//! The fixture log, `$AISQUARE_HOME/fake-claude.log.json`: one entry per start, kept
//! current while the start runs.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// This start's entry in the home's fixture log.
#[derive(Clone, Debug)]
pub struct Log {
    path: PathBuf,
    lock: PathBuf,
    /// The clock's origin, in milliseconds since the Unix epoch.
    origin: u128,
    /// This start's place in `starts`.
    index: usize,
}

impl Log {
    /// Adds this start, run as `argv`, to the log of the home `AISQUARE_HOME` names:
    /// `None` when it names none, or the log can't be written.
    pub(crate) fn open(argv: &[String]) -> Option<Self> {
        let home = PathBuf::from(std::env::var_os("AISQUARE_HOME").filter(|v| !v.is_empty())?);
        let path = home.join("fake-claude.log.json");
        let lock = home.join("fake-claude.log.lock");
        let clock = home.join("fake-claude.clock");
        let held = locked(&lock).ok()?;
        let origin = origin(&clock).ok()?;
        let mut log = read(&path);
        let starts = log["starts"].as_array_mut()?;
        starts.push(json!({
            "argv": super::printable(argv),
            "env": super::recorded_env(),
            "stdin": [],
            "stdin_eof_at": null,
            "connected_at": null,
            "delivered": false,
            "hook_replies": [],
            "tool_runs": 0,
            "exit": null,
        }));
        let index = starts.len() - 1;
        write(&path, &log).ok()?;
        drop(held);
        Some(Self {
            path,
            lock,
            origin,
            index,
        })
    }

    /// Milliseconds since the clock's origin, the first start in this home.
    pub fn now(&self) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default();
        u64::try_from(now.saturating_sub(self.origin)).unwrap_or(u64::MAX)
    }

    /// Changes this start's entry with `change`, under the log's lock. A failure is
    /// said on stderr: the scenario goes on.
    pub fn update(&self, change: impl FnOnce(&mut Value)) {
        let result = locked(&self.lock).and_then(|held| {
            let mut log = read(&self.path);
            if let Some(start) = log["starts"].get_mut(self.index) {
                change(start);
            }
            let written = write(&self.path, &log);
            drop(held);
            written
        });
        if let Err(e) = result {
            eprintln!("fake-claude: cannot update {}: {e}", self.path.display());
        }
    }
}

/// The lock file, held: released when the file is dropped.
fn locked(path: &PathBuf) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    file.lock()?;
    Ok(file)
}

/// The clock's origin: the time in `path`, or now, written there, for the first start.
/// Called under the log's lock, so one start writes it and the others read it whole.
fn origin(path: &PathBuf) -> io::Result<u128> {
    if let Ok(text) = fs::read_to_string(path)
        && let Ok(origin) = text.trim().parse()
    {
        return Ok(origin);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    fs::write(path, format!("{now}\n"))?;
    Ok(now)
}

/// The log, or a new one when there is none yet.
fn read(path: &PathBuf) -> Value {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .filter(|log| log["starts"].is_array())
        .unwrap_or_else(|| json!({"starts": []}))
}

/// Replaces the log whole: a sibling, then a rename. Windows refuses a rename over a
/// file a reader has open for a moment, so it is tried again.
fn write(path: &PathBuf, log: &Value) -> io::Result<()> {
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, format!("{log:#}\n"))?;
    let mut tries = 0;
    loop {
        match fs::rename(&temporary, path) {
            Ok(()) => return Ok(()),
            Err(e) if tries < 50 && e.kind() == io::ErrorKind::PermissionDenied => {
                tries += 1;
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e),
        }
    }
}
