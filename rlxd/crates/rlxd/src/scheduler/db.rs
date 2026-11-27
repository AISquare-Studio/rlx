//! The scheduler's store, `<home>/rlxd/rlxd.db` (handoff 0.10.3, 2.3): SQLite, built in.
//!
//! - **One writer:** one connection, behind a lock, used only as blocking work. WAL
//!   journal, `synchronous = FULL`, so a write that returned is on disk.
//! - **Migrations** are numbered steps in [`MIGRATIONS`], each applied in a transaction of
//!   its own and recorded in `schema_migrations`.
//! - **Private:** the folder is 0700 and the database (whose jobs hold prompts) 0600 on
//!   macOS and Linux; SQLite gives its `-wal` and `-shm` files the database's mode.
//! - **Never silently reset:** a database SQLite says is not a database or is corrupt,
//!   that fails its quick check, or that a newer rlxd wrote is moved aside to
//!   `rlxd.db.unreadable-<time>` (its `-wal` and `-shm` with it), logged, and reported in
//!   `status`; a fresh one is begun. Any other failure (busy, locked, a full disk, no
//!   access) is tried again a few times, then the scheduler doesn't start, `status` says
//!   why, and the file is left as it is.
//! - **A fire is begun before its run starts:** the job, its schedule moved past the fire,
//!   and a `starting` record of the fire, with its run's id, in one transaction
//!   ([`Db::begin_fire`]). So a crash between can't fire one scheduled time twice, and the
//!   next rlxd settles each `starting` fire by whether its run was made.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{Connection, ErrorCode, OptionalExtension, params};

use crate::log::error;

/// The schema, step by step: step `n` takes the database from version `n - 1` to `n`.
const MIGRATIONS: &[&str] = &[
    // 1: jobs, and the record of what each fire did.
    "CREATE TABLE jobs (
        job_id TEXT PRIMARY KEY NOT NULL,
        type TEXT NOT NULL CHECK (type IN ('run', 'watch')),
        definition TEXT NOT NULL,
        anchor_at TEXT NOT NULL,
        next_fire_at TEXT,
        last_scheduled_for TEXT,
        paused INTEGER NOT NULL DEFAULT 0,
        owed TEXT,
        held_until TEXT,
        last_fire_at TEXT,
        last_run_id TEXT,
        last_state TEXT,
        skipped_count INTEGER NOT NULL DEFAULT 0,
        put_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );
    CREATE TABLE fires (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        job_id TEXT NOT NULL,
        scheduled_for TEXT NOT NULL,
        at TEXT NOT NULL,
        outcome TEXT NOT NULL CHECK (outcome IN ('starting', 'fired', 'skipped')),
        reason TEXT,
        code TEXT,
        run_id TEXT,
        cause TEXT
    );
    CREATE INDEX fires_by_job ON fires (job_id, id);",
];

/// How many fire records a job keeps, the latest.
const FIRES_KEPT: i64 = 1000;

/// How many times a database is tried before the scheduler gives up on it.
const TRIES: u32 = 3;

/// One job as the store holds it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    pub job_id: String,
    pub r#type: String,
    /// The `job.put` params, as JSON.
    pub definition: String,
    pub anchor_at: String,
    pub next_fire_at: Option<String>,
    pub last_scheduled_for: Option<String>,
    pub paused: bool,
    pub owed: Option<String>,
    pub held_until: Option<String>,
    pub last_fire_at: Option<String>,
    pub last_run_id: Option<String>,
    pub last_state: Option<String>,
    pub skipped_count: u64,
    pub put_at: String,
    pub updated_at: String,
}

/// One fire's record.
pub struct Fire<'a> {
    pub job_id: &'a str,
    pub scheduled_for: &'a str,
    pub at: &'a str,
    /// `starting` (its run is being started), `fired` or `skipped`.
    pub outcome: &'a str,
    pub reason: Option<&'a str>,
    /// For `start_failed`: the code the start was refused with.
    pub code: Option<&'a str>,
    pub run_id: Option<&'a str>,
    pub cause: Option<&'a str>,
}

