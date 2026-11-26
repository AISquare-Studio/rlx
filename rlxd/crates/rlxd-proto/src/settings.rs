//! The settings API: app settings kept in the rlx home and served by rlxd, so every device
//! that connects sees the same values and hears when one changes (the owner's
//! multi-device brief, §4).
//!
//! - `settings.get {key}` → [`Setting`]
//! - `settings.set {key, value, expected_version?}` → [`Setting`]: a write whose
//!   `expected_version` is not the key's version now gets `conflict`.
//! - `settings.list {prefix?}` → [`SettingsList`]
//! - `settings.delete {key, expected_version?}` → [`Setting`], back at version 0
//! - the `settings` topic carries `settings.changed` and `settings.deleted`
//!   ([`SettingsChanged`]).
//!
//! Keys are rlxd's known keys ([`keys`]), whose values have a shape, and dotted keys,
//! `<namespace>.<name>` such as `rlxapp.sidebar`, which hold any JSON value but `null`
//! for later needs. Every value is at most [`MAX_DEPTH`] arrays or objects deep.
//!
//! **Protected keys** ([`is_protected`]): `send_safety_mode` and every key under `policy.`,
//! such as the app-absent policy or a connector's write policy. Only an app-role
//! connection that holds the gate, a paired device, may set or delete them. Any client may
//! set or delete any other key, so a user with the CLI or the TUI and no app can still
//! define agents and preferences.
//!
//! Settings are not for secrets: every client with the token reads every one. Device-local
//! state (SSH profiles, window size and position, per-device notifications, caches) does
//! not belong here either: it stays on the device.

use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::{Value, json};

use crate::Data;
use crate::check::{self, Invalid, drop_null, present};

/// The keys rlxd knows. A key without a `.` that is not one of these gets the error
/// `unknown_setting`; a later rlxd may know more.
pub mod keys {
    /// The app's theme, by name, such as `rlx` or `rlx_light`: a string of 1 to 64
    /// characters. The app owns the names.
    pub const THEME: &str = "theme";
    /// The app's layout: a JSON object whose fields the app owns.
    pub const LAYOUT: &str = "layout";
    /// The sign-off the agent appends to what it sends, such as
    /// `— Ava, Sam's AI Agent`: a string of at most 512 characters.
    pub const AGENT_SIGN_OFF: &str = "agent_sign_off";
    /// How much the app lets an agent send unasked: `{default, connectors?}`, each a
    /// [`send_safety_modes`] name, `connectors` by connector name. A policy, so it is
    /// strict: a field rlxd doesn't know is refused, not ignored. It is protected, and
    /// every change is logged.
    pub const SEND_SAFETY_MODE: &str = "send_safety_mode";

    /// Every key above.
    pub const KNOWN: &[&str] = &[THEME, LAYOUT, AGENT_SIGN_OFF, SEND_SAFETY_MODE];

    /// The protected namespace: every key under it, such as `policy.app_absent`, only an
    /// app-role connection holding the gate may set or delete.
    pub const PROTECTED_PREFIX: &str = "policy.";
}

/// The send-safety modes, safest first. A key's absence means [`DRAFT`].
pub mod send_safety_modes {
    /// Every send is a draft the user approves.
    pub const DRAFT: &str = "draft";
    /// A send that a standing approval covers goes out; any other is a draft.
    pub const STANDING_APPROVAL: &str = "standing_approval";
    /// Sends go out without asking.
    pub const AUTO: &str = "auto";

    /// Every mode, safest first: a move to a later one lowers send safety.
    pub const ALL: &[&str] = &[DRAFT, STANDING_APPROVAL, AUTO];
}

/// The longest key, in bytes.
pub const MAX_KEY: usize = 128;

/// The most arrays or objects a value nests: 32. A scalar is 0 deep, `{"a": 1}` 1.
/// JSON Schema cannot count depth, so the types alone check it.
pub const MAX_DEPTH: usize = 32;

