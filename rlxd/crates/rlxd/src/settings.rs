//! App settings (the owner's multi-device brief, §4): theme, layout, agent sign-off,
//! send-safety mode and dotted keys for later needs, kept in the rlx home so every device
//! that connects sees the same values. `settings.get`, `settings.set`, `settings.list` and
//! `settings.delete` serve them, and each change is published on the `settings` topic,
//! `settings.changed` or `settings.deleted`. Settings are not for secrets: every client
//! with the token reads every one.
//!
//! - **The file** is `<home>/rlxd/settings.json`, 0600, compact JSON written whole and
//!   durably (`crate::durable`): `{"schema_version": 1, "settings": {key: {version,
//!   value, updated_at, by_device?}}}`.
//! - **Versions** count each key's writes from 1, and a key never set is at 0. A write or
//!   a delete with an `expected_version` that is not the key's version now gets
//!   `conflict`, with `detail.version`. A deleted key's numbering starts again.
//! - **Who may change what.** Every client may read every setting. The protected keys,
//!   `send_safety_mode` and every key under `policy.` (`setting_is_protected`), only an
//!   app-role connection holding the gate, a paired device, may set or delete; any other
//!   gets `not_gate`. Any client may set or delete the rest, so a user with the CLI or the
//!   TUI and no app can still define agents and preferences. Every change is logged with
//!   who made it. A `send_safety_mode` change is logged with its old and new values, and
//!   as a warning when it lowers send safety for any connector. Lowering it needs no
//!   confirm here: whether it should is the hub's to decide.
//! - **Order.** A change is on disk before its reply and its event, and one key's events
//!   come in version order.
//! - **Limits.** A value is at most [`MAX_VALUE`] bytes of JSON and
//!   [`rlxd_proto::MAX_SETTING_DEPTH`] arrays or objects deep. The home holds at most
//!   [`MAX_KEYS`] settings and [`MAX_TOTAL`] bytes of keys and values, so `settings.list`'s
//!   reply always fits the line a client reads (16 MiB).
//! - **Reading the file** goes key by key, so one bad value never costs the others. A key
//!   whose entry this rlxd cannot hold is set aside: written to
//!   `settings.json.unreadable-<time>`, a new file each time, logged, and left out of
//!   the file from then on. A key without a `.` that this rlxd does not know, a newer
//!   rlxd's, is kept as it is through every write, and not served. A file that is not
//!   one of these documents at all is moved to such a file whole, and rlxd starts with no
//!   settings; clients then see every key at version 0 and apply their defaults.
//!   `send_safety_mode`'s is `draft`, the safest.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use rlxd_client::Home;
use rlxd_proto::{
    Setting, SettingsChanged, SettingsDeleteParams, SettingsList, SettingsSetParams,
    check_setting_key, check_setting_value, event_types, send_safety_lowers, setting_is_protected,
    setting_keys, topics,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;

use crate::bus::Bus;
use crate::log::{error, info, warning};
use crate::{clock, durable};

/// The longest value, as JSON: 64 KiB.
pub const MAX_VALUE: usize = 64 << 10;

/// The most settings a home holds.
pub const MAX_KEYS: usize = 512;

/// The most bytes of keys and values, as JSON, a home holds: 8 MiB, half the line a
/// client reads, so `settings.list`'s reply, with each setting's other fields, fits it.
pub const MAX_TOTAL: usize = 8 << 20;

/// The settings file, in `<home>/rlxd/`.
const FILE: &str = "settings.json";

/// What a home's settings may hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The longest value, as JSON.
    pub value: usize,
    /// The most settings.
    pub keys: usize,
    /// The most bytes of keys and values, as JSON.
    pub total: usize,
}

impl Limits {
    /// [`MAX_VALUE`], [`MAX_KEYS`] and [`MAX_TOTAL`].
    pub const DEFAULT: Self = Self {
        value: MAX_VALUE,
        keys: MAX_KEYS,
        total: MAX_TOTAL,
    };
}

/// The settings of one home.
pub struct Settings {
    path: PathBuf,
    limits: Limits,
    store: Mutex<Store>,
}

#[derive(Default)]
struct Store {
    /// The settings this rlxd serves.
    entries: BTreeMap<String, Entry>,
    /// Keys without a `.` that this rlxd does not know, kept verbatim for a newer one.
    kept: BTreeMap<String, Box<RawValue>>,
}

/// One setting as the file holds it.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
    version: u64,
    value: Value,
    updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    by_device: Option<String>,
}

/// The file, as read: each key's entry as its text, to be read on its own.
#[derive(Deserialize)]
struct FileIn {
    schema_version: u32,
    settings: BTreeMap<String, Box<RawValue>>,
}

