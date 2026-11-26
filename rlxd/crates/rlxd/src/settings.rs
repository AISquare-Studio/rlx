//! App settings (the owner's multi-device brief, §4): theme, layout, agent sign-off,
//! send-safety mode and dotted keys for later needs, kept in the rlx home so every device
//! that connects sees the same values. `settings.get`, `settings.set` and `settings.list`
//! serve them, and each change is published as `settings.changed` on the `settings`
//! topic.
//!
//! - **The file** is `<home>/rlxd/settings.json`, 0600, written whole and durably
//!   (`crate::durable`): `{"schema_version": 1, "settings": {key: {version, value,
//!   updated_at, by_device?}}}`.
//! - **Versions** count each key's writes from 1, and a key never set is at 0. A write
//!   with an `expected_version` that is not the key's version now gets `conflict`, with
//!   `detail.version`.
//! - **Who may set what.** Every client may read every setting and set most of them. Only
//!   an app-role connection, a paired device, may set the keys in `setting_keys::GATED`,
//!   `send_safety_mode`; any other gets `not_gate`. Every change is logged with who made
//!   it, and a `send_safety_mode` change with its old and new values and whether it
//!   lowers send safety, which is logged as a warning. Lowering it needs no confirm here:
//!   whether it should is the hub's to decide.
//! - **Order.** A write is on disk before its reply and its event, and one key's events
//!   come in version order.
//! - **Limits.** A value is at most [`MAX_VALUE`] bytes of JSON, and the home holds at
//!   most [`MAX_KEYS`] settings.
//! - **An unreadable file** is set aside as `settings.json.unreadable`, and rlxd starts
//!   with no settings, logging an error. Clients then see every key at version 0 and
//!   apply their defaults; `send_safety_mode`'s is `draft`, the safest.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use rlxd_client::Home;
use rlxd_proto::{
    Setting, SettingsChanged, SettingsList, SettingsSetParams, check_setting_key,
    check_setting_value, event_types, send_safety_floor, send_safety_modes, setting_keys, topics,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::bus::Bus;
use crate::log::{error, info, warning};
use crate::{clock, durable};

/// The longest value, as JSON: 64 KiB.
pub const MAX_VALUE: usize = 64 << 10;

/// The most settings a home holds.
pub const MAX_KEYS: usize = 512;

/// The settings file, in `<home>/rlxd/`.
const FILE: &str = "settings.json";

/// The settings of one home.
pub struct Settings {
    path: PathBuf,
    entries: Mutex<BTreeMap<String, Entry>>,
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

#[derive(Serialize, Deserialize)]
struct File {
    schema_version: u32,
    settings: BTreeMap<String, Entry>,
}

/// Who is setting a setting.
#[derive(Clone, Copy, Debug)]
pub struct Writer<'a> {
    /// Whether the connection is an app-role one, a paired device.
    pub app: bool,
    /// The paired device, for an app-role connection.
    pub device: Option<&'a str>,
    /// The connection's role and client, for the log, such as `cli rlxd-cli`.
    pub who: &'a str,
}

/// Why a read or a write was refused.
#[derive(Debug)]
pub enum Refused {
    /// A key without a `.` that rlxd does not know.
    UnknownKey(String),
    /// Only an app-role connection may set this key.
    NotGate(String),
    /// The key's version is not the one the write expected: the version now.
    Conflict(u64),
    /// The params are of the wrong shape, or past a limit: why.
    Invalid(String),
    /// The file could not be written: why. Nothing changed.
    Unwritten(String),
}

impl Settings {
    /// `home`'s settings, read from its file.
    pub fn open(home: &Home) -> Self {
        Self::open_at(home.rlxd_dir().join(FILE))
    }

    /// The settings in the file `path`. An unreadable one is set aside, and logged.
    pub fn open_at(path: PathBuf) -> Self {
        let entries = match read(&path) {
            Ok(entries) => entries,
            Err(why) => {
                let aside = durable::sibling(&path, "unreadable");
                let moved = fs::rename(&path, &aside);
                error!(
                    "{} cannot be read, so rlxd starts with no settings ({why}); {}",
                    path.display(),
                    match moved {
                        Ok(()) => format!("it was moved to {}", aside.display()),
                        Err(e) => format!("and it could not be moved aside: {e}"),
                    }
                );
                BTreeMap::new()
            }
        };
        Self {
            path,
            entries: Mutex::new(entries),
        }
    }

    /// `key`'s setting: version 0 when it was never set.
    pub fn get(&self, key: &str) -> Result<Setting, Refused> {
        known(key)?;
        let entries = self.entries();
        let setting = match entries.get(key) {
            Some(entry) => entry.setting(key),
            None => Setting::unset(key),
        };
        setting.map_err(|e| Refused::Invalid(e.to_string()))
    }

    /// Every setting that has been set whose key starts with `prefix`, in key order.
    pub fn list(&self, prefix: Option<&str>) -> Result<SettingsList, Refused> {
        let entries = self.entries();
        let settings = entries
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
        known(key)?;
        if setting_keys::GATED.contains(&key) && !(writer.app && writer.device.is_some()) {
            info!("settings: {key} refused for {} (not_gate)", writer.who);
            return Err(Refused::NotGate(key.to_owned()));
        }
        let length = serde_json::to_vec(&params.value).map_or(usize::MAX, |v| v.len());
        if length > MAX_VALUE {
            return Err(Refused::Invalid(format!(
                "{key}'s value is {length} bytes of JSON, more than {MAX_VALUE}"
            )));
        }
        let mut entries = self.entries();
        let current = entries.get(key).map_or(0, |entry| entry.version);
        if let Some(expected) = params.expected_version
            && expected != current
        {
            return Err(Refused::Conflict(current));
        }
        if current == 0 && entries.len() >= MAX_KEYS {
            return Err(Refused::Invalid(format!(
                "the home holds {MAX_KEYS} settings, the most it may"
            )));
        }
        let entry = Entry {
            version: current + 1,
            value: params.value,
            updated_at: clock::now(),
            by_device: writer.device.filter(|_| writer.app).map(str::to_owned),
        };
        let old = entries.insert(key.to_owned(), entry.clone());
        if let Err(e) = self.write(&entries) {
            match old {
                Some(old) => entries.insert(key.to_owned(), old),
                None => entries.remove(key),
            };
            let why = format!("cannot write {}: {e}", self.path.display());
            error!("settings: {key}: {why}");
            return Err(Refused::Unwritten(why));
        }
        log_change(key, old.as_ref(), &entry, writer);
        let changed = SettingsChanged::new(key, entry.version, entry.by_device.clone())
            .and_then(|changed| changed.to_data());
        let published = changed.map_err(|e| e.to_string()).and_then(|data| {
            bus.publish(topics::SETTINGS, event_types::SETTINGS_CHANGED, data)
                .map_err(|e| e.to_string())
        });
        if let Err(why) = published {
            error!("settings: {key}'s settings.changed was not published: {why}");
        }
        drop(entries);
        entry
            .setting(key)
            .map_err(|e| Refused::Invalid(e.to_string()))
    }

    fn entries(&self) -> MutexGuard<'_, BTreeMap<String, Entry>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(&self, entries: &BTreeMap<String, Entry>) -> io::Result<()> {
        let file = File {
            schema_version: 1,
            settings: entries.clone(),
        };
        let mut text = serde_json::to_vec_pretty(&file).map_err(io::Error::other)?;
        text.push(b'\n');
        durable::write(&self.path, &text)
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

/// Refuses a key without a `.` that rlxd does not know.
fn known(key: &str) -> Result<(), Refused> {
    check_setting_key("key", key).map_err(|e| Refused::Invalid(e.to_string()))?;
    if key.contains('.') || setting_keys::KNOWN.contains(&key) {
        return Ok(());
    }
    Err(Refused::UnknownKey(key.to_owned()))
}

/// The settings in `path`: none when there is no file yet. Entries a setting can't hold
/// are dropped, and logged.
fn read(path: &std::path::Path) -> Result<BTreeMap<String, Entry>, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(e.to_string()),
    };
    let file: File = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    if file.schema_version != 1 {
        return Err(format!("schema_version {} is not 1", file.schema_version));
    }
    let mut entries = file.settings;
    entries.retain(|key, entry| {
        let valid = known(key).is_ok()
            && entry.version >= 1
            && check_setting_value("value", key, &entry.value).is_ok()
            && entry.setting(key).is_ok();
        if !valid {
            warning!(
                "settings: {key} in {} is not a setting, and is dropped",
                path.display()
            );
        }
        valid
    });
    Ok(entries)
}

