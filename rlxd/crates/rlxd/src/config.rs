//! rlxd's config, `<home>/rlxd/config.toml`, read once when rlxd starts. The file is
//! optional, and so is every key in it:
//!
//! ```toml
//! [gate]
//! dev_mode = true   # gate dev mode, as `rlxd serve --gate-dev-mode` (handoff 0.10.3, 5.1)
//! ```
//!
//! A key rlxd does not know is logged and ignored. That includes `gate.requirement`: the
//! macOS code requirement, the Linux binary and the Windows publisher a gate peer must
//! match are compiled into rlxd, and no config key, variable or flag of a release build
//! changes them (see `peer`).

use std::fs;
use std::io;
use std::path::Path;

use serde::Deserialize;

use crate::log::warning;

/// What the config says. Absent keys keep their defaults.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Config {
    /// `gate.dev_mode`: accept an unsigned app as the gate, the credential still required.
    pub gate_dev_mode: bool,
}

#[derive(Deserialize)]
struct File {
    #[serde(default)]
    gate: Option<toml::Table>,
    #[serde(flatten)]
    rest: toml::Table,
}

impl Config {
    /// The config at `path`: the defaults when the file is missing, an error when it is
    /// there but not valid TOML, or a key has the wrong type.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        Self::parse(&text).map_err(|e| format!("{} is not valid: {e}", path.display()))
    }

    fn parse(text: &str) -> Result<Self, String> {
        let file: File = toml::from_str(text).map_err(|e| e.message().to_owned())?;
        let mut config = Self::default();
        for key in file.rest.keys() {
            warning!("config: ignoring {key}, which rlxd does not know");
        }
        for (key, value) in file.gate.unwrap_or_default() {
            match key.as_str() {
                "dev_mode" => {
                    config.gate_dev_mode = value
                        .as_bool()
                        .ok_or_else(|| "gate.dev_mode must be true or false".to_owned())?;
                }
                "requirement" => warning!(
                    "config: ignoring gate.requirement: the gate's peer requirement is \
                     compiled into rlxd, and no config changes it"
                ),
                other => warning!("config: ignoring gate.{other}, which rlxd does not know"),
            }
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_or_missing_config_is_the_defaults() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
        let missing = std::env::temp_dir().join("rlxd-no-such-config.toml");
        assert_eq!(Config::load(&missing).unwrap(), Config::default());
    }

    #[test]
    fn gate_dev_mode_is_read_and_must_be_a_boolean() {
        let on = Config::parse("[gate]\ndev_mode = true\n").unwrap();
        assert!(on.gate_dev_mode);
        let dotted = Config::parse("gate.dev_mode = true\n").unwrap();
        assert!(dotted.gate_dev_mode);
        assert!(
            !Config::parse("[gate]\ndev_mode = false\n")
                .unwrap()
                .gate_dev_mode
        );
        assert!(Config::parse("[gate]\ndev_mode = \"yes\"\n").is_err());
        assert!(Config::parse("not toml at all [").is_err());
    }

    #[test]
    fn a_requirement_in_the_config_changes_nothing() {
        let text = "[gate]\nrequirement = 'identifier \"anything\"'\n";
        assert_eq!(Config::parse(text).unwrap(), Config::default());
    }
}