/// Whether only an app-role connection that holds the gate may set or delete `key`:
/// `send_safety_mode`, and every key under [`keys::PROTECTED_PREFIX`].
pub fn is_protected(key: &str) -> bool {
    key == keys::SEND_SAFETY_MODE || key.starts_with(keys::PROTECTED_PREFIX)
}

/// How many arrays or objects `value` nests, counted without recursion.
fn depth(value: &Value) -> usize {
    let mut deepest = 0;
    let mut stack = vec![(value, 0)];
    while let Some((value, at)) = stack.pop() {
        let children: Box<dyn Iterator<Item = &Value>> = match value {
            Value::Array(items) => Box::new(items.iter()),
            Value::Object(fields) => Box::new(fields.values()),
            _ => continue,
        };
        deepest = deepest.max(at + 1);
        stack.extend(children.map(|child| (child, at + 1)));
    }
    deepest
}

/// `settings.get`'s params.
///
/// ```json
/// {"key": "theme"}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(title = "SettingsGetParams")]
#[non_exhaustive]
pub struct SettingsGetParams {
    /// The setting's key, such as `theme` or `rlxapp.sidebar`.
    #[schemars(
        length(min = 1, max = 128),
        regex(pattern = r"^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$")
    )]
    pub key: String,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "SettingsGetParams")]
struct UncheckedGet {
    key: String,
}

/// `settings.set`'s params: the new value, and, for a write that must not overwrite
/// another device's, the version it was made from.
///
/// ```json
/// {"key": "theme", "value": "rlx_light", "expected_version": 3}
/// ```
#[derive(Clone, Debug, PartialEq, JsonSchema)]
#[schemars(title = "SettingsSetParams", transform = value_rules)]
#[non_exhaustive]
pub struct SettingsSetParams {
    /// The setting's key, such as `theme` or `rlxapp.sidebar`.
    #[schemars(
        length(min = 1, max = 128),
        regex(pattern = r"^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$")
    )]
    pub key: String,
    /// The new value. A known key's has that key's shape; a dotted key's is any JSON
    /// value but `null`.
    #[schemars(transform = not_null)]
    pub value: Value,
    /// Write only if the key's version is this now, else get `conflict`: `0` for a key
    /// that has never been set. Absent writes whatever the version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub expected_version: Option<u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "SettingsSetParams")]
struct UncheckedSet {
    key: String,
    value: Value,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    expected_version: Option<u64>,
}

/// `settings.delete`'s params: the key to set back to never set, and, for a delete that
/// must not undo another device's write, the version it was made from.
///
/// ```json
/// {"key": "rlxapp.sidebar", "expected_version": 3}
/// ```
///
/// The key's numbering starts again: its next write is version 1.
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(title = "SettingsDeleteParams")]
#[non_exhaustive]
pub struct SettingsDeleteParams {
    /// The setting's key, such as `theme` or `rlxapp.sidebar`.
    #[schemars(
        length(min = 1, max = 128),
        regex(pattern = r"^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$")
    )]
    pub key: String,
    /// Delete only if the key's version is this now, else get `conflict`. Absent deletes
    /// whatever the version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub expected_version: Option<u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "SettingsDeleteParams")]
struct UncheckedDelete {
    key: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    expected_version: Option<u64>,
}

/// `settings.list`'s params.
///
/// ```json
/// {"prefix": "rlxapp."}
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, JsonSchema)]
#[schemars(title = "SettingsListParams")]
#[non_exhaustive]
pub struct SettingsListParams {
    /// Only the keys that start with this. Absent lists every setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, length(max = 128))]
    pub prefix: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "SettingsListParams")]
struct UncheckedList {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    prefix: Option<String>,
}

