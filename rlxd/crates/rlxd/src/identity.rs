//! Who an account is on a connector (task #14, step 2; the owner's brief, §2): a Slack
//! workspace and user, a Gmail address. Read with one `connectors.call` of the
//! connector's own read tool ([`crate::connector_map`] makes it), and never logged.
//!
//! The table names, for each connector that can say, the one tool to call, its fixed
//! input (never anything of the user's) and how its result reads. A connector not in it
//! has no identity. A release build's table holds no entry until the owner's probe names
//! each connector's tool and the shape of its answer (`connectors.call` is off in a
//! release build anyway); a test build's adds the fixture connector's.

use rlxd_proto::ConnectorIdentity;
use serde_json::{Map, Value};

/// One connector's way to say who the account is.
#[derive(Clone, Copy, Debug)]
pub struct Source {
    /// The connector, such as `claude.ai Slack`.
    pub connector: &'static str,
    /// Its read tool, as Claude Code names it (`mcp__<server>__<tool>`).
    pub tool: &'static str,
    /// The call's input: fixed, so the call sends nothing of the user's.
    pub input: fn() -> Map<String, Value>,
    /// The identity the tool's result content says, if it says one.
    pub read: fn(&Value) -> Option<ConnectorIdentity>,
}

/// The table's entry for `connector`, if it has one.
pub fn source_of(connector: &str) -> Option<Source> {
    sources()
        .into_iter()
        .find(|source| source.connector == connector)
}

/// The table.
fn sources() -> Vec<Source> {
    // Release entries land once the owner's probe has named each connector's tool.
    test_sources()
}

/// A result's content as one JSON object in its first text item, read for `workspace`,
/// `user` and `email`: `None` when it says none of them. (A release build's table, empty
/// until the owner's probe, has no entry that reads this way yet.)
#[cfg_attr(not(feature = "debug-methods"), allow(dead_code))]
pub fn read_json_text(content: &Value) -> Option<ConnectorIdentity> {
    let text = content
        .as_array()?
        .iter()
        .find(|item| item["type"] == "text")?["text"]
        .as_str()?;
    let said: Value = serde_json::from_str(text).ok()?;
    let field = |name: &str| {
        said[name]
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty() && value.len() <= MAX_FIELD)
            .map(str::to_owned)
    };
    let mut identity = ConnectorIdentity::default();
    identity.workspace = field("workspace");
    identity.user = field("user");
    identity.email = field("email");
    (identity != ConnectorIdentity::default()).then_some(identity)
}

/// The longest value an identity keeps of each field.
#[cfg_attr(not(feature = "debug-methods"), allow(dead_code))]
const MAX_FIELD: usize = 256;

/// Test builds only: the fixture connector's entry, `claude.ai Fixture`'s `whoami`.
#[cfg(feature = "debug-methods")]
fn test_sources() -> Vec<Source> {
    vec![Source {
        connector: "claude.ai Fixture",
        tool: "mcp__claude_ai_Fixture__whoami",
        input: Map::new,
        read: read_json_text,
    }]
}

#[cfg(not(feature = "debug-methods"))]
fn test_sources() -> Vec<Source> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_result_reads_as_its_text_items_json() {
        let content = json!([{"type": "text",
            "text": "{\"workspace\": \"Fixture Works\", \"user\": \" ada \", \"other\": 1}"}]);
        let identity = read_json_text(&content).unwrap();
        assert_eq!(identity.workspace.as_deref(), Some("Fixture Works"));
        assert_eq!(identity.user.as_deref(), Some("ada"));
        assert_eq!(identity.email, None);
        for nothing in [
            json!([]),
            json!([{"type": "text", "text": "not json"}]),
            json!([{"type": "text", "text": "{\"workspace\": \"\"}"}]),
            json!([{"type": "image", "data": "…"}]),
            json!({"type": "text"}),
        ] {
            assert_eq!(read_json_text(&nothing), None, "{nothing}");
        }
        let long = "x".repeat(MAX_FIELD + 1);
        let content = json!([{"type": "text", "text": json!({"user": long}).to_string()}]);
        assert_eq!(read_json_text(&content), None);
    }
}
