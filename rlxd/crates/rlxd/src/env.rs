//! The app's environment for Claude Code (handoff 0.10.3, 4.3): `PATH`,
//! `CLAUDE_CONFIG_DIR` and `CLAUDE_SECURESTORAGE_CONFIG_DIR`, set by the app through
//! `env.set` or its `hello`, kept in `<home>/rlxd/env.json` (0600) so an rlxd started at
//! login has them too, and passed raw to the brain and every Claude Code rlxd starts.
//!
//! - **Writing.** A change is checked whole first ([`EnvValues::from_object`]), then the
//!   file is written whole and durably (a 0600 sibling, fsynced, renamed over it), and
//!   only then do the values in memory change. A refused or unwritten change changes
//!   nothing.
//! - **`claude`.** It is resolved on the app's `PATH` ([`claude::resolve_on`]) when a
//!   change is taken in and when rlxd starts, and kept: a run starts that absolute path,
//!   never one looked up at its start.
//! - **An unreadable file** (not JSON, a key this rlxd doesn't know, a value too long) is
//!   moved aside to `env.json.unreadable-<time>`, logged, and rlxd starts with no values,
//!   as the settings do. One that can't be moved aside either freezes the values: every
//!   change is refused with why, until the file is fixed or moved and rlxd restarted.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rlxd_client::Home;
use rlxd_proto::{EnvChange, EnvValues};

use crate::claude::{self, Resolved};
use crate::log::{error, info};
use crate::{clock, durable};

/// The file's name, in `<home>/rlxd/`.
pub const FILE: &str = "env.json";

/// The app's environment, and the `claude` it resolves to.
pub struct Env {
    path: PathBuf,
    state: Mutex<State>,
    /// Why every change is refused: the file could not be read at start, nor moved aside.
    frozen: Option<String>,
}

struct State {
    values: EnvValues,
    claude: Resolved,
}

/// Why a change was not made. Nothing changed.
#[derive(Debug)]
pub enum Unchanged {
    /// The file could not be written: why.
    Unwritten(String),
    /// The file could not be read at start, nor moved aside: why.
    Frozen(String),
}

impl std::fmt::Display for Unchanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unwritten(why) | Self::Frozen(why) => f.write_str(why),
        }
    }
}

impl Env {
    /// `home`'s environment, read from its file, with `claude` resolved on it.
    pub fn open(home: &Home) -> Self {
        Self::open_at(home.rlxd_dir().join(FILE))
    }

