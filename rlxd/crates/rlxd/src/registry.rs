//! rlxd's record in the daemon registry, `<home>/daemons/` (handoff 0.10.3, 4.1): what an
//! upgrade reads to count rlxd, stop it and start it again (4.2).
//!
//! The format is Python's, `aisquare.core.registry` and `aisquare.core.daemons`, whose
//! docstrings say it precisely; this is its writer in Rust:
//!
//! - **The folder** is 0700 and every file in it 0600. rlxd makes it, and narrows a wider
//!   one.
//! - **Registering**, under `.gate.lock` (waited on for 2 s): a random 12-hex-digit id,
//!   `<id>.lock` created and locked (retried for 1 s, as a reader may hold it for an
//!   instant), then `<id>.json` written. rlxd reads none of the others: one rlxd per home
//!   is `run/rlxd.lock`'s to enforce, and pruning is the readers'. When the gate cannot
//!   be had, nothing is written and rlxd runs unregistered, trying again at each beat.
//! - **The record** is [`DaemonRecord`], `kind: "rlxd"`, written whole: `<id>.json.tmp`,
//!   fsynced, renamed over `<id>.json`. rlxd also registers the brain it supervises
//!   (`kind: "brain"`, [`Registration::register_process`]), holding that record's lock
//!   itself: see `crate::brain`.
//! - **The heartbeat**, every 5 s, rewrites it with a new `heartbeat_at`. First it checks
//!   that `<id>.lock` is still the file it holds (POSIX: the same device and inode), and
//!   takes it again under the gate when someone deleted or replaced it. On Windows a held
//!   lock file cannot be deleted, as it is opened without sharing delete.
//! - **Leaving**, on a clean stop: `<id>.json` deleted, the lock let go, `<id>.lock`
//!   deleted. After a kill the lock is free, so the record reads as dead and the next
//!   reader prunes it.
//!
//! On Windows a replace or a delete that another handle refuses for a moment
//! (`ERROR_ACCESS_DENIED`, `ERROR_SHARING_VIOLATION`) is tried again for 1 s.
//!
//! [`read`] is the other side: every record in the folder, and whether it is live by
//! `core.daemons`' rule, the lock alone. It deletes nothing; pruning is Python's.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use rlxd_client::Home;
use rlxd_proto::{DaemonRecord, VERSION, daemon_kinds};

use crate::lock::{self, Held};
use crate::{clock, host};

/// How often the record is rewritten.
pub const HEARTBEAT: Duration = Duration::from_secs(5);

/// The folder's gate, held while a process writes its record.
const GATE_LOCK: &str = ".gate.lock";

/// How long registering waits for the gate.
const GATE_WAIT: Duration = Duration::from_secs(2);

/// How long a new `<id>.lock` is retried.
const LOCK_WAIT: Duration = Duration::from_secs(1);

/// How long a refused replace or delete is retried, on Windows.
const RETRY: Duration = Duration::from_secs(1);

/// rlxd, registered: its record and the lock that says it is live.
#[derive(Debug)]
pub struct Registration {
    dir: PathBuf,
    record: DaemonRecord,
    lock: Held,
}

impl Registration {
    /// Registers this rlxd in `home`'s registry. Its record's `argv`, which restarts it,
    /// is `rlxd serve --detach` plus `flags`, the flags it was started with.
    pub fn register(home: &Home, flags: &[&str]) -> io::Result<Self> {
        let exe = std::env::current_exe()?;
        let mut argv = vec![
            exe.display().to_string(),
            "serve".to_owned(),
            "--detach".to_owned(),
        ];
        argv.extend(flags.iter().map(|&flag| flag.to_owned()));
        let cwd = std::env::current_dir()
            .ok()
            .map(|cwd| cwd.display().to_string());
        Self::register_process(
            home,
            daemon_kinds::RLXD,
            std::process::id(),
            argv,
            true,
            cwd,
        )
    }

    /// Registers another process in `home`'s registry, which this rlxd holds the lock of:
    /// a child it supervises, such as the brain (kind `brain`), whose pid is `pid`, run as
    /// `argv` in `cwd`. `restartable` says whether an upgrade may restart it from `argv`.
    pub fn register_process(
        home: &Home,
        kind: &str,
        pid: u32,
        argv: Vec<String>,
        restartable: bool,
        cwd: Option<String>,
    ) -> io::Result<Self> {
        let dir = home.daemons();
        crate::serve::private_dir(&dir)?;
        let _gate = gate(&dir)?;
        let id = fresh_id(&dir)?;
        let lock = take(&dir.join(format!("{id}.lock")))?;
        let now = clock::now();
        let mut record = DaemonRecord::new(
            id,
            kind,
            pid,
            host::name(),
            VERSION,
            now.clone(),
            now,
            argv,
            restartable,
        )
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if let Some(cwd) = cwd {
            record = record.with_cwd(cwd);
        }
        let registration = Self { dir, record, lock };
        if let Err(error) = registration.write() {
            registration.leave();
            return Err(error);
        }
        Ok(registration)
    }

    /// The record's id.
    pub fn id(&self) -> &str {
        &self.record.id
    }

    /// Rewrites the record with a new `heartbeat_at`, taking the lock again first when it
    /// was lost.
    pub fn beat(&mut self) -> io::Result<()> {
        if !self.still_holds_lock() {
            self.take_lock_again()?;
        }
        self.record.heartbeat_at = clock::now();
        self.write()
    }

    /// Leaves the registry: the record deleted, the lock let go, its file deleted.
    pub fn leave(self) {
        let record = self.path("json");
        let lock = self.path("lock");
        let _ = remove(&record);
        let _ = remove(&self.path("json.tmp"));
        drop(self.lock);
        let _ = remove(&lock);
    }