/// A fire whose run was being started when rlxd stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Starting {
    pub id: i64,
    pub job_id: String,
    pub scheduled_for: String,
    pub at: String,
    pub run_id: String,
    pub cause: Option<String>,
}

/// The open store.
pub struct Db {
    connection: Mutex<Connection>,
}

/// The store, and what was moved aside to begin it, when anything was.
pub struct Opened {
    pub db: Db,
    pub set_aside: Option<String>,
}

/// Why a database couldn't be opened.
enum Failure {
    /// It is not one SQLite can read, or a newer rlxd's: set it aside.
    Unreadable(String),
    /// Anything else (busy, locked, a full disk, no access): try again, then give up.
    Other(String),
}

impl Failure {
    /// `error`, sorted: SQLite's "not a database" and "corrupt" make a database
    /// unreadable; the rest passes, or is the machine's.
    fn of(error: &rusqlite::Error) -> Self {
        match error.sqlite_error_code() {
            Some(ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt) => {
                Self::Unreadable(error.to_string())
            }
            _ => Self::Other(error.to_string()),
        }
    }
}

/// Opens `dir`'s `rlxd.db`, made private, migrated to this rlxd's schema. One it can't
/// read is moved aside and a fresh one begun; one it can't open for another reason, after
/// [`TRIES`] tries, is an error, and the file is left as it is.
pub fn open(dir: &Path) -> Result<Opened, String> {
    private_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join("rlxd.db");
    let (connection, aside) = match connect_trying(&path)? {
        Ok(connection) => (connection, None),
        Err(why) => {
            let aside = set_aside(&path, &why)?;
            let connection = connect_trying(&path)?.map_err(|why| {
                format!(
                    "{}: a fresh database couldn't be made: {why}",
                    path.display()
                )
            })?;
            (connection, Some(aside))
        }
    };
    Ok(Opened {
        db: Db {
            connection: Mutex::new(connection),
        },
        set_aside: aside,
    })
}

/// [`connect`], tried [`TRIES`] times while it fails for a passing reason: the
/// connection, or why the database is unreadable (`Ok(Err)`), or why it couldn't be had
/// (`Err`: the scheduler doesn't start).
fn connect_trying(path: &Path) -> Result<Result<Connection, String>, String> {
    let mut last = String::new();
    for attempt in 1..=TRIES {
        match connect(path) {
            Ok(connection) => return Ok(Ok(connection)),
            Err(Failure::Unreadable(why)) => return Ok(Err(why)),
            Err(Failure::Other(why)) => {
                last = why;
                if attempt < TRIES {
                    std::thread::sleep(Duration::from_millis(300));
                }
            }
        }
    }
    Err(format!(
        "{} could not be opened ({last}), {TRIES} tries; it was left as it is",
        path.display()
    ))
}

/// A connection to `path`, checked and migrated: why not, when it can't be had.
fn connect(path: &Path) -> Result<Connection, Failure> {
    let sqlite = |e: rusqlite::Error| Failure::of(&e);
    private_file(path).map_err(|e| Failure::Other(e.to_string()))?;
    let connection = Connection::open(path).map_err(sqlite)?;
    connection
        .busy_timeout(Duration::from_secs(2))
        .map_err(sqlite)?;
    let mode: String = connection
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .map_err(sqlite)?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(Failure::Other(format!(
            "its journal mode is {mode}, not wal"
        )));
    }
    connection
        .execute_batch("PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;")
        .map_err(sqlite)?;
    let check: String = connection
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(sqlite)?;
    if check != "ok" {
        return Err(Failure::Unreadable(format!(
            "SQLite's quick check says {check:?}"
        )));
    }
    migrate(&connection)?;
    for side in ["-wal", "-shm"] {
        let mut name = path.as_os_str().to_owned();
        name.push(side);
        let side = PathBuf::from(name);
        if side.exists() {
            private_file(&side).map_err(|e| Failure::Other(e.to_string()))?;
        }
    }
    Ok(connection)
}

