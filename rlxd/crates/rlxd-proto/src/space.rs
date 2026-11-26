//! Spaces (handoff 0.10.3, 2.5): a home for runs that is not a project folder.
//! `space.ensure {name}` makes `<home>/spaces/<name>/`; `space.erase {name}` deletes it,
//! the Claude Code sessions of its runs, and rlxd's records of them.

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::Value;

use crate::Data;
use crate::check::Invalid;

/// The space a run works in when it names none.
pub const DEFAULT_SPACE: &str = "default";

/// The longest space name.
pub const MAX_SPACE_NAME: usize = 64;

/// The pattern a space name matches, besides being at most [`MAX_SPACE_NAME`] bytes and
/// not one of Windows' device names.
pub const SPACE_NAME_PATTERN: &str = r"^[a-z0-9][a-z0-9_-]*(\.[a-z0-9_-]+)*$";

/// Whether `name` may name a space: 1 to 64 of lower-case ASCII letters, digits, `-`, `_`
/// and `.`, starting with a letter or digit, with no `..` and no `.` at the end, and not a
/// name Windows keeps for a device (`con`, `nul`, `com1`, …, also with an extension). It
/// names a folder, so it holds no separator and no way up, and two names never name one
/// folder on a file system that ignores case.
pub fn check_name(field: &str, name: &str) -> Result<(), Invalid> {
    let rule = "a space name is 1 to 64 lower-case letters, digits, '-', '_' and '.', \
                starting with a letter or digit, with no '..', no '.' at the end, and not \
                a Windows device name";
    let shaped = !name.is_empty()
        && name.len() <= MAX_SPACE_NAME
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_.".contains(&b))
        && !name.contains("..")
        && !name.ends_with('.');
    if !shaped {
        return Err(Invalid::new(field, format!("{rule}, not {name:?}")));
    }
    let stem = name.split('.').next().unwrap_or(name);
    let device = matches!(stem, "con" | "prn" | "aux" | "nul")
        || ((stem.starts_with("com") || stem.starts_with("lpt"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    if device {
        return Err(Invalid::new(field, format!("{rule}: {name:?} is one")));
    }
    Ok(())
}

/// `space.ensure`'s and `space.erase`'s params: the space, by name.
///
/// ```json
/// {"name": "default"}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(title = "SpaceParams")]
#[non_exhaustive]
pub struct SpaceParams {
    /// The space: 1 to 64 lower-case letters, digits, `-`, `_` and `.`, starting with a
    /// letter or digit, with no `..`, no `.` at the end, and not a Windows device name
    /// such as `con` or `nul`.
    #[schemars(
        regex(pattern = r"^[a-z0-9][a-z0-9_-]*(\.[a-z0-9_-]+)*$"),
        length(max = 64)
    )]
    pub name: String,
}

/// The serde code of `SpaceParams`'s fields, unchecked: see `UncheckedRunStart`.
#[derive(Serialize, Deserialize)]
#[serde(remote = "SpaceParams")]
struct UncheckedSpace {
    name: String,
}

impl SpaceParams {
    /// The space `name`, or why that can't name one.
    pub fn new(name: impl Into<String>) -> Result<Self, Invalid> {
        let params = Self { name: name.into() };
        params.validate()?;
        Ok(params)
    }

    /// Whether the name keeps the rule of [`check_name`].
    pub fn validate(&self) -> Result<(), Invalid> {
        check_name("name", &self.name)
    }
}

impl Serialize for SpaceParams {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(serde::ser::Error::custom)?;
        UncheckedSpace::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for SpaceParams {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let object = Data::deserialize(deserializer)?;
        let params =
            UncheckedSpace::deserialize(Value::Object(object)).map_err(de::Error::custom)?;
        params.validate().map_err(de::Error::custom)?;
        Ok(params)
    }
}

/// `space.ensure`'s reply: the space's folder, made if it was not there.
///
/// ```json
/// {"name": "default", "path": "/Users/u/.aisquare/spaces/default"}
/// ```
///
/// The folder is the runs' working directory. It holds an empty project
/// `.claude/settings.json`, so that `--setting-sources project` has a project, and
/// nothing else of rlxd's: it is not a git repository and not an onboarded project.
/// `path` is absolute and resolved, as the runs' Claude Code reports its `cwd`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "SpaceEnsureData")]
#[non_exhaustive]
pub struct SpaceEnsureData {
    /// The space.
    pub name: String,
    /// Its folder.
    pub path: String,
}

impl SpaceEnsureData {
    /// The space `name`, at `path`.
    pub fn new(name: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            path: path.into(),
        }
    }
}

/// `space.erase`'s reply: every path it removed, and the links inside the space that went
/// as links. It refuses with `runs_live` while a run in the space is queued or running,
/// and with `link_refused` when a link is on the way to what it removes (`<home>/spaces`,
/// the space, a Claude Code config directory, its `projects` or the space's folder
/// there), and removes nothing then.
///
/// ```json
/// {"removed": ["/Users/u/.aisquare/spaces/default",
///              "/Users/u/.claude/projects/-Users-u--aisquare-spaces-default/<run_id>.jsonl"]}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "SpaceEraseData")]
#[non_exhaustive]
pub struct SpaceEraseData {
    /// The paths removed: the space's folder, its runs' session files, and rlxd's records
    /// of its runs.
    pub removed: Vec<String>,
    /// The links that were inside the space, removed as links: what they named is left as
    /// it was.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
}

impl SpaceEraseData {
    /// The reply that names `removed`, and the `links` that went as links.
    pub fn new(removed: Vec<String>, links: Vec<String>) -> Self {
        Self { removed, links }
    }
}
