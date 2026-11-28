//! The app's environment for Claude Code (handoff 0.10.3, 4.3): `PATH`,
//! `CLAUDE_CONFIG_DIR` and `CLAUDE_SECURESTORAGE_CONFIG_DIR`, which the app reads once from
//! a login shell and hands to rlxd, which passes them raw to the brain and to every Claude
//! Code it starts. They decide which `claude` a gated run starts and where it reads its
//! settings, so only the gate holder sets them.
//!
//! - `env.set {<key>: <value>, …}` → [`EnvValues`], every value now: only on an app-role
//!   connection that holds the gate, else `not_gate`. A key it doesn't name keeps its
//!   value, and an empty value removes the key.
//! - `env.get` → [`EnvValues`]: open to every client.
//! - `hello {…, env}`: as `env.set`, in the hello that takes the app role; any other
//!   hello with `env` is refused with `not_gate`.
//!
//! Any key but [`keys::ALL`] is refused with `bad_env_key`, and so is the whole request:
//! nothing changes ([`EnvValues::from_object`]). A value is a string of at most
//! [`MAX_VALUE`] bytes with no NUL, kept as it was sent: a relative path stays relative.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Data;
use crate::check::drop_null;

/// The keys the app may set: nothing else.
pub mod keys {
    /// The app's login-shell `PATH`, on which rlxd finds `claude`.
    pub const PATH: &str = "PATH";
    /// Where Claude Code keeps its settings and sign-in.
    pub const CLAUDE_CONFIG_DIR: &str = "CLAUDE_CONFIG_DIR";
    /// Where Claude Code's secure storage keeps its config.
    pub const CLAUDE_SECURESTORAGE_CONFIG_DIR: &str = "CLAUDE_SECURESTORAGE_CONFIG_DIR";

    /// Every key above.
    pub const ALL: &[&str] = &[PATH, CLAUDE_CONFIG_DIR, CLAUDE_SECURESTORAGE_CONFIG_DIR];
}

/// The longest value, in bytes: 64 KiB, room for any `PATH`.
pub const MAX_VALUE: usize = 65_536;

/// The app's environment: `env.set`'s params, `env.get`'s and `env.set`'s reply, and
/// `<home>/rlxd/env.json`. A key not set is absent.
///
/// ```json
/// {"PATH": "/opt/homebrew/bin:/usr/bin:/bin", "CLAUDE_CONFIG_DIR": "/Users/u/.claude-work"}
/// ```
///
/// Reading one checks it whole, as its schema does: no other key, no `null`, and every
/// value at most [`MAX_VALUE`] bytes with no NUL.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "EnvValues")]
#[non_exhaustive]
pub struct EnvValues {
    /// The app's login-shell `PATH`.
    #[serde(rename = "PATH", default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, length(max = 65536), regex(pattern = r"^[^\x00]*$"))]
    pub path: Option<String>,
    /// `CLAUDE_CONFIG_DIR`.
    #[serde(
        rename = "CLAUDE_CONFIG_DIR",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(transform = drop_null, length(max = 65536), regex(pattern = r"^[^\x00]*$"))]
    pub claude_config_dir: Option<String>,
    /// `CLAUDE_SECURESTORAGE_CONFIG_DIR`.
    #[serde(
        rename = "CLAUDE_SECURESTORAGE_CONFIG_DIR",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(transform = drop_null, length(max = 65536), regex(pattern = r"^[^\x00]*$"))]
    pub claude_securestorage_config_dir: Option<String>,
}

impl<'de> Deserialize<'de> for EnvValues {
    /// An object of strings only, read as a map: an array or a `null` is refused, as the
    /// schema refuses them, then every key and value checked.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut read = BTreeMap::<String, String>::deserialize(deserializer)?;
        if let Some(key) = read.keys().find(|key| !keys::ALL.contains(&key.as_str())) {
            return Err(serde::de::Error::custom(EnvRefusal::BadKey(key.clone())));
        }
        let values = Self {
            path: read.remove(keys::PATH),
            claude_config_dir: read.remove(keys::CLAUDE_CONFIG_DIR),
            claude_securestorage_config_dir: read.remove(keys::CLAUDE_SECURESTORAGE_CONFIG_DIR),
        };
        values.check().map_err(serde::de::Error::custom)?;
        Ok(values)
    }
}

/// Why a set of values is refused. Nothing of it may be applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvRefusal {
    /// A key that is not one of [`keys::ALL`]: `bad_env_key`.
    BadKey(String),
    /// A value that is not a string, is longer than [`MAX_VALUE`] or holds a NUL:
    /// `invalid_params`, with why.
    BadValue {
        /// The key.
        key: String,
        /// Why.
        why: String,
    },
}

impl std::fmt::Display for EnvRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadKey(key) => write!(
                f,
                "{key:?} is not one of the keys the app may set: {}",
                keys::ALL.join(", ")
            ),
            Self::BadValue { key, why } => write!(f, "{key}'s value {why}"),
        }
    }
}

impl std::error::Error for EnvRefusal {}

