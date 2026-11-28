//! A run's account, picked by its connectors (#15): `run.start {connectors, account}`,
//! and a job's run template, which is `run.start`'s params.
//!
//! - **Pinned** (`account`): the run goes there, once rlx knows that account and both its
//!   homes, and each of `connectors` is `connected` there now; else `no_such_account`,
//!   `account_unknown` or `connector_unavailable`. A pin is never refused as usage-limited:
//!   the run may end `rate_limited`.
//! - **By connectors** (no `account`): the accounts that hold every one of `connectors`
//!   `connected` now, with both homes known, are the run's to start on. None:
//!   `connector_unavailable`, for the first connector that leaves none, with its status on
//!   each account that held every connector before it (every account, for the first): so
//!   it is never `connected` there. Two or more:
//!   `ambiguous_account`, unless each one's identity on every connector is known and they
//!   are all the same (the STRICT rule, manager #4188: a release build reads no identity,
//!   so a send never picks an account by its headroom alone). Then the brain's usage
//!   ranks them: those not limited, least used first; one with no usage on record last but
//!   still eligible; the lowest slot on a tie. All limited: `connector_account_limited`,
//!   naming the one whose limit resets soonest. Ambiguity is decided before limits, so a
//!   limited account's workspace is never traded for another's.
//! - **Default** (neither): the app's environment, as every run before routing.
//!
//! The route reads the connector map as it is (no read at start: the reply stays fast),
//! and is decided once, at `run.start`: a queued run, every try (5.5) and `run.resume`
//! keep it. [`eligible`] and [`pick`] are pure; [`Router`] gives them the map and the
//! brain's usage. Python owns usage: rlxd only orders what the brain says.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use rlxd_proto::{
    AccountStatus, AmbiguousAccount, Candidate, ConnectorAccountLimited, ConnectorIdentity,
    ConnectorState, ConnectorUnavailable, Data, ErrorBody, RouteBy, RouteStatus, RunAccount,
    RunStartParams, codes,
};
use serde::Deserialize;

use crate::brain::{Brain, CallError};
use crate::connectors::Connectors;
use crate::log::info;

/// How long the brain may take to accept a `usage` ask: a run's start waits on it.
const USAGE_ACCEPT: Duration = Duration::from_secs(1);

/// How long the brain may take to answer it, once accepted.
const USAGE_RUN: Duration = Duration::from_secs(2);

/// One account, as a route sees it: whether a run can start there, and what the
/// connector map last read of it.
#[derive(Clone, Debug, PartialEq)]
pub struct Seen {
    /// Its rlx slot.
    pub slot: u32,
    /// Whether rlxd knows both its homes, as a run there needs
    /// ([`crate::connectors::Account::homes_known`]).
    pub homes_known: bool,
    /// Its connectors, as the map last read them: `None` when it never has.
    pub connectors: Option<Vec<Held>>,
}

/// One connector on one account, as the map last read it.
#[derive(Clone, Debug, PartialEq)]
pub struct Held {
    /// Its id.
    pub id: String,
    /// Its status then.
    pub state: ConnectorState,
    /// Who the account is on it, when rlxd has read that.
    pub identity: Option<ConnectorIdentity>,
}

impl Seen {
    /// `connector`'s row here, when the map lists it.
    fn row(&self, connector: &str) -> Option<&Held> {
        self.connectors.as_ref()?.iter().find(|c| c.id == connector)
    }

    /// Whether a run may start here with `connector`: both homes known, and it is
    /// `connected` now.
    fn holds(&self, connector: &str) -> bool {
        self.homes_known
            && self
                .row(connector)
                .is_some_and(|row| row.state == ConnectorState::Connected)
    }

    /// `connector`'s status here, as `connector_unavailable` lists it.
    fn status(&self, connector: &str) -> RouteStatus {
        if !self.homes_known {
            return RouteStatus::Unread;
        }
        match &self.connectors {
            None => RouteStatus::Unread,
            Some(rows) => rows
                .iter()
                .find(|c| c.id == connector)
                .map_or(RouteStatus::Absent, |row| row.state.into()),
        }
    }

    /// Who the account is on `connector`: only from a row connected now (#4064).
    fn identity(&self, connector: &str) -> Option<&ConnectorIdentity> {
        self.row(connector)
            .filter(|row| row.state == ConnectorState::Connected)
            .and_then(|row| row.identity.as_ref())
    }
}

/// One account's usage, as the brain's `usage` says it (Python's figures and rule).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct Room {
    /// Its slot.
    pub slot: u32,
    /// Its fullest window, percent used, from its last reading on record; `None` with no
    /// reading.
    #[serde(default)]
    pub used_percent: Option<f64>,
    /// Whether it refuses requests now.
    #[serde(default)]
    pub limited: bool,
    /// When its limit resets, when it is limited and rlx knows (RFC 3339).
    #[serde(default)]
    pub resets_at: Option<String>,
}

/// Every account's usage the brain gave, by slot; empty when it gave none.
pub type Usage = BTreeMap<u32, Room>;