/// One setting, as `settings.get` and `settings.set` reply and `settings.list` lists it.
///
/// ```json
/// {"key": "theme", "version": 4, "value": "rlx_light",
///  "updated_at": "2026-11-26T12:00:00.000Z", "by_device": "Sam MacBook"}
/// ```
///
/// A key never set has version 0 and nothing else: the app's default applies, and for
/// `send_safety_mode` that is `draft`.
#[derive(Clone, Debug, PartialEq, JsonSchema)]
#[schemars(title = "Setting", transform = setting_rules)]
#[non_exhaustive]
pub struct Setting {
    /// The setting's key.
    #[schemars(
        length(min = 1, max = 128),
        regex(pattern = r"^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$")
    )]
    pub key: String,
    /// How many times it has been set, from 1; 0 when it never has. `settings.set
    /// {expected_version}` takes it.
    pub version: u64,
    /// Its value. Absent when it was never set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = not_null)]
    pub value: Option<Value>,
    /// When it was last set: RFC 3339, in UTC, to the millisecond. Absent when it was
    /// never set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, extend("format" = "date-time"))]
    pub updated_at: Option<String>,
    /// The paired device that last set it, when an app-role connection did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, length(min = 1))]
    pub by_device: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "Setting")]
struct UncheckedSetting {
    key: String,
    version: u64,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    value: Option<Value>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    updated_at: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    by_device: Option<String>,
}

/// `settings.list`'s reply: every setting that has been set, by key.
///
/// ```json
/// {"settings": [{"key": "theme", "version": 4, "value": "rlx_light",
///                "updated_at": "2026-11-26T12:00:00.000Z"}]}
/// ```
#[derive(Clone, Debug, PartialEq, JsonSchema)]
#[schemars(title = "SettingsList")]
#[non_exhaustive]
pub struct SettingsList {
    /// The settings, in key order.
    pub settings: Vec<Setting>,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "SettingsList")]
struct UncheckedSettingsList {
    settings: Vec<Setting>,
}

/// `settings.changed`'s and `settings.deleted`'s data, on the `settings` topic: a setting
/// was set, at `version`, or deleted, from `version`. Read a new value with `settings.get`.
///
/// ```json
/// {"key": "theme", "version": 4, "by_device": "Sam MacBook"}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(title = "SettingsChanged")]
#[non_exhaustive]
pub struct SettingsChanged {
    /// The setting's key.
    #[schemars(
        length(min = 1, max = 128),
        regex(pattern = r"^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$")
    )]
    pub key: String,
    /// Its version now, from 1; for `settings.deleted`, the version it was deleted at.
    #[schemars(range(min = 1))]
    pub version: u64,
    /// The paired device that set or deleted it, when an app-role connection did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, length(min = 1))]
    pub by_device: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "SettingsChanged")]
struct UncheckedChanged {
    key: String,
    version: u64,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    by_device: Option<String>,
}

/// Whether `key` is a key's shape: a known key's name, or dotted snake_case names, at most
/// [`MAX_KEY`] bytes. Whether rlxd knows a key without a dot is rlxd's to say.
pub fn check_key(field: &str, key: &str) -> Result<(), Invalid> {
    check::non_empty(field, key)?;
    if key.len() > MAX_KEY {
        return Err(Invalid::new(
            field,
            format!("is longer than {MAX_KEY} bytes"),
        ));
    }
    if !key.split('.').all(check::is_snake) {
        return Err(Invalid::new(
            field,
            format!("{key:?} is not a setting key, such as theme or rlxapp.sidebar"),
        ));
    }
    Ok(())
}

