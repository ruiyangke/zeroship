//! Lifecycle rules shared by the embedded and deployed journal adapters.

use crate::errors::WorkflowServiceError;
use crate::operations::{RestartDeploy, RestartOptions};

/// Observed under the journal transaction's locks, before any history is removed.
#[derive(Debug, Clone, Copy, Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent restart hazards can be present together"
)]
pub struct RestartSafety {
    pub live_lease: bool,
    pub active_descendants: bool,
    pub active_compensation: bool,
    pub compensated_prefix: bool,
}

impl RestartSafety {
    /// # Errors
    /// Returns a conflict if execution, descendants or retained compensation
    /// make rewinding the journal unsafe.
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

/// Resolve the deploy policy without changing the immutable retained prefix.
///
/// # Errors
/// Rejects an invalid target or a partial restart onto another deploy.
pub fn restart_deploy_policy(
    options: &RestartOptions,
) -> Result<RestartDeploy, WorkflowServiceError> {
    if let Some(target) = &options.from {
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
        if options.deploy == Some(RestartDeploy::Latest) {
            return Err(WorkflowServiceError::Conflict(
                "partial restart cannot change deploy pin".into(),
            ));
        }
        Ok(RestartDeploy::Started)
    } else {
        Ok(options.deploy.unwrap_or(RestartDeploy::Latest))
    }
}