/// What [`eligible`] decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Neither `account` nor `connectors`: the app's environment.
    Default,
    /// The pinned account, which holds every connector.
    Pinned(u32),
    /// The accounts the run may start on, by slot, lowest first: [`pick`] takes one.
    Candidates(Vec<u32>),
}

/// Why no account takes the run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The pinned slot is no account rlx knows.
    NoSuchAccount(u32),
    /// rlxd doesn't know both homes of the pinned account.
    AccountUnknown(u32),
    /// No account the run may start on holds `connector` connected; `pinned` when it was
    /// the run's pinned account that lacks it.
    Unavailable {
        detail: ConnectorUnavailable,
        pinned: Option<u32>,
    },
    /// Two or more accounts hold it, and aren't known to be the same one there.
    Ambiguous(AmbiguousAccount),
    /// Every account that holds them is limited.
    Limited(ConnectorAccountLimited),
}

/// Which accounts `connectors` (and the pin `pinned`) allow among `accounts`, the
/// accounts rlx knows (the [module](self)'s rules, in their order).
pub fn eligible(
    pinned: Option<u32>,
    connectors: &[String],
    accounts: &[Seen],
) -> Result<Step, Refused> {
    let statuses = |connector: &str| {
        ConnectorUnavailable::new(
            connector,
            accounts
                .iter()
                .map(|seen| AccountStatus::new(seen.slot, seen.status(connector)))
                .collect(),
        )
    };
    if let Some(slot) = pinned {
        let seen = accounts
            .iter()
            .find(|seen| seen.slot == slot)
            .ok_or(Refused::NoSuchAccount(slot))?;
        if !seen.homes_known {
            return Err(Refused::AccountUnknown(slot));
        }
        if let Some(missing) = connectors.iter().find(|c| !seen.holds(c)) {
            return Err(Refused::Unavailable {
                detail: statuses(missing),
                pinned: Some(slot),
            });
        }
        return Ok(Step::Pinned(slot));
    }
    if connectors.is_empty() {
        return Ok(Step::Default);
    }
    // Narrowed connector by connector. The first that leaves none is named, with its
    // status on each account that held every one before it (every account, for the
    // first): connecting it on any of those is what the run needs, and none holds it
    // connected, so the refusal never says `connected` for it.
    let mut holders: Vec<&Seen> = accounts.iter().collect();
    for connector in connectors {
        let next: Vec<&Seen> = holders
            .iter()
            .copied()
            .filter(|seen| seen.holds(connector))
            .collect();
        if next.is_empty() {
            let detail = ConnectorUnavailable::new(
                connector,
                holders
                    .iter()
                    .map(|seen| AccountStatus::new(seen.slot, seen.status(connector)))
                    .collect(),
            );
            return Err(Refused::Unavailable {
                detail,
                pinned: None,
            });
        }
        holders = next;
    }
    holders.sort_by_key(|seen| seen.slot);
    if holders.len() > 1 {
        for connector in connectors {
            let first = holders[0].identity(connector);
            let same =
                first.is_some() && holders.iter().all(|seen| seen.identity(connector) == first);
            if !same {
                return Err(Refused::Ambiguous(AmbiguousAccount::new(
                    connector.clone(),
                    holders
                        .iter()
                        .map(|seen| Candidate::new(seen.slot, seen.identity(connector).cloned()))
                        .collect(),
                )));
            }
        }
    }
    Ok(Step::Candidates(
        holders.into_iter().map(|seen| seen.slot).collect(),
    ))
}

/// The account `candidates` (from [`eligible`]) start the run on, by `usage`: the one not
/// limited with the most room (least used; one with no reading after every one with
/// some; the lowest slot on a tie); or, when every one is limited,
/// `connector_account_limited` for `connector`, naming the one whose limit resets
/// soonest (one with no reset time after every one with some; the lowest slot on a tie).
pub fn pick(connector: &str, candidates: &[u32], usage: &Usage) -> Result<u32, Refused> {
    let room = |slot: &u32| usage.get(slot);
    let limited = |slot: &u32| room(slot).is_some_and(|room| room.limited);
    let free = candidates.iter().filter(|slot| !limited(slot));
    let used = |slot: &u32| {
        room(slot)
            .and_then(|room| room.used_percent)
            .filter(|percent| !percent.is_nan())
    };
    let best = free.min_by(|a, b| match (used(a), used(b)) {
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal).then(a.cmp(b)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.cmp(b),
    });
    if let Some(slot) = best {
        return Ok(*slot);
    }
    let resets = |slot: &u32| {
        room(slot)
            .and_then(|room| room.resets_at.as_deref())
            .and_then(|at| at.parse::<jiff::Timestamp>().ok())
    };
    let soonest = candidates
        .iter()
        .min_by(|a, b| match (resets(a), resets(b)) {
            (Some(x), Some(y)) => x.cmp(&y).then(a.cmp(b)),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => a.cmp(b),
        });
    let Some(slot) = soonest else {
        // `eligible` never gives no candidates; were it to, nothing holds the connector.
        return Err(Refused::Unavailable {
            detail: ConnectorUnavailable::new(connector, Vec::new()),
            pinned: None,
        });
    };
    let resets_at = resets(slot).map(|at| at.to_string());
    Err(Refused::Limited(ConnectorAccountLimited::new(
        connector, *slot, resets_at,
    )))
}

