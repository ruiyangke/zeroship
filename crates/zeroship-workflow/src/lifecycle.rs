//! Lifecycle rules shared by the embedded and deployed journal adapters.

use crate::errors::WorkflowServiceError;
use crate::operations::{RestartDeploy, RestartOptions};

/// Observed under the journal transaction's locks, before any history is removed.
#[derive(Debug, Clone, Copy, Default)]
pub struct RestartSafety {
    pub live_lease: bool,
    pub active_descendants: bool,
    pub active_compensation: bool,
    pub compensated_prefix: bool,
}

impl RestartSafety {
    pub fn check(self) -> Result<(), WorkflowServiceError> {
        let reason = if self.live_lease {
            "cannot restart while an execution lease is live"
        } else if self.active_descendants {
            "cannot restart while descendant workflows are active"
        } else if self.active_compensation {
            "cannot restart while compensation is active"
        } else if self.compensated_prefix {
            "cannot retain a compensated prefix; use a full restart"
        } else {
            return Ok(());
        };
        Err(WorkflowServiceError::Conflict(reason.into()))
    }
}

impl RestartOptions {
    /// Resolve the deploy policy without changing the immutable retained prefix.
    pub fn deploy_policy(&self) -> Result<RestartDeploy, WorkflowServiceError> {
        if let Some(target) = &self.from {
            if target.name.is_empty() {
                return Err(WorkflowServiceError::InvalidRequest(
                    "restart target name must not be empty".into(),
                ));
            }
            if target
                .occurrence
                .is_some_and(|value| value > i32::MAX as u32)
            {
                return Err(WorkflowServiceError::InvalidRequest(
                    "restart target occurrence exceeds the journal ordinal range".into(),
                ));
            }
            if self.deploy == Some(RestartDeploy::Latest) {
                return Err(WorkflowServiceError::Conflict(
                    "partial restart cannot change deploy pin".into(),
                ));
            }
            Ok(RestartDeploy::Started)
        } else {
            Ok(self.deploy.unwrap_or(RestartDeploy::Latest))
        }
    }
}
