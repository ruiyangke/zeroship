use super::AppPolicy;
use crate::{
    app_id::AppId,
    workflow_coordination::{AssignedScope, Revision, WorkerId},
};
use serde::{Deserialize, Serialize};
use std::num::NonZeroU64;

/// A worker's policy request for its current placement.
///
/// `establish_after` asks the manager for an open ingress epoch greater than the
/// named one, re-establishing recovery responsibility when needed. A request
/// without it is a plain refresh and never reopens responsibility.
/// `ingress_used` reports that the host accepted ingress since its previous
/// refresh; it only feeds the manager's idle closure trigger.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PolicyLeaseRequest {
    pub scope: AssignedScope,
    pub establish_after: Option<Revision>,
    pub ingress_used: bool,
}

/// Complete policy bound to the enrolled signer and its current placement.
///
/// Receivers validate the entire tuple and raw policy, and anchor the remaining
/// duration before sending the request. This envelope carries no wall-clock
/// timestamp and does not grant a caller-selected app capability.
/// `ingress_epoch` is present while the manager holds open or closing recovery
/// responsibility for the app; creator acceptance requires it to exceed the
/// journal's closed epoch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PolicyLease {
    pub app_id: AppId,
    pub worker_id: WorkerId,
    pub signing_key_id: String,
    pub assignment_revision: Revision,
    pub policy_revision: Revision,
    pub policy: AppPolicy,
    pub ingress_epoch: Option<Revision>,
    pub remaining_ms: NonZeroU64,
}