impl Refused {
    /// The refusal as `run.start` gives it: its code, a message for a person, and its
    /// detail. No message names a home or an identity: they are on the wire alone, in
    /// `ambiguous_account`'s detail.
    pub fn error(&self) -> ErrorBody {
        let body = |code: &str, message: String, hint: &str| {
            ErrorBody::new(code, message)
                .expect("a route's refusal has a code and a message")
                .with_hint(hint)
        };
        let with = |error: ErrorBody, detail: rlxd_proto::Detail| {
            error.clone().with_detail(detail).unwrap_or(error)
        };
        match self {
            Self::NoSuchAccount(slot) => body(
                codes::NO_SUCH_ACCOUNT,
                format!("rlx knows no Claude Code account {slot}"),
                "connectors.list names the accounts rlx knows",
            ),
            Self::AccountUnknown(slot) => body(
                codes::ACCOUNT_UNKNOWN,
                format!(
                    "rlxd doesn't know both homes of account {slot}, so it starts no run \
                     there rather than one on another account's"
                ),
                "rlxd learns them from rlx: connectors.list {\"refresh\": true} asks again",
            ),
            Self::Unavailable { detail, pinned } => {
                let message = match pinned {
                    Some(slot) => format!(
                        "{} is not connected on account {slot}, which the run pins",
                        detail.connector
                    ),
                    None => format!(
                        "{} is connected on no account rlxd can start the run on",
                        detail.connector
                    ),
                };
                with(
                    body(
                        codes::CONNECTOR_UNAVAILABLE,
                        message,
                        "connectors.login signs a connector in again, and connectors.list \
                         {\"refresh\": true} reads each account's again",
                    ),
                    detail.clone().into(),
                )
            }
            Self::Ambiguous(detail) => {
                let slots: Vec<String> = detail
                    .candidates
                    .iter()
                    .map(|c| c.account.to_string())
                    .collect();
                let error = body(
                    codes::AMBIGUOUS_ACCOUNT,
                    format!(
                        "{} is connected on accounts {}, which rlxd can't tell are the same \
                         one there",
                        detail.connector,
                        slots.join(", ")
                    ),
                    "pin the run's account",
                );
                with(error, detail.clone().into())
            }
            Self::Limited(detail) => {
                let until = detail
                    .resets_at
                    .as_deref()
                    .map_or_else(String::new, |at| format!(" until {at}"));
                let error = body(
                    codes::CONNECTOR_ACCOUNT_LIMITED,
                    format!(
                        "{} is connected only on account {}, which is usage-limited{until}",
                        detail.connector, detail.account
                    ),
                    "wait for the limit to reset, or connect it on another account",
                );
                with(error, detail.clone().into())
            }
        }
    }
}

/// Gives [`eligible`] and [`pick`] the connector map and the brain's usage.
pub struct Router {
    connectors: Arc<Connectors>,
    brain: Arc<Brain>,
}

impl Router {
    /// Routes by `connectors`' map, with `brain`'s usage.
    pub fn new(connectors: Arc<Connectors>, brain: Arc<Brain>) -> Arc<Self> {
        Arc::new(Self { connectors, brain })
    }

    /// Whether routing `params` asks the brain for usage: a route by connectors, with no
    /// pin.
    pub fn wants_usage(params: &RunStartParams) -> bool {
        params.account.is_none() && !params.connectors.is_empty()
    }

    /// Every account's usage, as the brain says: empty when it can't say (no brain, or
    /// it didn't answer in time). Then every account ranks as one with no reading, and
    /// none is refused as limited.
    pub async fn usage(&self) -> Usage {
        if let Some(usage) = test_usage() {
            return usage;
        }
        let data = match self
            .brain
            .call("usage", Data::new(), USAGE_ACCEPT, USAGE_RUN)
            .await
        {
            Ok(data) => data,
            Err(CallError::NotDelivered(error) | CallError::Failed(error)) => {
                info!("route: no usage: the brain said {}", error.code);
                return Usage::new();
            }
            Err(CallError::TimedOut) => {
                info!("route: no usage: the brain did not answer in time");
                return Usage::new();
            }
        };
        match usage_of(serde_json::Value::Object(data)) {
            Ok(usage) => usage,
            Err(why) => {
                info!("route: no usage: {why}");
                Usage::new()
            }
        }
    }