/// Whether `value` has `key`'s shape: a known key's, or for any other key any value but
/// `null`.
pub fn check_value(field: &str, key: &str, value: &Value) -> Result<(), Invalid> {
    let wrong = |shape: &str| Err(Invalid::new(field, format!("{key} takes {shape}")));
    let deep = depth(value);
    if deep > MAX_DEPTH {
        return Err(Invalid::new(
            field,
            format!("nests {deep} arrays or objects deep, more than {MAX_DEPTH}"),
        ));
    }
    match key {
        keys::THEME => match value.as_str() {
            Some(name) if (1..=64).contains(&name.chars().count()) => Ok(()),
            _ => wrong("a theme's name, a string of 1 to 64 characters"),
        },
        keys::LAYOUT => match value {
            Value::Object(_) => Ok(()),
            _ => wrong("a JSON object"),
        },
        keys::AGENT_SIGN_OFF => match value.as_str() {
            Some(text) if text.chars().count() <= 512 => Ok(()),
            _ => wrong("a string of at most 512 characters"),
        },
        keys::SEND_SAFETY_MODE => check_send_safety(value).or_else(|_| {
            wrong(
                "{default, connectors?} and no other field: each a mode, draft, \
                 standing_approval or auto, and connectors an object from connector names \
                 to modes",
            )
        }),
        _ if value.is_null() => wrong("any JSON value but null"),
        _ => Ok(()),
    }
}

fn check_send_safety(value: &Value) -> Result<(), ()> {
    let mode = |v: &Value| {
        v.as_str()
            .is_some_and(|m| send_safety_modes::ALL.contains(&m))
    };
    let Value::Object(fields) = value else {
        return Err(());
    };
    // A policy is read whole: a field this rlxd doesn't know is refused, not ignored.
    if fields
        .keys()
        .any(|field| field != "default" && field != "connectors")
    {
        return Err(());
    }
    if !fields.get("default").is_some_and(mode) {
        return Err(());
    }
    match fields.get("connectors") {
        None => Ok(()),
        Some(Value::Object(connectors)) => {
            let fine = connectors
                .iter()
                .all(|(name, m)| !name.is_empty() && mode(m));
            if fine { Ok(()) } else { Err(()) }
        }
        Some(_) => Err(()),
    }
}

/// Whether changing `send_safety_mode` from `before` to `after` lowers send safety for any
/// connector: the default, or a connector named in either, whose mode in `after` (its own,
/// else the default) is later in [`send_safety_modes::ALL`] than in `before`. `before` is
/// `None` for a key never set, which means `draft` everywhere. False when `after` is not
/// a send-safety value.
pub fn send_safety_lowers(before: Option<&Value>, after: &Value) -> bool {
    let draft = json!({ "default": send_safety_modes::DRAFT });
    let before = before.unwrap_or(&draft);
    if check_send_safety(after).is_err() {
        return false;
    }
    let rank = |mode: Option<&Value>| {
        let mode = mode?.as_str()?;
        send_safety_modes::ALL.iter().position(|m| *m == mode)
    };
    let connectors = |value: &Value| -> Vec<String> {
        value
            .get("connectors")
            .and_then(Value::as_object)
            .map(|c| c.keys().cloned().collect())
            .unwrap_or_default()
    };
    // A connector's mode: its own, else the default.
    let mode_of = |value: &Value, name: Option<&str>| {
        let own = name.and_then(|n| value.get("connectors")?.get(n));
        rank(own.or_else(|| value.get("default")))
    };
    let mut names: Vec<String> = connectors(before);
    names.extend(connectors(after));
    std::iter::once(None)
        .chain(names.iter().map(|n| Some(n.as_str())))
        .any(|name| {
            let (was, is) = (mode_of(before, name), mode_of(after, name));
            // A `before` that is no send-safety value reads as draft, the safest.
            is > Some(was.unwrap_or(0))
        })
}

impl SettingsGetParams {
    /// `settings.get` for `key`. Refuses what is not a key's shape.
    pub fn new(key: impl Into<String>) -> Result<Self, Invalid> {
        let params = Self { key: key.into() };
        params.validate()?;
        Ok(params)
    }

    /// Whether these params keep every rule of `settings-get.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        check_key("key", &self.key)
    }
}

impl SettingsSetParams {
    /// `settings.set` of `key` to `value`, whatever its version. Refuses a key or a
    /// value of the wrong shape.
    pub fn new(key: impl Into<String>, value: Value) -> Result<Self, Invalid> {
        let params = Self {
            key: key.into(),
            value,
            expected_version: None,
        };
        params.validate()?;
        Ok(params)
    }