/// A change to the values: each key given to set, or with `None` to remove.
pub type EnvChange = BTreeMap<String, Option<String>>;

impl EnvValues {
    /// The change `object` asks for: `env.set`'s params or `hello`'s `env`, every key and
    /// value checked first, so a refusal changes nothing. An empty string removes its key.
    pub fn from_object(object: &Data) -> Result<EnvChange, EnvRefusal> {
        if let Some(key) = object.keys().find(|key| !keys::ALL.contains(&key.as_str())) {
            return Err(EnvRefusal::BadKey(key.clone()));
        }
        let mut change = EnvChange::new();
        for (key, value) in object {
            let Value::String(text) = value else {
                return Err(EnvRefusal::BadValue {
                    key: key.clone(),
                    why: "is not a string".to_owned(),
                });
            };
            check_value(key, text)?;
            change.insert(key.clone(), (!text.is_empty()).then(|| text.clone()));
        }
        Ok(change)
    }

    /// [`EnvValues::from_object`], for `hello`'s `env`.
    pub fn from_map(map: &BTreeMap<String, String>) -> Result<EnvChange, EnvRefusal> {
        let object: Data = map
            .iter()
            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
            .collect();
        Self::from_object(&object)
    }

    /// These values with `change` applied.
    pub fn with(&self, change: &EnvChange) -> Self {
        let mut values = self.clone();
        for (key, value) in change {
            if let Some(slot) = values.slot(key) {
                *slot = value.clone();
            }
        }
        values
    }

    /// Every key set, and its value, in key order.
    pub fn pairs(&self) -> Vec<(&'static str, &str)> {
        [
            (keys::CLAUDE_CONFIG_DIR, &self.claude_config_dir),
            (
                keys::CLAUDE_SECURESTORAGE_CONFIG_DIR,
                &self.claude_securestorage_config_dir,
            ),
            (keys::PATH, &self.path),
        ]
        .into_iter()
        .filter_map(|(key, value)| value.as_deref().map(|value| (key, value)))
        .collect()
    }

    /// Checks every value, as [`EnvValues::from_object`] does: for values read from a file.
    pub fn check(&self) -> Result<(), EnvRefusal> {
        self.pairs()
            .into_iter()
            .try_for_each(|(key, value)| check_value(key, value))
    }

    fn slot(&mut self, key: &str) -> Option<&mut Option<String>> {
        match key {
            keys::PATH => Some(&mut self.path),
            keys::CLAUDE_CONFIG_DIR => Some(&mut self.claude_config_dir),
            keys::CLAUDE_SECURESTORAGE_CONFIG_DIR => {
                Some(&mut self.claude_securestorage_config_dir)
            }
            _ => None,
        }
    }
}

/// A value: at most [`MAX_VALUE`] bytes, and no NUL.
fn check_value(key: &str, value: &str) -> Result<(), EnvRefusal> {
    let why = if value.len() > MAX_VALUE {
        format!("is {} bytes, more than {MAX_VALUE}", value.len())
    } else if value.contains('\0') {
        "holds a NUL".to_owned()
    } else {
        return Ok(());
    };
    Err(EnvRefusal::BadValue {
        key: key.to_owned(),
        why,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn object(value: Value) -> Data {
        match value {
            Value::Object(object) => object,
            _ => panic!("an object"),
        }
    }

    #[test]
    fn any_other_key_refuses_the_whole_change() {
        let refused = EnvValues::from_object(&object(json!({"PATH": "/bin", "HOME": "x"})));
        assert_eq!(refused, Err(EnvRefusal::BadKey("HOME".to_owned())));
    }

    #[test]
    fn values_are_strings_bounded_and_without_nul() {
        for (value, why) in [
            (json!(1), "is not a string"),
            (json!("a\u{0}b"), "holds a NUL"),
            (json!("x".repeat(MAX_VALUE + 1)), "more than"),
        ] {
            let refused = EnvValues::from_object(&object(json!({"PATH": value})));
            let Err(EnvRefusal::BadValue { key, why: said }) = refused else {
                panic!("{refused:?}")
            };
            assert_eq!(key, "PATH");
            assert!(said.contains(why), "{said}");
        }
        assert!(EnvValues::from_object(&object(json!({"PATH": "x".repeat(MAX_VALUE)}))).is_ok());
    }

    #[test]
    fn a_change_sets_keeps_and_removes() {
        let values = EnvValues::default().with(
            &EnvValues::from_object(&object(json!({"PATH": "/bin", "CLAUDE_CONFIG_DIR": "cc"})))
                .unwrap(),
        );
        let values = values
            .with(&EnvValues::from_object(&object(json!({"CLAUDE_CONFIG_DIR": ""}))).unwrap());
        assert_eq!(values.path.as_deref(), Some("/bin"), "kept: not named");
        assert_eq!(values.claude_config_dir, None, "removed: empty");
        assert_eq!(
            serde_json::to_value(&values).unwrap(),
            json!({"PATH": "/bin"})
        );
    }
}