    /// The account `params` start on, as [`RunAccount`]: its slot for a pinned or routed
    /// run, none (worked out at each attempt) for a default one; or the refusal. `usage`
    /// is what [`Router::usage`] gave, when [`Router::wants_usage`] said to ask: without
    /// it, a route by connectors ranks every account as one with no reading.
    pub fn route(
        &self,
        params: &RunStartParams,
        usage: Option<&Usage>,
    ) -> Result<RunAccount, ErrorBody> {
        let accounts = self.connectors.route_view();
        let step = eligible(params.account, &params.connectors, &accounts)
            .map_err(|refused| refused.error())?;
        match step {
            Step::Default => Ok(RunAccount::new(None, RouteBy::Default)),
            Step::Pinned(slot) => Ok(RunAccount::new(Some(slot), RouteBy::Pinned)),
            Step::Candidates(candidates) => {
                let none = Usage::new();
                let first = params.connectors.first().map_or("", String::as_str);
                let slot = pick(first, &candidates, usage.unwrap_or(&none))
                    .map_err(|refused| refused.error())?;
                Ok(RunAccount::new(Some(slot), RouteBy::Connectors))
            }
        }
    }
}

/// The brain's `usage` reply: `{accounts: [{slot, used_percent?, limited, resets_at?}]}`.
fn usage_of(reply: serde_json::Value) -> Result<Usage, String> {
    #[derive(Deserialize)]
    struct Reply {
        accounts: Vec<Room>,
    }
    let reply: Reply = serde_json::from_value(reply)
        .map_err(|e| format!("the brain's usage was not read: {e}"))?;
    Ok(reply
        .accounts
        .into_iter()
        .map(|room| (room.slot, room))
        .collect())
}

/// Test builds only: `RLXD_TEST_USAGE` names a file holding the usage, as the brain's
/// `usage` would answer it, read each time it is asked for.
#[cfg(feature = "debug-methods")]
fn test_usage() -> Option<Usage> {
    let path = std::env::var_os("RLXD_TEST_USAGE")?;
    let read = std::fs::read_to_string(&path)
        .map_err(|e| format!("RLXD_TEST_USAGE could not be read: {e}"))
        .and_then(|text| serde_json::from_str(&text).map_err(|e| format!("RLXD_TEST_USAGE: {e}")))
        .and_then(usage_of);
    Some(read.unwrap_or_else(|why| {
        info!("route: no usage: {why}");
        Usage::new()
    }))
}