/// One key of the file, as written.
#[derive(Serialize)]
#[serde(untagged)]
enum Out<'a> {
    Entry(&'a Entry),
    Kept(&'a RawValue),
}

/// Who is changing a setting.
#[derive(Clone, Copy, Debug)]
pub struct Writer<'a> {
    /// Whether the connection is an app-role one that holds the gate, a paired device.
    pub app: bool,
    /// The paired device, for an app-role connection.
    pub device: Option<&'a str>,
    /// The connection's role and client, for the log, such as `cli rlxd-cli`.
    pub who: &'a str,
}

/// Why a read or a change was refused.
#[derive(Debug)]
pub enum Refused {
    /// A key without a `.` that rlxd does not know.
    UnknownKey(String),
    /// A protected key, which only an app-role connection holding the gate may change.
    NotGate(String),
    /// The key's version is not the one the change expected: the version now.
    Conflict(u64),
    /// The params are of the wrong shape, or past a limit: why.
    Invalid(String),
    /// The file could not be written: why. Nothing changed.
    Unwritten(String),
}

impl Settings {
    /// `home`'s settings, read from its file.
    pub fn open(home: &Home) -> Self {
        Self::open_at(home.rlxd_dir().join(FILE), Limits::DEFAULT)
    }

    /// The settings in the file `path`, holding what `limits` allow, read key by key: what
    /// this rlxd cannot hold is set aside and logged, and the rest kept.
    pub fn open_at(path: PathBuf, limits: Limits) -> Self {
        let store = match fs::read(&path) {
            Ok(text) => load(&path, &text, limits),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Store::default(),
            Err(e) => {
                error!(
                    "{} cannot be read, so rlxd starts with no settings: {e}",
                    path.display()
                );
                Store::default()
            }
        };
        Self {
            path,
            limits,
            store: Mutex::new(store),
        }
    }

    /// `key`'s setting: version 0 when it was never set.
    pub fn get(&self, key: &str) -> Result<Setting, Refused> {
        known(key)?;
        let store = self.store();
        let setting = match store.entries.get(key) {
            Some(entry) => entry.setting(key),
            None => Setting::unset(key),
        };
        setting.map_err(|e| Refused::Invalid(e.to_string()))
    }

    /// Every setting that has been set whose key starts with `prefix`, in key order.
    pub fn list(&self, prefix: Option<&str>) -> Result<SettingsList, Refused> {
        let store = self.store();
        let settings = store
            .entries
            .iter()
            .filter(|(key, _)| prefix.is_none_or(|p| key.starts_with(p)))
            .map(|(key, entry)| entry.setting(key))
            .collect::<Result<Vec<_>, _>>()
            .and_then(SettingsList::new);
        settings.map_err(|e| Refused::Invalid(e.to_string()))
    }

    /// Sets `params.key` to `params.value` for `writer`, and publishes `settings.changed`
    /// on `bus` once it is on disk. The setting as it is now.
    pub fn set(
        &self,
        params: SettingsSetParams,
        writer: Writer<'_>,
        bus: &Bus,
    ) -> Result<Setting, Refused> {
        let key = params.key.as_str();
        may_change(key, writer, "set")?;
        let limits = self.limits;
        let length = json_length(&params.value);
        if length > limits.value {
            return Err(Refused::Invalid(format!(
                "{key}'s value is {length} bytes of JSON, more than {}",
                limits.value
            )));
        }
        let mut store = self.store();
        let current = store.entries.get(key);
        let version = current.map_or(0, |entry| entry.version);
        if let Some(expected) = params.expected_version
            && expected != version
        {
            return Err(Refused::Conflict(version));
        }
        if current.is_none() && store.entries.len() >= limits.keys {
            return Err(Refused::Invalid(format!(
                "the home holds {} settings, the most it may: delete one first",
                limits.keys
            )));
        }
        let total =
            store.total() - current.map_or(0, |entry| size(key, entry)) + key.len() + length;
        if total > limits.total {
            return Err(Refused::Invalid(format!(
                "the home's settings would be {total} bytes of keys and values, more than \
                 {} ({} MiB): delete some first",
                limits.total,
                limits.total >> 20
            )));
        }
        let entry = Entry {
            version: version + 1,
            value: params.value,
            updated_at: clock::now(),
            by_device: writer.device.filter(|_| writer.app).map(str::to_owned),
        };
        let old = store.entries.insert(key.to_owned(), entry.clone());
        if let Err(why) = self.write(&store) {
            match old {
                Some(old) => store.entries.insert(key.to_owned(), old),
                None => store.entries.remove(key),
            };
            return Err(why);
        }
        log_set(key, old.as_ref(), &entry, writer);
        publish(
            bus,
            event_types::SETTINGS_CHANGED,
            key,
            entry.version,
            entry.by_device.as_deref(),
        );
        drop(store);
        entry
            .setting(key)
            .map_err(|e| Refused::Invalid(e.to_string()))
    }