/// Brings the schema up to [`MIGRATIONS`]' last step, one transaction a step.
fn migrate(connection: &Connection) -> Result<(), Failure> {
    let sqlite = |e: rusqlite::Error| Failure::of(&e);
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY NOT NULL,
                applied_at TEXT NOT NULL
            );",
        )
        .map_err(sqlite)?;
    let at: i64 = connection
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )
        .map_err(sqlite)?;
    let known = i64::try_from(MIGRATIONS.len()).unwrap_or(i64::MAX);
    if at > known {
        return Err(Failure::Unreadable(format!(
            "its schema is version {at}, newer than this rlxd's {known}"
        )));
    }
    for (step, sql) in MIGRATIONS
        .iter()
        .enumerate()
        .skip(usize::try_from(at).unwrap_or(0))
    {
        let version = i64::try_from(step + 1).unwrap_or(i64::MAX);
        let apply = || -> rusqlite::Result<()> {
            connection.execute_batch("BEGIN IMMEDIATE")?;
            connection.execute_batch(sql)?;
            connection.execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                params![version, crate::clock::now()],
            )?;
            connection.execute_batch("COMMIT")
        };
        if let Err(e) = apply() {
            let _ = connection.execute_batch("ROLLBACK");
            return Err(match Failure::of(&e) {
                Failure::Unreadable(why) => Failure::Unreadable(why),
                Failure::Other(why) => Failure::Other(format!("schema step {version}: {why}")),
            });
        }
    }
    Ok(())
}

/// Moves `path` and its `-wal` and `-shm` aside, for `why`: where it went, and why, for
/// `status`.
fn set_aside(path: &Path, why: &str) -> Result<String, String> {
    let stamp: String = crate::clock::now()
        .chars()
        .filter(|c| *c != '-' && *c != ':')
        .collect();
    let base = crate::durable::sibling(path, &format!("unreadable-{stamp}"));
    let mut aside = base.clone();
    let mut n = 1;
    while aside.exists() {
        aside = crate::durable::sibling(&base, &n.to_string());
        n += 1;
    }
    if path.exists() {
        fs::rename(path, &aside).map_err(|e| {
            format!(
                "{} can't be read ({why}), nor moved aside: {e}",
                path.display()
            )
        })?;
    }
    for side in ["-wal", "-shm"] {
        let mut from = path.as_os_str().to_owned();
        from.push(side);
        let mut to = aside.as_os_str().to_owned();
        to.push(side);
        let _ = fs::rename(PathBuf::from(from), PathBuf::from(to));
    }
    let said = format!("{} ({why})", aside.display());
    error!(
        "scheduler: {} can't be read ({why}); it was moved to {}, and a fresh one begun",
        path.display(),
        aside.display()
    );
    Ok(said)
}