#[cfg(not(feature = "debug-methods"))]
fn test_usage() -> Option<Usage> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLACK: &str = "claude.ai Slack";
    const GMAIL: &str = "claude.ai Gmail";

    fn who(workspace: &str) -> ConnectorIdentity {
        let mut identity = ConnectorIdentity::default();
        identity.workspace = Some(workspace.to_owned());
        identity
    }

    fn row(id: &str, state: ConnectorState, identity: Option<ConnectorIdentity>) -> Held {
        Held {
            id: id.to_owned(),
            state,
            identity,
        }
    }

    fn account(slot: u32, rows: Vec<Held>) -> Seen {
        Seen {
            slot,
            homes_known: true,
            connectors: Some(rows),
        }
    }

    fn connected(slot: u32, id: &str, identity: Option<&str>) -> Seen {
        account(
            slot,
            vec![row(id, ConnectorState::Connected, identity.map(who))],
        )
    }

    fn names(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    fn room(slot: u32, used: Option<f64>, limited: bool, resets_at: Option<&str>) -> Room {
        Room {
            slot,
            used_percent: used,
            limited,
            resets_at: resets_at.map(str::to_owned),
        }
    }

    fn usage(rooms: Vec<Room>) -> Usage {
        rooms.into_iter().map(|r| (r.slot, r)).collect()
    }

    fn unavailable(refused: Result<Step, Refused>) -> (ConnectorUnavailable, Option<u32>) {
        match refused {
            Err(Refused::Unavailable { detail, pinned }) => (detail, pinned),
            other => panic!("not connector_unavailable: {other:?}"),
        }
    }

    // Default.

    #[test]
    fn neither_a_pin_nor_connectors_is_the_default_route() {
        let accounts = vec![connected(1, SLACK, None)];
        assert_eq!(eligible(None, &[], &accounts), Ok(Step::Default));
        assert_eq!(eligible(None, &[], &[]), Ok(Step::Default));
    }

    // Pinned.

    #[test]
    fn a_pin_with_no_connectors_is_taken_where_rlx_knows_the_account() {
        let accounts = vec![connected(1, SLACK, None), account(2, Vec::new())];
        assert_eq!(eligible(Some(2), &[], &accounts), Ok(Step::Pinned(2)));
    }

    #[test]
    fn a_pin_to_an_account_rlx_does_not_know_is_no_such_account() {
        let accounts = vec![connected(1, SLACK, None)];
        assert_eq!(
            eligible(Some(4), &names(&[SLACK]), &accounts),
            Err(Refused::NoSuchAccount(4))
        );
        assert_eq!(
            Refused::NoSuchAccount(4).error().code,
            codes::NO_SUCH_ACCOUNT
        );
    }

    #[test]
    fn a_pin_to_an_account_without_both_homes_is_account_unknown() {
        let mut two = connected(2, SLACK, None);
        two.homes_known = false;
        let accounts = vec![connected(1, SLACK, None), two];
        assert_eq!(
            eligible(Some(2), &names(&[SLACK]), &accounts),
            Err(Refused::AccountUnknown(2))
        );
        assert_eq!(
            Refused::AccountUnknown(2).error().code,
            codes::ACCOUNT_UNKNOWN
        );
    }

    #[test]
    fn a_pin_lacking_a_connector_is_connector_unavailable_with_every_accounts_status() {
        let accounts = vec![
            connected(1, SLACK, Some("Acme")),
            account(2, vec![row(SLACK, ConnectorState::NeedsAuth, None)]),
            account(3, Vec::new()),
        ];
        let (detail, pinned) = unavailable(eligible(Some(2), &names(&[SLACK]), &accounts));
        assert_eq!(pinned, Some(2));
        assert_eq!(detail.connector, SLACK);
        assert_eq!(
            detail.accounts,
            vec![
                AccountStatus::new(1, RouteStatus::Connected),
                AccountStatus::new(2, RouteStatus::NeedsAuth),
                AccountStatus::new(3, RouteStatus::Absent),
            ]
        );
        let error = Refused::Unavailable {
            detail,
            pinned: Some(2),
        }
        .error();
        assert_eq!(error.code, codes::CONNECTOR_UNAVAILABLE);
        assert!(error.message.contains("account 2"), "{}", error.message);
        assert!(error.detail.is_some());
    }

    #[test]
    fn a_pin_needs_every_connector_and_names_the_first_it_lacks() {
        let accounts = vec![account(
            2,
            vec![
                row(SLACK, ConnectorState::Connected, None),
                row(GMAIL, ConnectorState::Pending, None),
            ],
        )];
        let (detail, _) = unavailable(eligible(Some(2), &names(&[SLACK, GMAIL]), &accounts));
        assert_eq!(detail.connector, GMAIL);
        assert_eq!(
            detail.accounts,
            vec![AccountStatus::new(2, RouteStatus::Pending)]
        );
        assert_eq!(
            eligible(Some(2), &names(&[SLACK]), &accounts),
            Ok(Step::Pinned(2))
        );
    }

    #[test]
    fn a_pin_is_taken_whatever_the_other_accounts_identities_are() {
        // D2's ambiguity is for an unpinned run alone: the pin is the client's answer.
        let accounts = vec![connected(1, SLACK, Some("Acme")), connected(2, SLACK, None)];
        assert_eq!(
            eligible(Some(2), &names(&[SLACK]), &accounts),
            Ok(Step::Pinned(2))
        );
    }

    // By connectors: who holds them.

    #[test]
    fn the_sole_account_holding_the_connector_connected_is_the_only_candidate() {
        let accounts = vec![
            connected(1, SLACK, None),
            account(2, vec![row(SLACK, ConnectorState::NeedsAuth, None)]),
        ];
        assert_eq!(
            eligible(None, &names(&[SLACK]), &accounts),
            Ok(Step::Candidates(vec![1]))
        );
    }

    #[test]
    fn no_account_holding_it_is_connector_unavailable_with_each_status() {
        let mut unknown = connected(4, SLACK, None);
        unknown.homes_known = false;
        let accounts = vec![
            account(1, vec![row(SLACK, ConnectorState::Failed, None)]),
            Seen {
                slot: 2,
                homes_known: true,
                connectors: None,
            },
            account(3, vec![row(GMAIL, ConnectorState::Connected, None)]),
            unknown,
        ];
        let (detail, pinned) = unavailable(eligible(None, &names(&[SLACK]), &accounts));
        assert_eq!(pinned, None);
        assert_eq!(
            detail.accounts,
            vec![
                AccountStatus::new(1, RouteStatus::Failed),
                AccountStatus::new(2, RouteStatus::Unread),
                AccountStatus::new(3, RouteStatus::Absent),
                // Connected there, but no run starts on an account without both homes.
                AccountStatus::new(4, RouteStatus::Unread),
            ]
        );
    }

    #[test]
    fn with_no_account_known_every_connector_is_unavailable() {
        let (detail, _) = unavailable(eligible(None, &names(&[SLACK]), &[]));
        assert_eq!(detail.connector, SLACK);
        assert!(detail.accounts.is_empty());
    }

    #[test]
    fn an_account_needs_every_connector_and_the_first_that_leaves_none_is_named() {
        // Slack on 1 and 2, Gmail on 2 and 3: account 2 alone holds both.
        let accounts = vec![
            connected(1, SLACK, None),
            account(
                2,
                vec![
                    row(SLACK, ConnectorState::Connected, None),
                    row(GMAIL, ConnectorState::Connected, None),
                ],
            ),
            connected(3, GMAIL, None),
        ];
        assert_eq!(
            eligible(None, &names(&[SLACK, GMAIL]), &accounts),
            Ok(Step::Candidates(vec![2]))
        );
        // Slack on 1 alone, Gmail on 3 alone: none holds both, and Gmail is the one that
        // leaves none after Slack. It is named with its status on account 1 alone, the
        // one that holds Slack: connecting Gmail there is what the run needs. Account 3,
        // which lacks Slack, is not listed: its Gmail is connected, which would mislead.
        let apart = vec![connected(1, SLACK, None), connected(3, GMAIL, None)];
        let (detail, _) = unavailable(eligible(None, &names(&[SLACK, GMAIL]), &apart));
        assert_eq!(detail.connector, GMAIL);
        assert_eq!(
            detail.accounts,
            vec![AccountStatus::new(1, RouteStatus::Absent)]
        );
    }

    #[test]
    fn an_unavailable_connector_is_never_said_connected_where_it_is_listed() {
        let both = |slot, slack: ConnectorState, gmail: ConnectorState| {
            account(slot, vec![row(SLACK, slack, None), row(GMAIL, gmail, None)])
        };
        let mut unknown = both(4, ConnectorState::Connected, ConnectorState::Connected);
        unknown.homes_known = false;
        let cases = vec![
            vec![
                both(1, ConnectorState::Connected, ConnectorState::NeedsAuth),
                both(2, ConnectorState::NeedsAuth, ConnectorState::Connected),
                unknown.clone(),
            ],
            vec![
                both(1, ConnectorState::Failed, ConnectorState::Connected),
                both(2, ConnectorState::Pending, ConnectorState::Connected),
            ],
            vec![unknown],
        ];
        for accounts in cases {
            let (detail, _) = unavailable(eligible(None, &names(&[SLACK, GMAIL]), &accounts));
            assert!(
                detail
                    .accounts
                    .iter()
                    .all(|a| a.status != RouteStatus::Connected),
                "{detail:?}"
            );
        }
        // Slack on 1 (Gmail needs-auth there) and on 2 (Gmail connected there, Slack not):
        // Gmail is named, with account 1's status alone.
        let accounts = vec![
            both(1, ConnectorState::Connected, ConnectorState::NeedsAuth),
            both(2, ConnectorState::NeedsAuth, ConnectorState::Connected),
        ];
        let (detail, _) = unavailable(eligible(None, &names(&[SLACK, GMAIL]), &accounts));
        assert_eq!(detail.connector, GMAIL);
        assert_eq!(
            detail.accounts,
            vec![AccountStatus::new(1, RouteStatus::NeedsAuth)]
        );
    }

    #[test]
    fn a_connector_still_pending_or_needing_auth_is_not_connected_now() {
        for state in [
            ConnectorState::Pending,
            ConnectorState::NeedsAuth,
            ConnectorState::Failed,
        ] {
            let accounts = vec![account(1, vec![row(SLACK, state, Some(who("Acme")))])];
            assert!(
                matches!(
                    eligible(None, &names(&[SLACK]), &accounts),
                    Err(Refused::Unavailable { .. })
                ),
                "{state:?}"
            );
        }
    }

    // By connectors: D2, the strict rule.

    #[test]
    fn two_holders_with_different_identities_are_ambiguous() {
        let accounts = vec![
            connected(1, SLACK, Some("Acme")),
            connected(3, SLACK, Some("Other")),
        ];
        let Err(Refused::Ambiguous(detail)) = eligible(None, &names(&[SLACK]), &accounts) else {
            panic!("not ambiguous");
        };
        assert_eq!(detail.connector, SLACK);
        assert_eq!(
            detail.candidates,
            vec![
                Candidate::new(1, Some(who("Acme"))),
                Candidate::new(3, Some(who("Other"))),
            ]
        );
        let error = Refused::Ambiguous(detail).error();
        assert_eq!(error.code, codes::AMBIGUOUS_ACCOUNT);
        // The identities are in the detail alone, never in the message.
        assert!(!error.message.contains("Acme"), "{}", error.message);
        assert!(error.message.contains("1, 3"), "{}", error.message);
    }

    #[test]
    fn two_holders_with_unknown_identities_are_ambiguous_with_nulls() {
        let accounts = vec![connected(1, SLACK, None), connected(2, SLACK, None)];
        let Err(Refused::Ambiguous(detail)) = eligible(None, &names(&[SLACK]), &accounts) else {
            panic!("not ambiguous");
        };
        assert_eq!(
            detail.candidates,
            vec![Candidate::new(1, None), Candidate::new(2, None)]
        );
        let printed = serde_json::to_value(&detail).unwrap();
        assert!(printed["candidates"][0]["identity"].is_null(), "{printed}");
    }

    #[test]
    fn one_known_and_one_unknown_identity_are_ambiguous() {
        let accounts = vec![connected(1, SLACK, Some("Acme")), connected(2, SLACK, None)];
        let Err(Refused::Ambiguous(detail)) = eligible(None, &names(&[SLACK]), &accounts) else {
            panic!("not ambiguous");
        };
        assert_eq!(
            detail.candidates,
            vec![
                Candidate::new(1, Some(who("Acme"))),
                Candidate::new(2, None)
            ]
        );
        // Whichever is first.
        let flipped = vec![connected(1, SLACK, None), connected(2, SLACK, Some("Acme"))];
        assert!(matches!(
            eligible(None, &names(&[SLACK]), &flipped),
            Err(Refused::Ambiguous(_))
        ));
    }

    #[test]
    fn two_holders_known_to_be_the_same_identity_go_to_the_rank() {
        let accounts = vec![
            connected(1, SLACK, Some("Acme")),
            connected(3, SLACK, Some("Acme")),
        ];
        assert_eq!(
            eligible(None, &names(&[SLACK]), &accounts),
            Ok(Step::Candidates(vec![1, 3]))
        );
    }

    #[test]
    fn a_sole_holder_needs_no_identity() {
        let accounts = vec![connected(2, SLACK, None), account(1, Vec::new())];
        assert_eq!(
            eligible(None, &names(&[SLACK]), &accounts),
            Ok(Step::Candidates(vec![2]))
        );
    }

    #[test]
    fn every_connector_must_be_known_the_same_and_the_first_that_isnt_is_named() {
        let both = |slot, gmail: Option<&str>| {
            account(
                slot,
                vec![
                    row(SLACK, ConnectorState::Connected, Some(who("Acme"))),
                    row(GMAIL, ConnectorState::Connected, gmail.map(who)),
                ],
            )
        };
        let accounts = vec![both(1, Some("one")), both(2, Some("two"))];
        let Err(Refused::Ambiguous(detail)) = eligible(None, &names(&[SLACK, GMAIL]), &accounts)
        else {
            panic!("not ambiguous");
        };
        assert_eq!(detail.connector, GMAIL);
        let unknown = vec![both(1, Some("one")), both(2, None)];
        let Err(Refused::Ambiguous(detail)) = eligible(None, &names(&[SLACK, GMAIL]), &unknown)
        else {
            panic!("not ambiguous");
        };
        assert_eq!(detail.connector, GMAIL);
        let same = vec![both(1, Some("one")), both(2, Some("one"))];
        assert_eq!(
            eligible(None, &names(&[SLACK, GMAIL]), &same),
            Ok(Step::Candidates(vec![1, 2]))
        );
    }

    #[test]
    fn ambiguity_counts_only_accounts_that_hold_every_connector() {
        // Account 3 holds Slack under another workspace but not Gmail: it is no
        // candidate, so it makes nothing ambiguous.
        let accounts = vec![
            account(
                1,
                vec![
                    row(SLACK, ConnectorState::Connected, Some(who("Acme"))),
                    row(GMAIL, ConnectorState::Connected, None),
                ],
            ),
            connected(3, SLACK, Some("Other")),
        ];
        assert_eq!(
            eligible(None, &names(&[SLACK, GMAIL]), &accounts),
            Ok(Step::Candidates(vec![1]))
        );
    }

    #[test]
    fn an_identity_is_read_only_from_a_row_connected_now() {
        // Account 2's Slack row is connected, so it is a holder; its identity is known.
        // Account 3's is pending: not a holder, and its identity counts for nothing.
        let accounts = vec![
            connected(1, SLACK, Some("Acme")),
            connected(2, SLACK, Some("Acme")),
            account(
                3,
                vec![row(SLACK, ConnectorState::Pending, Some(who("Other")))],
            ),
        ];
        assert_eq!(
            eligible(None, &names(&[SLACK]), &accounts),
            Ok(Step::Candidates(vec![1, 2]))
        );
        assert_eq!(
            account(3, vec![row(SLACK, ConnectorState::Pending, Some(who("x")))]).identity(SLACK),
            None
        );
    }

    #[test]
    fn candidates_and_their_ambiguity_are_listed_lowest_slot_first() {
        let accounts = vec![
            connected(3, SLACK, Some("a")),
            connected(1, SLACK, Some("b")),
        ];
        let Err(Refused::Ambiguous(detail)) = eligible(None, &names(&[SLACK]), &accounts) else {
            panic!("not ambiguous");
        };
        let slots: Vec<u32> = detail.candidates.iter().map(|c| c.account).collect();
        assert_eq!(slots, vec![1, 3]);
    }

    // By connectors: the rank.

    #[test]
    fn the_least_used_account_not_limited_is_picked() {
        let usage = usage(vec![
            room(1, Some(70.0), false, None),
            room(2, Some(10.0), false, None),
            room(3, Some(40.0), false, None),
        ]);
        assert_eq!(pick(SLACK, &[1, 2, 3], &usage), Ok(2));
    }

    #[test]
    fn an_account_with_no_reading_ranks_last_but_stays_eligible() {
        let usage = usage(vec![
            room(1, None, false, None),
            room(2, Some(95.0), false, None),
        ]);
        assert_eq!(pick(SLACK, &[1, 2], &usage), Ok(2));
        // An account the brain says nothing of is one with no reading.
        assert_eq!(pick(SLACK, &[1, 3], &usage), Ok(1));
        assert_eq!(
            pick(SLACK, &[3], &usage),
            Ok(3),
            "alone, it is still picked"
        );
    }

    #[test]
    fn with_no_usage_at_all_the_lowest_slot_is_picked() {
        assert_eq!(pick(SLACK, &[2, 3], &Usage::new()), Ok(2));
    }

    #[test]
    fn a_tie_goes_to_the_lowest_slot() {
        let usage = usage(vec![
            room(3, Some(20.0), false, None),
            room(2, Some(20.0), false, None),
        ]);
        assert_eq!(pick(SLACK, &[3, 2], &usage), Ok(2));
    }

    #[test]
    fn a_reading_that_is_no_number_is_no_reading() {
        let usage = usage(vec![
            room(1, Some(f64::NAN), false, None),
            room(2, Some(99.0), false, None),
        ]);
        assert_eq!(pick(SLACK, &[1, 2], &usage), Ok(2));
    }

    #[test]
    fn a_limited_account_is_skipped_for_one_that_isnt() {
        let usage = usage(vec![
            room(1, Some(0.0), true, Some("2026-11-28T17:00:00Z")),
            room(2, Some(90.0), false, None),
        ]);
        assert_eq!(pick(SLACK, &[1, 2], &usage), Ok(2));
    }

    #[test]
    fn a_limited_sole_holder_is_connector_account_limited_with_its_reset() {
        let usage = usage(vec![room(
            1,
            Some(100.0),
            true,
            Some("2026-11-28T17:00:00Z"),
        )]);
        let Err(Refused::Limited(detail)) = pick(SLACK, &[1], &usage) else {
            panic!("not limited");
        };
        assert_eq!(
            detail,
            ConnectorAccountLimited::new(SLACK, 1, Some("2026-11-28T17:00:00Z".to_owned()))
        );
        let error = Refused::Limited(detail).error();
        assert_eq!(error.code, codes::CONNECTOR_ACCOUNT_LIMITED);
        assert!(
            error.message.contains("until 2026-11-28T17:00:00Z"),
            "{}",
            error.message
        );
    }

    #[test]
    fn every_holder_limited_names_the_soonest_reset() {
        let usage = usage(vec![
            room(1, None, true, Some("2026-11-29T09:00:00Z")),
            room(2, None, true, None),
            room(3, None, true, Some("2026-11-28T21:00:00+01:00")),
        ]);
        let Err(Refused::Limited(detail)) = pick(SLACK, &[1, 2, 3], &usage) else {
            panic!("not limited");
        };
        assert_eq!(detail.account, 3);
        // In UTC, as rlxd writes times.
        assert_eq!(detail.resets_at.as_deref(), Some("2026-11-28T20:00:00Z"));
        // None with a reset time: the lowest slot, and no time.
        let unknown = usage_of_slots(&[1, 2]);
        let Err(Refused::Limited(detail)) = pick(SLACK, &[2, 1], &unknown) else {
            panic!("not limited");
        };
        assert_eq!((detail.account, detail.resets_at), (1, None));
    }

    fn usage_of_slots(slots: &[u32]) -> Usage {
        usage(slots.iter().map(|s| room(*s, None, true, None)).collect())
    }

    #[test]
    fn ambiguity_is_decided_before_limits() {
        // Slack on 1 (Acme, limited) and 2 (Other, free): picking 2 would trade Acme's
        // workspace for Other's, so it is ambiguous before any usage is read.
        let accounts = vec![
            connected(1, SLACK, Some("Acme")),
            connected(2, SLACK, Some("Other")),
        ];
        assert!(matches!(
            eligible(None, &names(&[SLACK]), &accounts),
            Err(Refused::Ambiguous(_))
        ));
    }

    #[test]
    fn the_brains_reply_is_read_by_slot() {
        let reply = serde_json::json!({"accounts": [
            {"slot": 1, "used_percent": 40.5, "limited": false, "read_at": "2026-11-28T10:00:00Z"},
            {"slot": 2, "limited": true, "resets_at": "2026-11-28T17:00:00Z"},
            {"slot": 3}
        ]});
        let usage = usage_of(reply).unwrap();
        assert_eq!(usage[&1].used_percent, Some(40.5));
        assert!(usage[&2].limited);
        assert_eq!(usage[&2].resets_at.as_deref(), Some("2026-11-28T17:00:00Z"));
        assert_eq!(usage[&3], room(3, None, false, None));
        assert!(usage_of(serde_json::json!({"rows": []})).is_err());
    }

    #[test]
    fn only_a_route_by_connectors_with_no_pin_asks_for_usage() {
        let id = rlxd_proto::RunId::new("0f8e2d6c-3b1a-4c5d-9e7f-1a2b3c4d5e6f").unwrap();
        let mut params = RunStartParams::new(id, rlxd_proto::RunKind::Task, "hi");
        assert!(!Router::wants_usage(&params));
        params.connectors = names(&[SLACK]);
        assert!(Router::wants_usage(&params));
        params.account = Some(1);
        assert!(!Router::wants_usage(&params));
    }
}