    /// Deletes `params.key` for `writer`, back to never set, and publishes
    /// `settings.deleted` on `bus` once it is on disk. A key never set is left as it is,
    /// with no event. The setting as it is now: version 0.
    pub fn delete(
        &self,
        params: SettingsDeleteParams,
        writer: Writer<'_>,
        bus: &Bus,
    ) -> Result<Setting, Refused> {
        let key = params.key.as_str();
        may_change(key, writer, "delete")?;
        let mut store = self.store();
        let version = store.entries.get(key).map_or(0, |entry| entry.version);
        if let Some(expected) = params.expected_version
            && expected != version
        {
            return Err(Refused::Conflict(version));
        }
        let unset = Setting::unset(key).map_err(|e| Refused::Invalid(e.to_string()))?;
        let Some(old) = store.entries.remove(key) else {
            return Ok(unset);
        };
        if let Err(why) = self.write(&store) {
            store.entries.insert(key.to_owned(), old);
            return Err(why);
        }
        info!(
            "settings: {key} deleted, was version {version}, by {}",
            by(writer)
        );
        let by_device = writer.device.filter(|_| writer.app);
        publish(bus, event_types::SETTINGS_DELETED, key, version, by_device);
        Ok(unset)
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        self.store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Writes `store` as the file, compact. `Unwritten` when it can't be.
    fn write(&self, store: &Store) -> Result<(), Refused> {
        write_file(&self.path, store).map_err(|e| {
            let why = format!("cannot write {}: {e}", self.path.display());
            error!("settings: {why}");
            Refused::Unwritten(why)
        })
    }
}

impl Store {
    /// The bytes of keys and values, as JSON, the store holds.
    fn total(&self) -> usize {
        self.entries
            .iter()
            .map(|(key, entry)| size(key, entry))
            .sum()
    }
}

impl Entry {
    fn setting(&self, key: &str) -> Result<Setting, rlxd_proto::Invalid> {
        Setting::new(
            key,
            self.version,
            self.value.clone(),
            self.updated_at.clone(),
            self.by_device.clone(),
        )
    }
}

/// `key` and its value, as the limits count them.
fn size(key: &str, entry: &Entry) -> usize {
    key.len() + json_length(&entry.value)
}

fn json_length(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |text| text.len())
}

/// Refuses a key without a `.` that rlxd does not know.
fn known(key: &str) -> Result<(), Refused> {
    check_setting_key("key", key).map_err(|e| Refused::Invalid(e.to_string()))?;
    if key.contains('.') || setting_keys::KNOWN.contains(&key) {
        return Ok(());
    }
    Err(Refused::UnknownKey(key.to_owned()))
}

/// Refuses a key rlxd does not know, and a protected key to any but an app-role
/// connection that holds the gate.
fn may_change(key: &str, writer: Writer<'_>, what: &str) -> Result<(), Refused> {
    known(key)?;
    if setting_is_protected(key) && !(writer.app && writer.device.is_some()) {
        info!(
            "settings: {what} of {key} refused for {} (not_gate)",
            writer.who
        );
        return Err(Refused::NotGate(key.to_owned()));
    }
    Ok(())
}

fn by(writer: Writer<'_>) -> String {
    match writer.device.filter(|_| writer.app) {
        Some(device) => format!("device {device:?} ({})", writer.who),
        None => writer.who.to_owned(),
    }
}

/// Publishes `kind {key, version, by_device?}`, logging a failure: the change itself is on
/// disk already.
fn publish(bus: &Bus, kind: &str, key: &str, version: u64, by_device: Option<&str>) {
    let data = SettingsChanged::new(key, version, by_device.map(str::to_owned))
        .and_then(|changed| changed.to_data())
        .map_err(|e| e.to_string());
    let published = data.and_then(|data| {
        bus.publish(topics::SETTINGS, kind, data)
            .map_err(|e| e.to_string())
    });
    if let Err(why) = published {
        error!("settings: {key}'s {kind} was not published: {why}");
    }
}

