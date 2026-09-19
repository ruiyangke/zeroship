//! The decode and fold a runtime batch travels through on the way to a journal.
//!
//! A worker hands the service a `WorkflowExecution`, and the run and lease it is
//! applied to come from the authorized task claim: `frontier::apply` reads the
//! run off the claim's row and the lease off the task token, never off the
//! batch. What the batch does decide is these two halves - the decoder that
//! normalizes the runtime's outcome array, and the fold that turns that array
//! into checkpoints and a run update - so those are what a case here binds.
//!
//! The floor on batch width belongs to `frontier::apply`, which refuses a
//! completion carrying no outcome; the completion-batch cases hold it.

use serde_json::{json, Value};
use zeroship_workflow::engine::{fold_outcomes, RunUpdate, StepCheckpoint};
use zeroship_workflow::{WorkflowExecution, WorkflowServiceError};

#[test]
fn an_untimed_wait_keeps_its_signal_age_through_decode_and_fold() {
    let (checkpoints, run_update) = fold(json!({"outcomes": [{
        "kind": "Wait", "ordinal": 0, "name": "approved",
        "signalType": "approved", "maxSignalAge": "2m"
    }]}))
    .unwrap();
    assert!(matches!(run_update, RunUpdate::Waiting { wake_at: None }));
    assert_eq!(checkpoints[0].max_signal_age_ms, Some(120_000));
    assert_eq!(
        checkpoints[0].signal_type.as_deref(),
        Some("approved"),
        "the decoded wait must carry the signal it waits on"
    );
}

#[test]
fn invalid_batches_do_not_become_empty_successful_completions() {
    for value in [
        json!({"outcomes": []}),
        json!({"outcomes": {}}),
        json!({"kind": "RunCompleted", "output": true}),
        json!({"checkpoints": [], "runUpdate": {"state": "completed"}}),
        json!({"outcomes": [{"kind": "Sleep", "ordinal": 0, "name": "nap"}]}),
        json!({"outcomes": [{"kind": "Sleep", "ordinal": 0, "name": "nap", "wakeAt": "1e100d"}]}),
        json!({"outcomes": [
            {"kind": "RunCompleted", "output": true},
            {"kind": "RunCompleted", "output": false}
        ]}),
    ] {
        assert!(fold(value.clone()).is_err(), "accepted {value}");
    }
    // The control: one well-formed batch, so the loop above is refusing these
    // batches rather than everything handed to the same pair.
    assert!(fold(json!({"outcomes": [{"kind": "RunCompleted", "output": true}]})).is_ok());
}

/// The pair a completion actually runs through: the shared runtime decoder, then
/// the shared fold `frontier::apply` applies to an authorized claim's run.
fn fold(value: Value) -> Result<(Vec<StepCheckpoint>, RunUpdate), WorkflowServiceError> {
    let execution = WorkflowExecution::from_runtime_value(value)?;
    fold_outcomes(&execution.outcomes).map_err(WorkflowServiceError::InvalidRequest)
}
