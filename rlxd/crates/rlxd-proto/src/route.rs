//! A run's route by its connectors (#15): the `detail` of each refusal `run.start` gives
//! when no account can take a run by its `connectors` (or its pinned `account` lacks one).
//!
//! - `ambiguous_account`: [`AmbiguousAccount`];
//! - `connector_account_limited`: [`ConnectorAccountLimited`];
//! - `connector_unavailable`: [`ConnectorUnavailable`].
//!
//! Each names accounts by their rlx slot, as `connectors.list` does, and never by a home.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::check::drop_null;
use crate::connector::{ConnectorIdentity, ConnectorState, all_required};
use crate::envelope::Data;

/// `ambiguous_account`'s detail: two or more accounts hold `connector` connected, and
/// their identities on it aren't all known and the same, so rlxd won't pick one. The
/// client asks the user once and pins `account`.
///
/// ```json
/// {"connector": "claude.ai Slack",
///  "candidates": [{"account": 1, "identity": {"workspace": "Acme"}},
///                 {"account": 3, "identity": null}]}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "AmbiguousAccount")]
#[non_exhaustive]
pub struct AmbiguousAccount {
    /// The connector, by its id.
    pub connector: String,
    /// Every account that holds it connected, by slot.
    pub candidates: Vec<Candidate>,
}

/// One account `ambiguous_account` lists. `identity` is always there, `null` when
/// unknown.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(transform = all_required)]
#[non_exhaustive]
pub struct Candidate {
    /// The account's rlx slot.
    #[schemars(range(min = 1))]
    pub account: u32,
    /// Who the account is on the connector, or `null` when rlxd hasn't read it.
    pub identity: Option<ConnectorIdentity>,
}

impl Candidate {
    /// Account `account`, who it is there `identity`.
    pub fn new(account: u32, identity: Option<ConnectorIdentity>) -> Self {
        Self { account, identity }
    }
}

impl AmbiguousAccount {
    /// `connector`, held by `candidates`.
    pub fn new(connector: impl Into<String>, candidates: Vec<Candidate>) -> Self {
        Self {
            connector: connector.into(),
            candidates,
        }
    }
}

/// `connector_account_limited`'s detail: every account that holds `connector` (with the
/// run's other connectors) connected is usage-limited. rlxd starts the run on no other
/// account: the client says to wait, or to connect it on another account.
///
/// ```json
/// {"connector": "claude.ai Slack", "account": 1, "resets_at": "2026-11-28T17:00:00Z"}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "ConnectorAccountLimited")]
#[non_exhaustive]
pub struct ConnectorAccountLimited {
    /// The connector, by its id: the run's first.
    pub connector: String,
    /// The account that holds it, limited: the one whose limit resets soonest, when
    /// several do.
    #[schemars(range(min = 1))]
    pub account: u32,
    /// When that account's limit resets, if rlx knows (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, extend("format" = "date-time"))]
    pub resets_at: Option<String>,
}

impl ConnectorAccountLimited {
    /// `connector`, held by `account` alone, limited until `resets_at`.
    pub fn new(connector: impl Into<String>, account: u32, resets_at: Option<String>) -> Self {
        Self {
            connector: connector.into(),
            account,
            resets_at,
        }
    }
}

/// `connector_unavailable`'s detail: no account rlxd may start the run on holds
/// `connector` connected (with the run's connectors before it), or the pinned account
/// doesn't. `accounts` gives its status on each account rlx knows.
///
/// ```json
/// {"connector": "claude.ai Slack",
///  "accounts": [{"account": 1, "status": "needs-auth"}, {"account": 2, "status": "absent"}]}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "ConnectorUnavailable")]
#[non_exhaustive]
pub struct ConnectorUnavailable {
    /// The connector, by its id.
    pub connector: String,
    /// Its status on each account rlx knows, by slot.
    pub accounts: Vec<AccountStatus>,
}

impl ConnectorUnavailable {
    /// `connector`, as `accounts` hold it.
    pub fn new(connector: impl Into<String>, accounts: Vec<AccountStatus>) -> Self {
        Self {
            connector: connector.into(),
            accounts,
        }
    }
}

/// One account's status for a connector, in `connector_unavailable`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct AccountStatus {
    /// The account's rlx slot.
    #[schemars(range(min = 1))]
    pub account: u32,
    /// The connector's status there.
    pub status: RouteStatus,
}

impl AccountStatus {
    /// `status` on `account`.
    pub fn new(account: u32, status: RouteStatus) -> Self {
        Self { account, status }
    }
}

/// A connector's status on one account, as a route reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RouteStatus {
    /// Connected now.
    Connected,
    /// Its sign-in has lapsed: `connectors.login` signs in again.
    NeedsAuth,
    /// Still connecting when it was read.
    Pending,
    /// It could not connect.
    Failed,
    /// The account's connectors were read, and it isn't among them.
    Absent,
    /// rlxd hasn't read the account's connectors (`connectors.list {refresh: true}`
    /// does), or rlxd doesn't know both of its homes, so none would be read.
    Unread,
}

impl From<ConnectorState> for RouteStatus {
    fn from(state: ConnectorState) -> Self {
        match state {
            ConnectorState::Connected => Self::Connected,
            ConnectorState::NeedsAuth => Self::NeedsAuth,
            ConnectorState::Pending => Self::Pending,
            ConnectorState::Failed => Self::Failed,
        }
    }
}

/// `detail` as the error envelope carries it: an object.
fn object(value: &impl Serialize) -> Data {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::Object(fields)) => fields,
        _ => Data::new(),
    }
}

impl From<AmbiguousAccount> for crate::envelope::Detail {
    fn from(detail: AmbiguousAccount) -> Self {
        Self::Object(object(&detail))
    }
}

impl From<ConnectorAccountLimited> for crate::envelope::Detail {
    fn from(detail: ConnectorAccountLimited) -> Self {
        Self::Object(object(&detail))
    }
}

impl From<ConnectorUnavailable> for crate::envelope::Detail {
    fn from(detail: ConnectorUnavailable) -> Self {
        Self::Object(object(&detail))
    }
}
