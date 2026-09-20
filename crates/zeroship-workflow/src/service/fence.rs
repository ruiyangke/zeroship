//! Settlement for the journal's read-modify-write sites.

use crate::WorkflowServiceError;

/// Settle a compare-and-set write that filtered on the value it read.
///
/// A read-modify-write names the value it observed in its own filter, so a
/// writer that moved that value in between leaves the update matching nothing.
/// Any count but one contradicts the state the caller read, and `invalid` names
/// the journal whose invariant that contradicts.
///
/// Every such site shares this one settlement. A site that reimplements it is
/// free to drop it, and a site that reads as ordinary code has already lost the
/// guard, which is how the check goes missing.
pub(super) fn changed_once(
    count: i64,
    invalid: impl FnOnce() -> WorkflowServiceError,
) -> Result<(), WorkflowServiceError> {
    if count == 1 {
        Ok(())
    } else {
        Err(invalid())
    }
}