impl Db {
    fn connection(&self) -> MutexGuard<'_, Connection> {
        self.connection.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Every job.
    pub fn jobs(&self) -> Result<Vec<Row>, String> {
        let connection = self.connection();
        let mut statement = connection
            .prepare(
                "SELECT job_id, type, definition, anchor_at, next_fire_at, last_scheduled_for,
                    paused, owed, held_until, last_fire_at, last_run_id, last_state,
                    skipped_count, put_at, updated_at
                 FROM jobs ORDER BY job_id",
            )
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok(Row {
                    job_id: row.get(0)?,
                    r#type: row.get(1)?,
                    definition: row.get(2)?,
                    anchor_at: row.get(3)?,
                    next_fire_at: row.get(4)?,
                    last_scheduled_for: row.get(5)?,
                    paused: row.get::<_, i64>(6)? != 0,
                    owed: row.get(7)?,
                    held_until: row.get(8)?,
                    last_fire_at: row.get(9)?,
                    last_run_id: row.get(10)?,
                    last_state: row.get(11)?,
                    skipped_count: u64::try_from(row.get::<_, i64>(12)?).unwrap_or(0),
                    put_at: row.get(13)?,
                    updated_at: row.get(14)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    /// Writes `row`, a new job or one replaced.
    pub fn save(&self, row: &Row) -> Result<(), String> {
        save(&self.connection(), row)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Deletes job `job_id` and its fire records: whether there was one.
    pub fn delete(&self, job_id: &str) -> Result<bool, String> {
        let connection = self.connection();
        connection
            .execute("DELETE FROM fires WHERE job_id = ?1", params![job_id])
            .map_err(|e| e.to_string())?;
        connection
            .execute("DELETE FROM jobs WHERE job_id = ?1", params![job_id])
            .map(|n| n > 0)
            .map_err(|e| e.to_string())
    }

    /// Records a fire, keeping the job's latest [`FIRES_KEPT`].
    pub fn record(&self, fire: &Fire<'_>) -> Result<(), String> {
        let connection = self.connection();
        insert(&connection, fire).map_err(|e| e.to_string())?;
        trim(&connection, fire.job_id)
    }

    /// Writes `row`, the job with its schedule moved past the fire, and records `fire`,
    /// `starting`, in one transaction, before the fire's run is started. The record's id,
    /// to settle it by ([`Db::settle`]).
    pub fn begin_fire(&self, row: &Row, fire: &Fire<'_>) -> Result<i64, String> {
        let connection = self.connection();
        let begin = || -> rusqlite::Result<i64> {
            connection.execute_batch("BEGIN IMMEDIATE")?;
            save(&connection, row)?;
            insert(&connection, fire)?;
            let id = connection.last_insert_rowid();
            connection.execute_batch("COMMIT")?;
            Ok(id)
        };
        match begin() {
            Ok(id) => {
                trim(&connection, fire.job_id)?;
                Ok(id)
            }
            Err(e) => {
                let _ = connection.execute_batch("ROLLBACK");
                Err(e.to_string())
            }
        }
    }

    /// Settles fire record `id`: `fired`, or `skipped` for `reason` with `code`.
    pub fn settle(
        &self,
        id: i64,
        outcome: &str,
        reason: Option<&str>,
        code: Option<&str>,
    ) -> Result<(), String> {
        self.connection()
            .execute(
                "UPDATE fires SET outcome = ?2, reason = ?3, code = ?4 WHERE id = ?1",
                params![id, outcome, reason, code],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// The fires whose runs were being started when rlxd stopped.
    pub fn starting(&self) -> Result<Vec<Starting>, String> {
        let connection = self.connection();
        let mut statement = connection
            .prepare(
                "SELECT id, job_id, scheduled_for, at, run_id, cause FROM fires
                 WHERE outcome = 'starting' AND run_id IS NOT NULL ORDER BY id",
            )
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok(Starting {
                    id: row.get(0)?,
                    job_id: row.get(1)?,
                    scheduled_for: row.get(2)?,
                    at: row.get(3)?,
                    run_id: row.get(4)?,
                    cause: row.get(5)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    /// How many fire records job `job_id` has: for tests.
    #[cfg(test)]
    pub fn fires(&self, job_id: &str) -> i64 {
        self.connection()
            .query_row(
                "SELECT COUNT(*) FROM fires WHERE job_id = ?1",
                params![job_id],
                |row| row.get(0),
            )
            .unwrap_or(-1)
    }
}

fn save(connection: &Connection, row: &Row) -> rusqlite::Result<usize> {
    connection.execute(
        "INSERT OR REPLACE INTO jobs (job_id, type, definition, anchor_at, next_fire_at,
            last_scheduled_for, paused, owed, held_until, last_fire_at, last_run_id,
            last_state, skipped_count, put_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            row.job_id,
            row.r#type,
            row.definition,
            row.anchor_at,
            row.next_fire_at,
            row.last_scheduled_for,
            i64::from(row.paused),
            row.owed,
            row.held_until,
            row.last_fire_at,
            row.last_run_id,
            row.last_state,
            i64::try_from(row.skipped_count).unwrap_or(i64::MAX),
            row.put_at,
            row.updated_at,
        ],
    )
}

fn insert(connection: &Connection, fire: &Fire<'_>) -> rusqlite::Result<usize> {
    connection.execute(
        "INSERT INTO fires (job_id, scheduled_for, at, outcome, reason, code, run_id, cause)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            fire.job_id,
            fire.scheduled_for,
            fire.at,
            fire.outcome,
            fire.reason,
            fire.code,
            fire.run_id,
            fire.cause
        ],
    )
}

/// Keeps job `job_id`'s latest [`FIRES_KEPT`] fire records.
fn trim(connection: &Connection, job_id: &str) -> Result<(), String> {
    let oldest_kept: Option<i64> = connection
        .query_row(
            "SELECT id FROM fires WHERE job_id = ?1 ORDER BY id DESC LIMIT 1 OFFSET ?2",
            params![job_id, FIRES_KEPT - 1],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    if let Some(oldest) = oldest_kept {
        connection
            .execute(
                "DELETE FROM fires WHERE job_id = ?1 AND id < ?2",
                params![job_id, oldest],
            )
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Makes `dir` and its parents; on POSIX, `dir` itself 0700.
fn private_dir(dir: &Path) -> std::io::Result<()> {
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

/// Makes `path` if it isn't there, and on POSIX 0600 either way, before SQLite opens it.
fn private_file(path: &Path) -> std::io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(false);
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
    drop(file);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test's own folder (and its `rlxd` inside), removed when the test ends.
    struct Temp(PathBuf);

    impl Temp {
        /// The `rlxd` folder the store goes in.
        fn rlxd(&self) -> PathBuf {
            self.0.join("rlxd")
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp(name: &str) -> Temp {
        let dir = std::env::temp_dir().join(format!("rlxd-db-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Temp(dir)
    }

    fn row(job_id: &str) -> Row {
        Row {
            job_id: job_id.to_owned(),
            r#type: "run".to_owned(),
            definition: "{}".to_owned(),
            anchor_at: "2027-01-01T00:00:00Z".to_owned(),
            put_at: "2027-01-01T00:00:00Z".to_owned(),
            updated_at: "2027-01-01T00:00:00Z".to_owned(),
            ..Row::default()
        }
    }

    fn fire<'a>(job_id: &'a str, outcome: &'a str, run_id: Option<&'a str>) -> Fire<'a> {
        Fire {
            job_id,
            scheduled_for: "2027-01-01T00:00:00Z",
            at: "2027-01-01T00:00:00Z",
            outcome,
            reason: None,
            code: None,
            run_id,
            cause: Some("due"),
        }
    }

    #[test]
    fn jobs_are_kept_replaced_and_deleted_across_opens() {
        let dir = temp("keep");
        let opened = open(&dir.rlxd()).unwrap();
        assert!(opened.set_aside.is_none());
        opened.db.save(&row("a")).unwrap();
        let mut b = row("b");
        b.paused = true;
        b.skipped_count = 3;
        opened.db.save(&b).unwrap();
        b.next_fire_at = Some("2027-01-01T00:05:00Z".to_owned());
        opened.db.save(&b).unwrap();
        drop(opened);
        let again = open(&dir.rlxd()).unwrap();
        let jobs = again.db.jobs().unwrap();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[1], b);
        assert!(again.db.delete("a").unwrap());
        assert!(!again.db.delete("a").unwrap());
        assert_eq!(again.db.jobs().unwrap().len(), 1);
    }

    #[test]
    fn a_fire_is_begun_with_its_job_and_settled_after() {
        let dir = temp("begin");
        let opened = open(&dir.rlxd()).unwrap();
        let mut moved = row("a");
        moved.next_fire_at = Some("2027-01-01T00:02:00Z".to_owned());
        let id = opened
            .db
            .begin_fire(&moved, &fire("a", "starting", Some("r1")))
            .unwrap();
        // As a crash here would leave them: the job moved on, the fire starting.
        drop(opened);
        let again = open(&dir.rlxd()).unwrap();
        assert_eq!(again.db.jobs().unwrap(), [moved]);
        let starting = again.db.starting().unwrap();
        assert_eq!(starting.len(), 1);
        assert_eq!((starting[0].id, starting[0].run_id.as_str()), (id, "r1"));
        again.db.settle(id, "fired", None, None).unwrap();
        assert!(again.db.starting().unwrap().is_empty());
    }

    #[test]
    fn a_job_keeps_its_latest_fire_records() {
        let dir = temp("fires");
        let opened = open(&dir.rlxd()).unwrap();
        for _ in 0..(FIRES_KEPT + 5) {
            opened.db.record(&fire("a", "skipped", None)).unwrap();
        }
        assert_eq!(opened.db.fires("a"), FIRES_KEPT);
    }

    #[test]
    fn a_corrupt_database_is_set_aside_and_a_fresh_one_begun() {
        let dir = temp("corrupt");
        fs::create_dir_all(dir.rlxd()).unwrap();
        let path = dir.rlxd().join("rlxd.db");
        fs::write(&path, "this is not a database, and never was").unwrap();
        let opened = open(&dir.rlxd()).unwrap();
        let said = opened.set_aside.expect("set aside");
        assert!(said.contains("rlxd.db.unreadable-"), "{said}");
        let aside: Vec<_> = fs::read_dir(dir.rlxd())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("unreadable-"))
            .collect();
        assert_eq!(aside.len(), 1);
        assert_eq!(
            fs::read_to_string(aside[0].path()).unwrap(),
            "this is not a database, and never was"
        );
        assert!(opened.db.jobs().unwrap().is_empty());
        opened.db.save(&row("fresh")).unwrap();
    }

    #[test]
    fn a_database_from_a_newer_rlxd_is_set_aside() {
        let dir = temp("newer");
        let opened = open(&dir.rlxd()).unwrap();
        opened
            .db
            .connection()
            .execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (99, 'x')",
                [],
            )
            .unwrap();
        drop(opened);
        let again = open(&dir.rlxd()).unwrap();
        assert!(again.set_aside.unwrap().contains("newer than this rlxd"));
    }

    #[test]
    fn a_busy_database_is_tried_again_then_left_as_it_is() {
        let dir = temp("busy");
        let opened = open(&dir.rlxd()).unwrap();
        opened.db.save(&row("kept")).unwrap();
        drop(opened);
        // Another connection holds it whole (exclusive locking keeps even WAL readers
        // out), past every try.
        let holder = Connection::open(dir.rlxd().join("rlxd.db")).unwrap();
        holder
            .execute_batch(
                "PRAGMA locking_mode = EXCLUSIVE; BEGIN EXCLUSIVE; \
                 UPDATE jobs SET paused = paused;",
            )
            .unwrap();
        let refused = open(&dir.rlxd()).err().expect("busy");
        assert!(refused.contains("left as it is"), "{refused}");
        holder.execute_batch("COMMIT").unwrap();
        drop(holder);
        // Nothing was moved; once free, it opens with its job.
        let again = open(&dir.rlxd()).unwrap();
        assert!(again.set_aside.is_none());
        assert_eq!(again.db.jobs().unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    #[allow(unsafe_code)]
    fn a_database_it_may_not_open_is_left_as_it_is() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid(2) can't fail.
        if unsafe { libc::geteuid() } == 0 {
            return; // root opens anything
        }
        let dir = temp("denied");
        let opened = open(&dir.rlxd()).unwrap();
        opened.db.save(&row("kept")).unwrap();
        drop(opened);
        let path = dir.rlxd().join("rlxd.db");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let refused = open(&dir.rlxd()).err().expect("denied");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(refused.contains("left as it is"), "{refused}");
        let again = open(&dir.rlxd()).unwrap();
        assert!(again.set_aside.is_none());
        assert_eq!(again.db.jobs().unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn the_database_and_its_wal_and_shm_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp("modes");
        let opened = open(&dir.rlxd()).unwrap();
        opened.db.save(&row("a")).unwrap();
        let mode = |path: PathBuf| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(dir.rlxd()), 0o700, "the rlxd folder");
        for name in ["rlxd.db", "rlxd.db-wal", "rlxd.db-shm"] {
            assert_eq!(mode(dir.rlxd().join(name)), 0o600, "{name}");
        }
    }
}
