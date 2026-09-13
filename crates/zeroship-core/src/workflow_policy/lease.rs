use super::AppPolicy;
use crate::{
    app_id::AppId,
    workflow_coordination::{Revision, WorkerId},
};
use serde::{Deserialize, Serialize};
use std::num::NonZeroU64;

/// Complete policy bound to the enrolled signer and its current placement.
///
/// Receivers validate the entire tuple and raw policy, and anchor the remaining
/// duration before sending the request. This envelope carries no wall-clock
/// timestamp and does not grant a caller-selected app capability.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PolicyLease {
    pub app_id: AppId,
    pub worker_id: WorkerId,
    pub signing_key_id: String,
    pub assignment_revision: Revision,
    pub policy_revision: Revision,
    pub policy: AppPolicy,
    pub remaining_ms: NonZeroU64,
}
