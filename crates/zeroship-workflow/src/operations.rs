//! App-scoped workflow operations shared by native callers and transport adapters.

use crate::engine::WorkflowOutputRef;
use serde::{Deserialize, Serialize};
pub use zeroship_core::workflow_coordination::{
    DeliveredSignal, RestartDeploy, RestartOptions, RestartTarget, RestartedRun, RunOperation,
    RunState, RunStatus, SignalOptions, TransitionedRun,
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
    /// The payload object the run starts from, or none when it starts from
    /// nothing.
    ///
    /// A generation row keeps no inline slot for a run's input, so whatever
    /// accepted the caller's value staged it first and names the object here.
    /// The value itself never reaches this type, which is why a run's input
    /// costs the same however large it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_ref: Option<WorkflowOutputRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "ConflictPolicy::is_join")]
    pub on_conflict: ConflictPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartedRun {
    pub id: String,
    pub state: RunState,
}