    /// The environment in the file `path`.
    pub fn open_at(path: PathBuf) -> Self {
        let (values, frozen) = match fs::read(&path) {
            Ok(text) => match read(&text) {
                Ok(values) => (values, None),
                Err(why) => (EnvValues::default(), set_aside(&path, &why)),
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => (EnvValues::default(), None),
            Err(e) => (EnvValues::default(), set_aside(&path, &e.to_string())),
        };
        let claude = resolve(&values);
        info!("env: {}; claude: {}", describe(&values), shown(&claude));
        Self {
            path,
            state: Mutex::new(State { values, claude }),
            frozen,
        }
    }

    /// The values now.
    pub fn values(&self) -> EnvValues {
        self.state().values.clone()
    }

    /// The `claude` the values resolve to, as resolved when they were last taken in.
    pub fn claude(&self) -> Resolved {
        self.state().claude.clone()
    }

    /// Applies `change`, already checked, for `who`: the file first, then the values, and
    /// `claude` resolved again. The values now.
    pub fn set(&self, change: &EnvChange, who: &str) -> Result<EnvValues, Unchanged> {
        if let Some(why) = &self.frozen {
            return Err(Unchanged::Frozen(why.clone()));
        }
        let mut state = self.state();
        let values = state.values.with(change);
        let text = serde_json::to_vec(&values).map_err(|e| Unchanged::Unwritten(e.to_string()))?;
        durable::write(&self.path, &text).map_err(|e| {
            Unchanged::Unwritten(format!("cannot write {}: {e}", self.path.display()))
        })?;
        state.claude = resolve(&values);
        state.values = values.clone();
        let named: Vec<&str> = change.keys().map(String::as_str).collect();
        info!(
            "env: {who} set {}; now {}; claude: {}",
            named.join(", "),
            describe(&values),
            shown(&state.claude)
        );
        Ok(values)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// `text`, the file, as values every one of which this rlxd holds: else why not.
fn read(text: &[u8]) -> Result<EnvValues, String> {
    let values: EnvValues = serde_json::from_slice(text).map_err(|e| e.to_string())?;
    values.check().map_err(|e| e.to_string())?;
    Ok(values)
}

/// `claude` on the values' `PATH`, else on rlxd's own.
fn resolve(values: &EnvValues) -> Resolved {
    claude::resolve_on(values.path.as_deref().map(std::ffi::OsStr::new))
}

/// Moves the unreadable file `path` aside to a new `env.json.unreadable-<time>`, and logs
/// it. When it can't be moved either, why every change is refused.
fn set_aside(path: &Path, why: &str) -> Option<String> {
    let aside = unreadable(path);
    match fs::rename(path, &aside) {
        Ok(()) => {
            error!(
                "env: {} cannot be read ({why}), so rlxd starts with no app environment; it \
                 was moved to {}, as it was",
                path.display(),
                aside.display()
            );
            None
        }
        Err(e) => {
            let frozen = format!(
                "{} could not be read at start ({why}), nor moved aside ({e}), so rlxd \
                 changes no app environment: fix the file, or move it away, and restart rlxd",
                path.display()
            );
            error!("env: {frozen}");
            Some(frozen)
        }
    }
}

/// A new `env.json.unreadable-<time>` beside `path`: never one that exists.
fn unreadable(path: &Path) -> PathBuf {
    let stamp: String = clock::now()
        .chars()
        .filter(|c| *c != '-' && *c != ':')
        .collect();
    let base = durable::sibling(path, &format!("unreadable-{stamp}"));
    let mut candidate = base.clone();
    let mut n = 1;
    while candidate.exists() {
        candidate = durable::sibling(&base, &n.to_string());
        n += 1;
    }
    candidate
}

/// The keys set, for the log: their values may be long, and are the user's.
fn describe(values: &EnvValues) -> String {
    let keys: Vec<&str> = values.pairs().into_iter().map(|(key, _)| key).collect();
    if keys.is_empty() {
        "no app environment".to_owned()
    } else {
        keys.join(", ")
    }
}

/// `resolved`, for the log.
fn shown(resolved: &Resolved) -> String {
    match resolved {
        Resolved::Found(path) => path.display().to_string(),
        Resolved::NotFound => "not found".to_owned(),
        Resolved::NotRunnable(path) => format!("{} (a batch file, not run)", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rlxd-env-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn change(value: Value) -> EnvChange {
        let Value::Object(object) = value else {
            panic!("an object")
        };
        EnvValues::from_object(&object).unwrap()
    }

    #[test]
    fn a_change_is_written_then_read_back_by_the_next_rlxd() {
        let dir = dir("written");
        let file = dir.join(FILE);
        let env = Env::open_at(file.clone());
        assert_eq!(env.values(), EnvValues::default());
        env.set(&change(json!({"CLAUDE_CONFIG_DIR": "cc-test"})), "test")
            .unwrap();
        let again = Env::open_at(file.clone());
        assert_eq!(again.values().claude_config_dir.as_deref(), Some("cc-test"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_unreadable_file_is_set_aside_and_reported_and_rlxd_starts_without() {
        for (name, text) in [
            ("not-json", "{nope".to_owned()),
            ("bad-key", json!({"HOME": "x"}).to_string()),
            (
                "too-long",
                json!({"PATH": "x".repeat(rlxd_proto::ENV_MAX_VALUE + 1)}).to_string(),
            ),
        ] {
            let dir = dir(name);
            let file = dir.join(FILE);
            fs::write(&file, &text).unwrap();
            let env = Env::open_at(file.clone());
            assert_eq!(env.values(), EnvValues::default(), "{name}");
            assert!(!file.exists(), "{name}: moved aside");
            let aside: Vec<_> = fs::read_dir(&dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|entry| entry.starts_with("env.json.unreadable-"))
                .collect();
            assert_eq!(aside.len(), 1, "{name}: {aside:?}");
            assert_eq!(
                fs::read_to_string(dir.join(&aside[0])).unwrap(),
                text,
                "as it was"
            );
            env.set(&change(json!({"PATH": "/bin"})), "test").unwrap();
            fs::remove_dir_all(&dir).unwrap();
        }
    }
}
