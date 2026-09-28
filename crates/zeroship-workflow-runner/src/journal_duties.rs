//! What a host owes on behalf of the journal it acts through.
//!
//! # Why this is a method on the journal shape
//!
//! It is a duty an in-process host performs against its own journal and a severed
//! host does not perform at all. Written as a call, it becomes a place a caller
//! has to remember to skip, and skipping is invisible; written here, the severed
//! host DECLARES what it does not owe, at the definition site, where the wrong
//! form fails to compile rather than passing quietly.
//!
//! # `()` means unreachable, not unchecked
//!
//! The unit implementation is not "this host skips the check". It is that the
//! condition it defends against cannot arise without a journal handle: a host
//! holding none has no handle whose identity could disagree with its policy
//! binding. The service owns the journal and performs the check against its own.

use zeroship_workflow::{
    service::{AppWorkflows, PolicyBinding},
    WorkflowServiceError,
};

/// The journal's side of a host's duties.
pub trait JournalDuties {
    /// Refuse a journal handle that does not belong to the placement it was
    /// opened for.
    ///
    /// # Errors
    /// [`WorkflowServiceError::PermissionDenied`] when the handle names another
    /// app or another policy registry than `binding` does.
    fn belongs_to(&self, binding: &PolicyBinding) -> Result<(), WorkflowServiceError>;
}

impl JournalDuties for AppWorkflows {
    fn belongs_to(&self, binding: &PolicyBinding) -> Result<(), WorkflowServiceError> {
        if self.app_id() != binding.app_id() || !self.binding().same_binding(binding) {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        Ok(())
    }
}

impl JournalDuties for () {
    /// NO HANDLE WHOSE IDENTITY COULD DISAGREE. The check above refuses a journal
    /// handle opened for the wrong app; a host that holds no handle cannot hold a
    /// wrong one, so the condition is unreachable rather than unchecked. What the
    /// severed host still proves is that the PLACEMENT and the app agree, which
    /// `ConsumerScope::new` does against the policy binding in both shapes.
    fn belongs_to(&self, _binding: &PolicyBinding) -> Result<(), WorkflowServiceError> {
        Ok(())
    }
}