/// Logs a change: who made it, and for `send_safety_mode` from what to what, and whether
/// it lowers send safety, as a warning.
fn log_change(key: &str, old: Option<&Entry>, new: &Entry, writer: Writer<'_>) {
    let by = match writer.device.filter(|_| writer.app) {
        Some(device) => format!("device {device:?} ({})", writer.who),
        None => writer.who.to_owned(),
    };
    if key != setting_keys::SEND_SAFETY_MODE {
        info!("settings: {key} set, version {}, by {by}", new.version);
        return;
    }
    let draft = serde_json::json!({"default": send_safety_modes::DRAFT});
    let before = old.map_or(&draft, |old| &old.value);
    let lowers = send_safety_floor(&new.value) > send_safety_floor(before);
    let message = format!(
        "settings: send_safety_mode set, version {}, by {by}: {before} -> {}{}",
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use serde_json::json;

    use super::*;
    use crate::bus::{self, Limits, Topics};

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
            Bus::open_at(self.0.join("seq"), Limits::DEFAULT).unwrap()
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
        device: Some("Sam's MacBook"),
        who: "app test",
    };

    fn set(key: &str, value: Value) -> SettingsSetParams {
        SettingsSetParams::new(key, value).unwrap()
    }

    #[test]
    fn a_set_is_on_disk_and_published_with_its_version() {
        let dir = Dir::new();
        let bus = dir.bus();
        let settings = Settings::open_at(dir.file());
        assert_eq!(settings.get("theme").unwrap().version, 0);
        let one = settings.set(set("theme", json!("rlx")), CLI, &bus).unwrap();
        let two = settings
            .set(set("theme", json!("rlx_light")), APP, &bus)
            .unwrap();
        assert_eq!((one.version, two.version), (1, 2));
        assert_eq!(two.by_device.as_deref(), Some("Sam's MacBook"));
        let again = Settings::open_at(dir.file());
        let read = again.get("theme").unwrap();
        assert_eq!(read.value, Some(json!("rlx_light")));
        assert_eq!(read.version, 2);

        let mut cursor = bus
            .cursor(Topics::of([topics::SETTINGS]).unwrap(), Some(0), true)
            .unwrap();
        let lines = bus.read(&mut cursor, bus::BATCH, bus::BATCH_BYTES);
        let events: Vec<Value> = lines
            .iter()
            .map(|l| serde_json::from_slice(l).unwrap())
            .collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "settings.changed");
        assert_eq!(events[0]["data"], json!({"key": "theme", "version": 1}));
        assert_eq!(
            events[1]["data"],
            json!({"key": "theme", "version": 2, "by_device": "Sam's MacBook"})
        );
    }

    #[test]
    fn a_stale_expected_version_is_a_conflict_and_changes_nothing() {
        let dir = Dir::new();
        let bus = dir.bus();
        let settings = Settings::open_at(dir.file());
        let first = set("layout", json!({"a": 1})).expecting(0);
        let second = set("layout", json!({"b": 2})).expecting(0);
        assert_eq!(settings.set(first, CLI, &bus).unwrap().version, 1);
        assert!(matches!(
            settings.set(second, CLI, &bus),
            Err(Refused::Conflict(1))
        ));
        assert_eq!(settings.get("layout").unwrap().value, Some(json!({"a": 1})));
        assert_eq!(bus.head(), 1, "one event: the conflict published nothing");
    }

    #[test]
    fn only_an_app_role_connection_sets_send_safety_mode() {
        let dir = Dir::new();
        let bus = dir.bus();
        let settings = Settings::open_at(dir.file());
        let mode = json!({"default": "auto"});
        assert!(matches!(
            settings.set(set("send_safety_mode", mode.clone()), CLI, &bus),
            Err(Refused::NotGate(_))
        ));
        let no_device = Writer {
            device: None,
            ..APP
        };
        assert!(matches!(
            settings.set(set("send_safety_mode", mode.clone()), no_device, &bus),
            Err(Refused::NotGate(_))
        ));
        let done = settings
            .set(set("send_safety_mode", mode), APP, &bus)
            .unwrap();
        assert_eq!(done.by_device.as_deref(), Some("Sam's MacBook"));
        // Its other settings a cli connection still sets.
        assert!(settings.set(set("theme", json!("x")), CLI, &bus).is_ok());
    }

    #[test]
    fn keys_rlxd_does_not_know_and_values_past_the_limits_are_refused() {
        let dir = Dir::new();
        let bus = dir.bus();
        let settings = Settings::open_at(dir.file());
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
        for n in 0..MAX_KEYS {
            settings
                .set(set(&format!("rlxapp.k{n}"), json!(n)), CLI, &bus)
                .unwrap();
        }
        assert!(matches!(
            settings.set(set("rlxapp.one_more", json!(1)), CLI, &bus),
            Err(Refused::Invalid(_))
        ));
        assert!(
            settings
                .set(set("rlxapp.k0", json!("again")), CLI, &bus)
                .is_ok(),
            "a key already held may still be set"
        );
    }

    #[test]
    fn list_filters_by_prefix_in_key_order() {
        let dir = Dir::new();
        let bus = dir.bus();
        let settings = Settings::open_at(dir.file());
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
        assert!(keys(settings.list(Some("nothing")).unwrap()).is_empty());
    }

    #[test]
    fn an_unreadable_file_is_set_aside_and_rlxd_starts_with_no_settings() {
        let dir = Dir::new();
        fs::write(dir.file(), "{not json").unwrap();
        let settings = Settings::open_at(dir.file());
        assert_eq!(settings.get("theme").unwrap().version, 0);
        assert!(!dir.file().exists());
        assert_eq!(
            fs::read_to_string(durable::sibling(&dir.file(), "unreadable")).unwrap(),
            "{not json"
        );
    }

    #[test]
    fn entries_a_setting_cannot_hold_are_dropped_on_read() {
        let dir = Dir::new();
        let file = json!({"schema_version": 1, "settings": {
            "theme": {"version": 2, "value": "rlx", "updated_at": "2026-11-26T12:00:00.000Z"},
            "layout": {"version": 1, "value": "not an object",
                       "updated_at": "2026-11-26T12:00:00.000Z"},
            "nope": {"version": 1, "value": 1, "updated_at": "2026-11-26T12:00:00.000Z"},
        }});
        fs::write(dir.file(), file.to_string()).unwrap();
        let settings = Settings::open_at(dir.file());
        let listed: Vec<String> = settings
            .list(None)
            .unwrap()
            .settings
            .into_iter()
            .map(|s| s.key)
            .collect();
        assert_eq!(listed, ["theme"]);
    }

    #[test]
    fn a_failed_write_changes_nothing_and_publishes_nothing() {
        let dir = Dir::new();
        let bus = dir.bus();
        // A folder where the file goes: the write's rename fails.
        let settings = Settings::open_at(dir.0.join("blocked"));
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
        let settings = Settings::open_at(dir.file());
        settings.set(set("theme", json!("rlx")), CLI, &bus).unwrap();
        let mode = fs::metadata(dir.file()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
