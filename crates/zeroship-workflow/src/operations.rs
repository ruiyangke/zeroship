//! App-scoped workflow operations shared by native callers and transport adapters.

use serde::{Deserialize, Serialize};
use serde_json::Value;
pub use zeroship_core::workflow_coordination::{
    RestartDeploy, RestartOptions, RestartTarget, RunOperation, RunState,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConflictPolicy {
    #[default]
    Join,
    Reject,
    Replace,
}

impl ConflictPolicy {
    #[allow(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde skip_serializing_if requires a borrowed field"
    )]
    fn is_join(&self) -> bool {
        *self == Self::Join
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StartOptions {
    #[serde(default)]
    pub input: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "ConflictPolicy::is_join")]
    pub on_conflict: ConflictPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignalOptions {
    #[serde(rename = "type")]
    pub signal_type: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartedRun {
    pub id: String,
    pub state: RunState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunStatus {
    pub state: RunState,
    pub output: Option<Value>,
    pub error: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveredSignal {
    pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionedRun {
    pub state: RunState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestartedRun {
    pub run_id: String,
    pub state: RunState,
    pub restarted_from_ordinal: Option<u32>,
    pub pinned_to: String,
}
