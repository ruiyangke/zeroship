//! Control-owned app and plan facts, and the watermark that orders them.
//!
//! The workflow service reads these to decide an app's policy and to notice a
//! terminal deletion. It holds no Control database binding for either: the
//! facts arrive over one authenticated endpoint, and this module is the
//! contract both ends compile against.
//!
//! # The watermark is what makes an answer orderable
//!
//! Control reads the facts and [`SourceWatermark`] in ONE statement, so the
//! watermark is at least the write position of every change visible in that
//! statement's snapshot. Two answers are therefore comparable: if the second
//! carries a watermark at or above the first, everything the first saw the
//! second saw too. That is the whole of the guarantee, and it is what the
//! workflow policy ledger's publication bracket spends it on - see the note on
//! `ControlPolicyStore` in `zeroship-workflow-manager`.
//!
//! It is NOT a lease, an expiry or a revision. The finite validity of an
//! observation stays with the workflow service, which owns the rollout config
//! that bounds it; nothing here expires.

use crate::app_id::AppId;
use serde::{Deserialize, Serialize};

/// How many apps one request may name.
///
/// The closing lane asks about a page of candidates at once, so the bound is
/// the page rather than one. A caller with more apps than this sends more
/// requests.
pub const MAX_APPS_PER_REQUEST: usize = 256;

/// Where Control's source stood when it read the facts beside it.
///
/// Comparable only against another watermark from the same source. A larger
/// value saw at least as much; equal values saw the same or more. The value is
/// opaque: nothing outside Control's handler may construct one from anything
/// but a response, and nothing may read meaning into its magnitude beyond the
/// ordering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceWatermark(i64);

impl SourceWatermark {
    /// A watermark from a source position.
    ///
    /// Refuses a negative position: the source counts forward from zero, and a
    /// negative value would compare below every real one, which is the exact
    /// direction that turns the fence into a pass.
    #[must_use]
    pub const fn new(position: i64) -> Option<Self> {
        if position < 0 {
            return None;
        }
        Some(Self(position))
    }

    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

/// The apps whose facts a caller wants, in one exchange.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppFactsRequest {
    pub app_ids: Vec<AppId>,
}

/// One consistent read of every requested app Control has a row for.
///
/// Apps Control has no row for are ABSENT rather than reported. That keeps
/// both existing behaviours: a policy observation refuses an app with no row,
/// and a deletion sweep reports only a recorded deletion, never an absence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppFactsResponse {
    pub watermark: SourceWatermark,
    pub apps: Vec<AppSourceFacts>,
}

/// One app's Control-owned facts, and its plan's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppSourceFacts {
    pub app_id: AppId,
    pub plan_id: String,
    pub workflows_enabled: bool,
    /// `archived_at` is set. Archived apps stay placeable for maintenance
    /// jobs; they do not admit new work.
    pub archived: bool,
    /// `deleted_at` is set. Terminal: an app never returns from it.
    pub deleted: bool,
    pub plan: PlanSourceFacts,
}

/// The plan half of an app's policy inputs.
///
/// `workflow_policy` is carried as raw JSON rather than a decoded policy on
/// purpose: an operator provisions it, it can be absent or malformed, and the
/// consumer already refuses both. Decoding it here would move that refusal
/// into Control, which does not own the policy contract.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlanSourceFacts {
    pub workflows_allowed: bool,
    pub archived: bool,
    pub workflow_policy: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::{AppFactsRequest, SourceWatermark};

    /// A negative position is refused rather than clamped. Admitting one would
    /// put a watermark below every genuine value, so a ledger holding it would
    /// accept every later observation - the fence inverted into a pass.
    #[test]
    fn a_watermark_refuses_a_negative_position() {
        assert_eq!(SourceWatermark::new(0).map(SourceWatermark::get), Some(0));
        assert_eq!(SourceWatermark::new(1).map(SourceWatermark::get), Some(1));
        assert_eq!(
            SourceWatermark::new(i64::MAX).map(SourceWatermark::get),
            Some(i64::MAX)
        );
        for refused in [-1, -2, i64::MIN] {
            assert!(
                SourceWatermark::new(refused).is_none(),
                "{refused} is not a source position"
            );
        }
    }

    /// Watermarks order by position, which is what the ledger comparison
    /// relies on. Without `Ord` agreeing with the position, a regression would
    /// not be detectable by comparison at all.
    #[test]
    fn watermarks_order_by_position() {
        let low = SourceWatermark::new(7).unwrap();
        let high = SourceWatermark::new(8).unwrap();
        assert!(low < high);
        assert!(high > low);
        assert_eq!(low, SourceWatermark::new(7).unwrap());
    }

    /// The request rejects unknown fields, so a caller cannot smuggle a
    /// selector past a handler that only reads the ones it knows.
    #[test]
    fn the_request_refuses_unknown_fields() {
        let accepted: AppFactsRequest =
            serde_json::from_str(r#"{"appIds":["app_0000000002e4nenowz3qmamtd"]}"#).unwrap();
        assert_eq!(accepted.app_ids.len(), 1);
        assert!(
            serde_json::from_str::<AppFactsRequest>(
                r#"{"appIds":["app_0000000002e4nenowz3qmamtd"],"watermark":0}"#
            )
            .is_err(),
            "an unknown field is refused"
        );
        // Rejection control: the same shape in snake_case is also refused, so
        // the camelCase rename is what the wire actually carries.
        assert!(
            serde_json::from_str::<AppFactsRequest>(
                r#"{"app_ids":["app_0000000002e4nenowz3qmamtd"]}"#
            )
            .is_err(),
            "the wire name is camelCase"
        );
    }
}