    fn path(&self, extension: &str) -> PathBuf {
        self.dir.join(format!("{}.{extension}", self.record.id))
    }

    /// Writes the record whole: a sibling written and fsynced, then renamed over it.
    fn write(&self) -> io::Result<()> {
        let mut text = serde_json::to_vec(&self.record)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        text.push(b'\n');
        let temporary = self.path("json.tmp");
        let mut file = private_file(&temporary)?;
        file.write_all(&text)?;
        file.sync_all()?;
        drop(file);
        retry_refused(|| fs::rename(&temporary, self.path("json")))
    }

    #[cfg(unix)]
    fn still_holds_lock(&self) -> bool {
        use std::os::unix::fs::MetadataExt;
        match (self.lock.file().metadata(), fs::metadata(self.path("lock"))) {
            (Ok(held), Ok(there)) => held.dev() == there.dev() && held.ino() == there.ino(),
            _ => false,
        }
    }

    /// No one can delete or replace a held lock file on Windows, which rlxd opens without
    /// sharing delete.
    #[cfg(not(unix))]
    fn still_holds_lock(&self) -> bool {
        true
    }

    /// Takes `<id>.lock` again, under the gate. Without the gate, keeps the one it has
    /// and tries again at the next beat.
    fn take_lock_again(&mut self) -> io::Result<()> {
        let Ok(_gate) = gate(&self.dir) else {
            return Ok(());
        };
        self.lock = take(&self.path("lock"))?;
        Ok(())
    }
}

/// The folder's gate, held for the caller's block.
fn gate(dir: &Path) -> io::Result<Held> {
    lock::acquire(&dir.join(GATE_LOCK), GATE_WAIT)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("{} stayed held for {} s", GATE_LOCK, GATE_WAIT.as_secs()),
        )
    })
}

/// The lock file at `path`, taken.
fn take(path: &Path) -> io::Result<Held> {
    lock::acquire(path, LOCK_WAIT)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("{} is held by another process", path.display()),
        )
    })
}

/// A random id no file in `dir` has yet.
fn fresh_id(dir: &Path) -> io::Result<String> {
    loop {
        let mut bytes = [0u8; 6];
        getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
        let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let taken = ["json", "lock"]
            .iter()
            .any(|ext| dir.join(format!("{id}.{ext}")).exists());
        if !taken {
            return Ok(id);
        }
    }
}

/// `path`, opened to be written from empty, and 0600 on POSIX however it was before: the
/// mode `open` sets applies only to a file it creates, so a stale one left 0644 is
/// narrowed here, before anything is written to it.
pub(crate) fn private_file(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if file.metadata()?.permissions().mode() & 0o777 != 0o600 {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(file)
}

/// Deletes `path`; a file already gone is fine.
fn remove(path: &Path) -> io::Result<()> {
    match retry_refused(|| fs::remove_file(path)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Runs `step`, again for up to [`RETRY`] while Windows refuses it because another handle
/// has the file open.
pub(crate) fn retry_refused(mut step: impl FnMut() -> io::Result<()>) -> io::Result<()> {
    /// ERROR_ACCESS_DENIED and ERROR_SHARING_VIOLATION.
    const REFUSED: [i32; 2] = [5, 32];
    let deadline = Instant::now() + RETRY;
    loop {
        match step() {
            Err(error)
                if cfg!(windows)
                    && error
                        .raw_os_error()
                        .is_some_and(|code| REFUSED.contains(&code))
                    && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(50));
            }
            other => return other,
        }
    }
}

/// What [`read`] found in the folder.
#[derive(Debug, Default)]
pub struct Roll {
    /// Records whose lock is held: their processes run.
    pub live: Vec<DaemonRecord>,
    /// Records whose lock is free or missing: their processes are gone.
    pub dead: Vec<DaemonRecord>,
    /// Records whose lock could not be probed at all, such as on a mount with no lock
    /// manager. `core.daemons` then asks the pid or the heartbeat; rlxd does not judge.
    pub unknown: Vec<DaemonRecord>,
    /// Files named `<id>.json` that are not a record this release reads: not JSON, a
    /// field missing or wrong, or another id inside. Reported, never dropped.
    pub unreadable: Vec<PathBuf>,
}

/// Every record in `home`'s registry, by liveness, as `core.daemons` reads them: a record
/// is live while its `<id>.lock` is held, whatever host it names. Missing folder, no
/// records. A record whose read fails is read once more 50 ms later, as Python's are.
pub fn read(home: &Home) -> io::Result<Roll> {
    let dir = home.daemons();
    let mut roll = Roll::default();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(roll),
        Err(error) => return Err(error),
    };
    let mut ids: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .filter_map(|name| name.strip_suffix(".json").map(str::to_owned))
        .filter(|id| valid_id(id))
        .collect();
    ids.sort();
    for id in ids {
        let path = dir.join(format!("{id}.json"));
        let text = match fs::read_to_string(&path).or_else(|_| {
            thread::sleep(Duration::from_millis(50));
            fs::read_to_string(&path)
        }) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => {
                roll.unreadable.push(path);
                continue;
            }
        };
        let record = match serde_json::from_str::<DaemonRecord>(&text) {
            Ok(record) if record.id == id => record,
            _ => {
                roll.unreadable.push(path);
                continue;
            }
        };
        match lock::is_held(&dir.join(format!("{id}.lock"))) {
            Ok(true) => roll.live.push(record),
            Ok(false) => roll.dead.push(record),
            Err(_) => roll.unknown.push(record),
        }
    }
    Ok(roll)
}

/// Whether `text` is a record id: twelve lower-case hex digits, so a path built from it
/// stays a file in the folder.
fn valid_id(text: &str) -> bool {
    text.len() == 12
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
