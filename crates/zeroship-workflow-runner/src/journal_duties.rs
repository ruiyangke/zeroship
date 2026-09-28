//! What a host owes on behalf of the journal it acts through.
//!
//! # Why these are methods on the journal shape
//!
//! Each one is a duty an in-process host performs against its own journal and a
//! severed host does not perform at all. Written as calls, each becomes a place a
//! caller has to remember to skip, and skipping is invisible; written here, the
//! severed host DECLARES what it does not owe, at the definition site, where the
//! wrong form fails to compile rather than passing quietly.
//!
//! # Why one trait rather than one per duty
//!
//! There are already three of these -- the outbox drain, the runtime identity
//! check, and the placement guard `ConsumerScope::new` makes -- and the flip is
//! likely to find more. Three narrow traits would multiply, and each new duty
//! would look like it needed its own. One trait named for the relationship keeps
//! them together.
//!
//! # `()` means unreachable, not unchecked
//!
//! The unit implementation is not "this host skips the check". It is that the
//! condition each check defends against cannot arise without a journal handle: a
//! host holding none has no handle whose identity could disagree with its policy
//! binding, and no outbox of its own to leave undrained. The service owns the
//! journal and performs both against its own.

use crate::publication::publish_pending;
use std::time::Duration;
use zeroship_workflow::{
    service::{publication::JobPublisher, AppWorkflows, PolicyBinding},
    WorkflowServiceError,
};

/// The journal's side of a host's duties.
pub trait JournalDuties {
    /// Publish the intents this journal committed but has not yet handed to the
    /// queue.
    ///
    /// # Errors
    /// Reports whatever the journal refuses while publishing.
    fn drain(
        &self,
        publisher: &impl JobPublisher,
        timeout: Duration,
    ) -> impl std::future::Future<Output = Result<(), WorkflowServiceError>>;

    /// Refuse a journal handle that does not belong to the placement it was
    /// opened for.
    ///
    /// # Errors
    /// [`WorkflowServiceError::PermissionDenied`] when the handle names another
    /// app or another policy registry than `binding` does.
    fn belongs_to(&self, binding: &PolicyBinding) -> Result<(), WorkflowServiceError>;
}

impl JournalDuties for AppWorkflows {
    fn drain(
        &self,
        publisher: &impl JobPublisher,
        timeout: Duration,
    ) -> impl std::future::Future<Output = Result<(), WorkflowServiceError>> {
        publish_pending(self, publisher, timeout)
    }

    fn belongs_to(&self, binding: &PolicyBinding) -> Result<(), WorkflowServiceError> {
        if self.app_id() != binding.app_id() || !self.binding().same_binding(binding) {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        Ok(())
    }
}

impl JournalDuties for () {
    /// NO OUTBOX ON THIS SIDE. The outbox is a journal table and the service owns
    /// the journal, so the service drains it there. This is not a drain that was
    /// skipped; it is a drain that belongs to another process.
    fn drain(
        &self,
        _publisher: &impl JobPublisher,
        _timeout: Duration,
    ) -> impl std::future::Future<Output = Result<(), WorkflowServiceError>> {
        async { Ok(()) }
    }

    /// NO HANDLE WHOSE IDENTITY COULD DISAGREE. The check above refuses a journal
    /// handle opened for the wrong app; a host that holds no handle cannot hold a
    /// wrong one, so the condition is unreachable rather than unchecked. What the
    /// severed host still proves is that the PLACEMENT and the app agree, which
    /// `ConsumerScope::new` does against the policy binding in both shapes.
    fn belongs_to(&self, _binding: &PolicyBinding) -> Result<(), WorkflowServiceError> {
        Ok(())
    }
}
