//! Lifecycle rules shared by the embedded and deployed journal adapters.

use crate::errors::WorkflowServiceError;
use zeroship_core::workflow_coordination::InvalidRestart;

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

impl From<InvalidRestart> for WorkflowServiceError {
    fn from(error: InvalidRestart) -> Self {
        match error {
            InvalidRestart::PartialLatest => Self::Conflict(error.to_string()),
            InvalidRestart::EmptyTargetName | InvalidRestart::TargetOccurrenceOutOfRange => {
                Self::InvalidRequest(error.to_string())
            }
        }
    }
}