/// Logs a set: who made it, and for `send_safety_mode` from what to what, as a warning
/// when it lowers send safety for any connector.
fn log_set(key: &str, old: Option<&Entry>, new: &Entry, writer: Writer<'_>) {
    let by = by(writer);
    if key != setting_keys::SEND_SAFETY_MODE {
        let protected = if setting_is_protected(key) {
            " (protected)"
        } else {
            ""
        };
        info!(
            "settings: {key} set, version {}, by {by}{protected}",
            new.version
        );
        return;
    }
    let before = old.map(|old| &old.value);
    let lowers = send_safety_lowers(before, &new.value);
    let was = before.map_or_else(
        || r#"{"default":"draft"} (never set)"#.to_owned(),
        Value::to_string,
    );
    let message = format!(
        "settings: send_safety_mode set, version {}, by {by}: {was} -> {}{}",
        new.version,
        new.value,
        if lowers { " (lowers send safety)" } else { "" }
    );
    if lowers {
        warning!("{message}");
    } else {
        info!("{message}");
    }
}

/// The file `text` at `path`, read key by key. What this rlxd cannot hold is set aside to a
/// new `settings.json.unreadable-<time>`, logged, and written out of the file.
fn load(path: &Path, text: &[u8], limits: Limits) -> Store {
    let file = match serde_json::from_slice::<FileIn>(text) {
        Ok(file) if file.schema_version == 1 => file,
        Ok(file) => {
            set_aside_whole(
                path,
                &format!("schema_version {} is not 1", file.schema_version),
            );
            return Store::default();
        }
        Err(e) => {
            set_aside_whole(path, &e.to_string());
            return Store::default();
        }
    };
    let mut store = Store::default();
    let mut aside: BTreeMap<String, (Box<RawValue>, String)> = BTreeMap::new();
    let mut total = 0;
    for (key, raw) in file.settings {
        match read_entry(&key, &raw) {
            Ok(None) => {
                store.kept.insert(key, raw);
            }
            Ok(Some(entry)) => {
                let grows = size(&key, &entry);
                if store.entries.len() >= limits.keys || total + grows > limits.total {
                    aside.insert(key, (raw, "past the home's limits".to_owned()));
                    continue;
                }
                total += grows;
                store.entries.insert(key, entry);
            }
            Err(why) => {
                aside.insert(key, (raw, why));
            }
        }
    }
    if !aside.is_empty() {
        set_aside_keys(path, &aside);
        if let Err(e) = write_file(path, &store) {
            error!(
                "settings: cannot write {} without them: {e}",
                path.display()
            );
        }
    }
    store
}

/// One key's entry: `None` for a key without a `.` this rlxd does not know, kept as it is.
fn read_entry(key: &str, raw: &RawValue) -> Result<Option<Entry>, String> {
    check_setting_key("key", key).map_err(|e| e.to_string())?;
    if !key.contains('.') && !setting_keys::KNOWN.contains(&key) {
        return Ok(None);
    }
    let entry: Entry = serde_json::from_str(raw.get()).map_err(|e| e.to_string())?;
    check_setting_value("value", key, &entry.value).map_err(|e| e.to_string())?;
    entry.setting(key).map_err(|e| e.to_string())?;
    Ok(Some(entry))
}

/// Moves the whole file aside, when it is not a settings document at all.
fn set_aside_whole(path: &Path, why: &str) {
    let aside = unreadable(path);
    let moved = fs::rename(path, &aside);
    error!(
        "{} cannot be read, so rlxd starts with no settings ({why}); {}",
        path.display(),
        match moved {
            Ok(()) => format!("it was moved to {}", aside.display()),
            Err(e) => format!("and it could not be moved aside: {e}"),
        }
    );
}

/// Writes the keys set aside, each with why, to a new file beside the settings.
fn set_aside_keys(path: &Path, aside: &BTreeMap<String, (Box<RawValue>, String)>) {
    #[derive(Serialize)]
    struct Aside<'a> {
        settings: BTreeMap<&'a str, &'a RawValue>,
        why: BTreeMap<&'a str, &'a str>,
    }
    let doc = Aside {
        settings: aside
            .iter()
            .map(|(k, (raw, _))| (k.as_str(), &**raw))
            .collect(),
        why: aside
            .iter()
            .map(|(k, (_, why))| (k.as_str(), why.as_str()))
            .collect(),
    };
    let target = unreadable(path);
    let keys: Vec<&str> = aside.keys().map(String::as_str).collect();
    let reasons: Vec<String> = doc.why.iter().map(|(k, w)| format!("{k}: {w}")).collect();
    let written = serde_json::to_vec(&doc)
        .map_err(io::Error::other)
        .and_then(|text| durable::write(&target, &text));
    match written {
        Ok(()) => error!(
            "settings: {} can't be held, and were set aside to {}: {}",
            keys.join(", "),
            target.display(),
            reasons.join("; ")
        ),
        Err(e) => error!(
            "settings: {} can't be held, and could not be set aside to {}: {e}",
            keys.join(", "),
            target.display()
        ),
    }
}