    /// These params, writing only if the key's version is `version` now.
    #[must_use]
    pub fn expecting(mut self, version: u64) -> Self {
        self.expected_version = Some(version);
        self
    }

    /// Whether these params keep every rule of `settings-set.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        check_key("key", &self.key)?;
        check_value("value", &self.key, &self.value)
    }
}

impl SettingsDeleteParams {
    /// `settings.delete` of `key`, whatever its version. Refuses what is not a key's shape.
    pub fn new(key: impl Into<String>) -> Result<Self, Invalid> {
        let params = Self {
            key: key.into(),
            expected_version: None,
        };
        params.validate()?;
        Ok(params)
    }

    /// These params, deleting only if the key's version is `version` now.
    #[must_use]
    pub fn expecting(mut self, version: u64) -> Self {
        self.expected_version = Some(version);
        self
    }

    /// Whether these params keep every rule of `settings-delete.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        check_key("key", &self.key)
    }
}

impl SettingsListParams {
    /// `settings.list` of every setting.
    pub fn new() -> Self {
        Self { prefix: None }
    }

    /// These params, listing only the keys that start with `prefix`.
    #[must_use]
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into());
        self
    }

    /// Whether these params keep every rule of `settings-list.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        match &self.prefix {
            Some(prefix) if prefix.len() > MAX_KEY => Err(Invalid::new(
                "prefix",
                format!("is longer than {MAX_KEY} bytes"),
            )),
            _ => Ok(()),
        }
    }
}

impl Setting {
    /// `key`, never set: version 0, and nothing else.
    pub fn unset(key: impl Into<String>) -> Result<Self, Invalid> {
        let setting = Self {
            key: key.into(),
            version: 0,
            value: None,
            updated_at: None,
            by_device: None,
        };
        setting.validate()?;
        Ok(setting)
    }

    /// `key` at `version`, holding `value` since `updated_at`, set by `by_device` when an
    /// app-role connection set it.
    pub fn new(
        key: impl Into<String>,
        version: u64,
        value: Value,
        updated_at: impl Into<String>,
        by_device: Option<String>,
    ) -> Result<Self, Invalid> {
        let setting = Self {
            key: key.into(),
            version,
            value: Some(value),
            updated_at: Some(updated_at.into()),
            by_device,
        };
        setting.validate()?;
        Ok(setting)
    }

    /// Whether this setting keeps every rule of `setting.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        check_key("key", &self.key)?;
        if let Some(device) = &self.by_device {
            check::non_empty("by_device", device)?;
        }
        if self.version == 0 {
            if self.value.is_some() || self.updated_at.is_some() || self.by_device.is_some() {
                return Err(Invalid::new(
                    "version",
                    "a setting at version 0 was never set: it has no value, updated_at or \
                     by_device",
                ));
            }
            return Ok(());
        }
        let Some(value) = &self.value else {
            return Err(Invalid::new("value", "a setting that was set has a value"));
        };
        check_value("value", &self.key, value)?;
        match &self.updated_at {
            Some(time) => check::date_time("updated_at", time),
            None => Err(Invalid::new(
                "updated_at",
                "a setting that was set says when",
            )),
        }
    }
}

impl SettingsList {
    /// A list of `settings`.
    pub fn new(settings: Vec<Setting>) -> Result<Self, Invalid> {
        let list = Self { settings };
        list.validate()?;
        Ok(list)
    }

    /// Whether this list keeps every rule of `settings-list-reply.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        for (at, setting) in self.settings.iter().enumerate() {
            setting
                .validate()
                .map_err(|e| e.within(&format!("settings.{at}")))?;
        }
        Ok(())
    }
}