/// A new `settings.json.unreadable-<time>` beside `path`: never one that exists.
fn unreadable(path: &Path) -> PathBuf {
    let stamp: String = clock::now()
        .chars()
        .filter(|c| *c != '-' && *c != ':')
        .collect();
    unreadable_at(path, &stamp)
}

/// `settings.json.unreadable-<stamp>` beside `path`, numbered on when one exists.
fn unreadable_at(path: &Path, stamp: &str) -> PathBuf {
    let base = durable::sibling(path, &format!("unreadable-{stamp}"));
    let mut candidate = base.clone();
    let mut n = 1;
    while candidate.exists() {
        candidate = durable::sibling(&base, &n.to_string());
        n += 1;
    }
    candidate
}

/// Writes `store` to `path`, compact, its kept keys verbatim.
fn write_file(path: &Path, store: &Store) -> io::Result<()> {
    #[derive(Serialize)]
    struct FileOut<'a> {
        schema_version: u32,
        settings: BTreeMap<&'a str, Out<'a>>,
    }
    let mut settings: BTreeMap<&str, Out<'_>> = store
        .kept
        .iter()
        .map(|(key, raw)| (key.as_str(), Out::Kept(raw)))
        .collect();
    settings.extend(
        store
            .entries
            .iter()
            .map(|(k, e)| (k.as_str(), Out::Entry(e))),
    );
    let mut text = serde_json::to_vec(&FileOut {
        schema_version: 1,
        settings,
    })
    .map_err(io::Error::other)?;
    text.push(b'\n');
    durable::write(path, &text)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use serde_json::json;

    use super::*;
    use crate::bus::{self, Topics};

    struct Dir(PathBuf);

    impl Dir {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("rlxd-set-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self) -> PathBuf {
            self.0.join(FILE)
        }

        fn bus(&self) -> Bus {
            Bus::open_at(self.0.join("seq"), bus::Limits::DEFAULT).unwrap()
        }

        fn settings(&self) -> Settings {
            Settings::open_at(self.file(), Limits::DEFAULT)
        }

        /// The files set aside, by name.
        fn unreadable(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(&self.0)
                .unwrap()
                .filter_map(|e| e.ok()?.file_name().into_string().ok())
                .filter(|name| name.starts_with("settings.json.unreadable-"))
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const CLI: Writer<'static> = Writer {
        app: false,
        device: None,
        who: "cli test",
    };

    const APP: Writer<'static> = Writer {
        app: true,
        device: Some("Sam MacBook"),
        who: "app test",
    };

    fn set(key: &str, value: Value) -> SettingsSetParams {
        SettingsSetParams::new(key, value).unwrap()
    }

    fn delete(key: &str) -> SettingsDeleteParams {
        SettingsDeleteParams::new(key).unwrap()
    }

    fn events(bus: &Bus) -> Vec<Value> {
        let mut cursor = bus
            .cursor(Topics::of([topics::SETTINGS]).unwrap(), Some(0), true)
            .unwrap();
        bus.read(&mut cursor, bus::BATCH, bus::BATCH_BYTES)
            .iter()
            .map(|l| serde_json::from_slice(l).unwrap())
            .collect()
    }

    fn nested(depth: usize) -> String {
        format!("{}1{}", "[".repeat(depth), "]".repeat(depth))
    }

    #[test]
    fn a_set_is_on_disk_and_published_with_its_version() {
        let dir = Dir::new();
        let bus = dir.bus();
        let settings = dir.settings();
        assert_eq!(settings.get("theme").unwrap().version, 0);
        let one = settings.set(set("theme", json!("rlx")), CLI, &bus).unwrap();
        let two = settings
            .set(set("theme", json!("rlx_light")), APP, &bus)
            .unwrap();
        assert_eq!((one.version, two.version), (1, 2));
        assert_eq!(two.by_device.as_deref(), Some("Sam MacBook"));
        let again = dir.settings();
        let read = again.get("theme").unwrap();
        assert_eq!(read.value, Some(json!("rlx_light")));
        assert_eq!(read.version, 2);

        let events = events(&bus);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "settings.changed");
        assert_eq!(events[0]["data"], json!({"key": "theme", "version": 1}));
        assert_eq!(
            events[1]["data"],
            json!({"key": "theme", "version": 2, "by_device": "Sam MacBook"})
        );
    }

    #[test]
    fn the_file_is_compact_json() {
        let dir = Dir::new();
        let bus = dir.bus();
        let settings = dir.settings();
        let deep = (0..30).fold(json!(1), |inner, _| json!({ "a": inner }));
        settings.set(set("layout", deep), CLI, &bus).unwrap();
        let text = fs::read_to_string(dir.file()).unwrap();
        assert_eq!(text.lines().count(), 1, "one line: {text}");
        assert!(!text.contains("  "), "no indentation");
    }

    #[test]
    fn a_stale_expected_version_is_a_conflict_and_changes_nothing() {
        let dir = Dir::new();
        let bus = dir.bus();
        let settings = dir.settings();
        let first = set("layout", json!({"a": 1})).expecting(0);
        let second = set("layout", json!({"b": 2})).expecting(0);
        assert_eq!(settings.set(first, CLI, &bus).unwrap().version, 1);
        assert!(matches!(
            settings.set(second, CLI, &bus),
            Err(Refused::Conflict(1))
        ));
        assert_eq!(settings.get("layout").unwrap().value, Some(json!({"a": 1})));
        assert!(matches!(
            settings.delete(delete("layout").expecting(5), CLI, &bus),
            Err(Refused::Conflict(1))
        ));
        assert_eq!(bus.head(), 1, "one event: the conflicts published nothing");
    }

    #[test]
    fn only_an_app_role_connection_changes_protected_keys() {
        let dir = Dir::new();
        let bus = dir.bus();
        let settings = dir.settings();
        let no_device = Writer {
            device: None,
            ..APP
        };
        for (key, value) in [
            ("send_safety_mode", json!({"default": "auto"})),
            ("policy.app_absent", json!("queue")),
            ("policy.connectors.slack", json!({"write": "draft"})),
        ] {
            for writer in [CLI, no_device] {
                assert!(
                    matches!(
                        settings.set(set(key, value.clone()), writer, &bus),
                        Err(Refused::NotGate(_))
                    ),
                    "{key}"
                );
            }
            let done = settings.set(set(key, value), APP, &bus).unwrap();
            assert_eq!(done.by_device.as_deref(), Some("Sam MacBook"));
            assert!(matches!(
                settings.delete(delete(key), CLI, &bus),
                Err(Refused::NotGate(_))
            ));
            assert_eq!(settings.get(key).unwrap().version, 1, "{key} stays");
        }
        // The rest any client sets and deletes, agents included.
        for key in ["theme", "agents.ava", "rlxapp.policy"] {
            let value = if key == "theme" {
                json!("x")
            } else {
                json!({"a": 1})
            };
            assert!(settings.set(set(key, value), CLI, &bus).is_ok(), "{key}");
            assert!(settings.delete(delete(key), CLI, &bus).is_ok(), "{key}");
        }
    }

    #[test]
    fn a_delete_is_on_disk_published_and_frees_its_place() {
        let dir = Dir::new();
        let bus = dir.bus();
        let few = Limits {
            keys: 8,
            ..Limits::DEFAULT
        };
        let settings = Settings::open_at(dir.file(), few);
        settings.set(set("rlxapp.a", json!(1)), CLI, &bus).unwrap();
        settings.set(set("rlxapp.a", json!(2)), APP, &bus).unwrap();
        let gone = settings
            .delete(delete("rlxapp.a").expecting(2), APP, &bus)
            .unwrap();
        assert_eq!(gone.version, 0);
        assert!(gone.value.is_none());
        let events = events(&bus);
        assert_eq!(events[2]["type"], "settings.deleted");
        assert_eq!(
            events[2]["data"],
            json!({"key": "rlxapp.a", "version": 2, "by_device": "Sam MacBook"})
        );
        assert_eq!(
            Settings::open_at(dir.file(), few)
                .get("rlxapp.a")
                .unwrap()
                .version,
            0
        );
        // Its numbering starts again; a key never set deletes to nothing, unannounced.
        let again = settings.set(set("rlxapp.a", json!(3)), CLI, &bus).unwrap();
        assert_eq!(again.version, 1);
        let head = bus.head();
        let never = settings.delete(delete("rlxapp.never"), CLI, &bus).unwrap();
        assert_eq!(never.version, 0);
        assert_eq!(bus.head(), head);

        // The key limit is no lasting cap: a delete makes room.
        for n in 1..few.keys {
            settings
                .set(set(&format!("rlxapp.k{n}"), json!(n)), CLI, &bus)
                .unwrap();
        }
        assert!(matches!(
            settings.set(set("rlxapp.one_more", json!(1)), CLI, &bus),
            Err(Refused::Invalid(_))
        ));
        settings.delete(delete("rlxapp.k1"), CLI, &bus).unwrap();
        assert!(
            settings
                .set(set("rlxapp.one_more", json!(1)), CLI, &bus)
                .is_ok()
        );
    }

    #[test]
    fn keys_rlxd_does_not_know_and_values_past_the_limits_are_refused() {
        let dir = Dir::new();
        let bus = dir.bus();
        let settings = dir.settings();
        assert!(matches!(
            settings.set(set("later_known_key", json!(1)), CLI, &bus),
            Err(Refused::UnknownKey(_))
        ));
        assert!(matches!(
            settings.get("later_known_key"),
            Err(Refused::UnknownKey(_))
        ));
        let huge = json!("x".repeat(MAX_VALUE));
        assert!(matches!(
            settings.set(set("rlxapp.huge", huge), CLI, &bus),
            Err(Refused::Invalid(_))
        ));
    }

    #[test]
    fn the_total_stays_under_its_cap_with_a_clear_refusal() {
        // The real cap (8 MiB of 64 KiB values) is tests/settings.rs's; here a small one.
        let dir = Dir::new();
        let bus = dir.bus();
        let small = Limits {
            value: 1 << 10,
            total: 8 << 10,
            ..Limits::DEFAULT
        };
        let settings = Settings::open_at(dir.file(), small);
        let value = json!("x".repeat((1 << 10) - 2));
        let mut refused = None;
        for n in 0..20 {
            match settings.set(set(&format!("rlxapp.k{n}"), value.clone()), CLI, &bus) {
                Ok(_) => {}
                Err(Refused::Invalid(why)) => {
                    refused = Some((n, why));
                    break;
                }
                Err(other) => panic!("{other:?}"),
            }
        }
        let (n, why) = refused.expect("refused before 20");
        assert!(
            why.contains("more than 8192") && why.contains("delete some first"),
            "{why}"
        );
        assert_eq!(n, 7, "eight 1 KiB values and their keys pass 8 KiB");
        // Rewriting one smaller still fits; a reload holds what was kept.
        assert!(settings.set(set("rlxapp.k0", json!(1)), CLI, &bus).is_ok());
        let again = Settings::open_at(dir.file(), small);
        assert_eq!(again.list(None).unwrap().settings.len(), 7);
    }

    #[test]
    fn list_filters_by_prefix_in_key_order() {
        let dir = Dir::new();
        let bus = dir.bus();
        let settings = dir.settings();
        for key in ["rlxapp.b", "theme", "rlxapp.a", "tray.style"] {
            settings.set(set(key, json!("v")), CLI, &bus).unwrap();
        }
        let keys = |list: SettingsList| -> Vec<String> {
            list.settings.into_iter().map(|s| s.key).collect()
        };
        assert_eq!(
            keys(settings.list(None).unwrap()),
            ["rlxapp.a", "rlxapp.b", "theme", "tray.style"]
        );
        assert_eq!(
            keys(settings.list(Some("rlxapp.")).unwrap()),
            ["rlxapp.a", "rlxapp.b"]
        );
    }

    /// A settings file holding `entries`, each `key: raw entry text`.
    fn write_raw(dir: &Dir, entries: &[(&str, String)]) {
        let body: Vec<String> = entries
            .iter()
            .map(|(key, raw)| format!("{}:{raw}", json!(key)))
            .collect();
        let text = format!(
            r#"{{"schema_version":1,"settings":{{{}}}}}"#,
            body.join(",")
        );
        fs::write(dir.file(), text).unwrap();
    }

    fn entry(value: &str) -> String {
        format!(r#"{{"version":3,"value":{value},"updated_at":"2026-11-26T12:00:00.000Z"}}"#)
    }

    #[test]
    fn a_value_too_deep_to_hold_is_set_aside_alone_and_the_rest_kept() {
        let dir = Dir::new();
        write_raw(
            &dir,
            &[
                ("theme", entry(r#""rlx""#)),
                ("rlxapp.deep", entry(&nested(125))),
                ("rlxapp.deeper", entry(&nested(500))),
                ("layout", entry(r#""not an object""#)),
                ("rlxapp.fine", entry("[1,2]")),
            ],
        );
        let settings = dir.settings();
        let keys: Vec<String> = settings
            .list(None)
            .unwrap()
            .settings
            .into_iter()
            .map(|s| s.key)
            .collect();
        assert_eq!(keys, ["rlxapp.fine", "theme"], "the good ones survive");
        assert_eq!(settings.get("theme").unwrap().version, 3);
        let aside = dir.unreadable();
        assert_eq!(aside.len(), 1, "{aside:?}");
        // Read only why: the file holds the 500-deep value, which no Value can.
        #[derive(Deserialize)]
        struct Aside {
            why: BTreeMap<String, String>,
        }
        let set_aside: Aside =
            serde_json::from_str(&fs::read_to_string(dir.0.join(&aside[0])).unwrap()).unwrap();
        let held: Vec<&String> = set_aside.why.keys().collect();
        assert_eq!(held, ["layout", "rlxapp.deep", "rlxapp.deeper"]);
        // And they are gone from the file, so the next start sets nothing aside again.
        drop(settings);
        let again = dir.settings();
        assert_eq!(again.list(None).unwrap().settings.len(), 2);
        assert_eq!(dir.unreadable().len(), 1);
    }

    #[test]
    fn a_set_too_deep_is_refused() {
        let dir = Dir::new();
        let bus = dir.bus();
        let settings = dir.settings();
        let deep: Value = serde_json::from_str(&nested(125)).unwrap();
        assert!(SettingsSetParams::new("rlxapp.deep", deep).is_err());
        let fine: Value = serde_json::from_str(&nested(32)).unwrap();
        assert!(settings.set(set("rlxapp.deep", fine), CLI, &bus).is_ok());
    }

    #[test]
    fn a_key_this_rlxd_does_not_know_is_kept_verbatim_through_writes() {
        let dir = Dir::new();
        let future = r#"{"version":9,"value":{"new":"shape"},"updated_at":"x","more":true}"#;
        write_raw(
            &dir,
            &[
                ("theme", entry(r#""rlx""#)),
                ("future_key", future.to_owned()),
            ],
        );
        let bus = dir.bus();
        let settings = dir.settings();
        assert!(matches!(
            settings.get("future_key"),
            Err(Refused::UnknownKey(_))
        ));
        settings
            .set(set("theme", json!("rlx_light")), CLI, &bus)
            .unwrap();
        let file: Value = serde_json::from_str(&fs::read_to_string(dir.file()).unwrap()).unwrap();
        let kept: Value = serde_json::from_str(future).unwrap();
        assert_eq!(file["settings"]["future_key"], kept, "kept as it was");
        assert!(dir.unreadable().is_empty());
    }

    #[test]
    fn an_unreadable_file_is_set_aside_whole_and_never_over_an_earlier_one() {
        let dir = Dir::new();
        fs::write(dir.file(), "{not json").unwrap();
        let settings = dir.settings();
        assert_eq!(settings.get("theme").unwrap().version, 0);
        assert!(!dir.file().exists());
        fs::write(dir.file(), r#"{"schema_version":2,"settings":{}}"#).unwrap();
        let _ = dir.settings();
        let aside = dir.unreadable();
        assert_eq!(
            aside.len(),
            2,
            "two files, neither over the other: {aside:?}"
        );
        let texts: Vec<String> = aside
            .iter()
            .map(|name| fs::read_to_string(dir.0.join(name)).unwrap())
            .collect();
        assert!(texts.contains(&"{not json".to_owned()));
        assert!(texts.iter().any(|t| t.contains("schema_version\":2")));
        for name in &aside {
            assert!(!name.contains(':'), "a name Windows takes: {name}");
        }
    }

    #[test]
    fn a_file_set_aside_never_takes_the_name_of_one_that_exists() {
        let dir = Dir::new();
        let stamp = "20261126T120000.000Z";
        let first = unreadable_at(&dir.file(), stamp);
        assert!(first.ends_with(format!("settings.json.unreadable-{stamp}")));
        fs::write(&first, "one").unwrap();
        let second = unreadable_at(&dir.file(), stamp);
        assert_ne!(second, first);
        fs::write(&second, "two").unwrap();
        let third = unreadable_at(&dir.file(), stamp);
        assert!(third != first && third != second, "{third:?}");
        assert_eq!(
            fs::read_to_string(&first).unwrap(),
            "one",
            "never overwritten"
        );
    }

    #[test]
    fn a_failed_write_changes_nothing_and_publishes_nothing() {
        let dir = Dir::new();
        let bus = dir.bus();
        // A folder where the file goes: the write's rename fails.
        let settings = Settings::open_at(dir.0.join("blocked"), Limits::DEFAULT);
        fs::create_dir_all(dir.0.join("blocked")).unwrap();
        fs::write(dir.0.join("blocked").join("x"), "").unwrap();
        assert!(matches!(
            settings.set(set("theme", json!("rlx")), CLI, &bus),
            Err(Refused::Unwritten(_))
        ));
        assert_eq!(settings.get("theme").unwrap().version, 0);
        assert_eq!(bus.head(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Dir::new();
        let bus = dir.bus();
        let settings = dir.settings();
        settings.set(set("theme", json!("rlx")), CLI, &bus).unwrap();
        let mode = fs::metadata(dir.file()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