impl SettingsChanged {
    /// `key` changed to `version`, by `by_device` when an app-role connection set it.
    pub fn new(
        key: impl Into<String>,
        version: u64,
        by_device: Option<String>,
    ) -> Result<Self, Invalid> {
        let changed = Self {
            key: key.into(),
            version,
            by_device,
        };
        changed.validate()?;
        Ok(changed)
    }

    /// This change as an event's `data`.
    pub fn to_data(&self) -> Result<Data, Invalid> {
        self.validate()?;
        match serde_json::to_value(self) {
            Ok(Value::Object(data)) => Ok(data),
            _ => Err(Invalid::new("data", "did not print as an object")),
        }
    }

    /// Whether this change keeps every rule of `settings-changed.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        check_key("key", &self.key)?;
        check::at_least("version", self.version, 1)?;
        if let Some(device) = &self.by_device {
            check::non_empty("by_device", device)?;
        }
        Ok(())
    }
}

/// The schema half of a value that may be anything but `null`.
fn not_null(schema: &mut Schema) {
    schema.insert("not".to_owned(), json!({"type": "null"}));
}

/// Each known key's value shape, as `if key is K then value is …` rules.
fn value_shapes() -> Vec<Value> {
    let modes = json!(send_safety_modes::ALL);
    let shapes = [
        (
            keys::THEME,
            json!({"type": "string", "minLength": 1, "maxLength": 64}),
        ),
        (keys::LAYOUT, json!({"type": "object"})),
        (
            keys::AGENT_SIGN_OFF,
            json!({"type": "string", "maxLength": 512}),
        ),
        (
            keys::SEND_SAFETY_MODE,
            json!({
                "type": "object",
                "properties": {
                    "default": {"enum": modes},
                    "connectors": {
                        "type": "object",
                        "propertyNames": {"minLength": 1},
                        "additionalProperties": {"enum": modes},
                    },
                },
                "required": ["default"],
                "additionalProperties": false,
            }),
        ),
    ];
    shapes
        .into_iter()
        .map(|(key, shape)| {
            json!({
                "if": {"properties": {"key": {"const": key}}, "required": ["key"]},
                "then": {"properties": {"value": shape}},
            })
        })
        .collect()
}

/// The schema half of `settings.set`'s rules: a known key's value has its shape.
fn value_rules(schema: &mut Schema) {
    schema.insert("allOf".to_owned(), Value::Array(value_shapes()));
}

/// The schema half of a setting's rules: a known key's value has its shape, and version 0
/// carries nothing else while a later one carries its value and when it was set.
fn setting_rules(schema: &mut Schema) {
    let mut rules = value_shapes();
    rules.push(json!({
        "if": {"properties": {"version": {"const": 0}}, "required": ["version"]},
        "then": {"not": {"anyOf": [
            {"required": ["value"]},
            {"required": ["updated_at"]},
            {"required": ["by_device"]},
        ]}},
        "else": {"required": ["value", "updated_at"]},
    }));
    schema.insert("allOf".to_owned(), Value::Array(rules));
}

macro_rules! checked_serde {
    ($type:ident, $unchecked:ident) => {
        impl Serialize for $type {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                self.validate().map_err(serde::ser::Error::custom)?;
                $unchecked::serialize(self, serializer)
            }
        }

        /// Reads an object only, as the schema does.
        impl<'de> Deserialize<'de> for $type {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let object = Data::deserialize(deserializer)?;
                let value =
                    $unchecked::deserialize(Value::Object(object)).map_err(de::Error::custom)?;
                value.validate().map_err(de::Error::custom)?;
                Ok(value)
            }
        }
    };
}

checked_serde!(SettingsGetParams, UncheckedGet);
checked_serde!(SettingsSetParams, UncheckedSet);
checked_serde!(SettingsDeleteParams, UncheckedDelete);
checked_serde!(SettingsListParams, UncheckedList);
checked_serde!(Setting, UncheckedSetting);
checked_serde!(SettingsList, UncheckedSettingsList);
checked_serde!(SettingsChanged, UncheckedChanged);
