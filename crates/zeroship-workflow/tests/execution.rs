//! The decode and fold a runtime batch travels through on the way to a journal.
//!
//! A worker hands the service a `WorkflowExecution`, which carries outcomes and
//! nothing else. The task id and token travel beside it, `inspect_task` resolves
//! the claim from those, and `frontier::apply` reads the run off that claim's
//! row - so the batch selects neither the run nor the lease it settles. What the
//! batch does decide is these two halves: the decoder that normalizes the
//! runtime's outcome array, and the fold that turns that array into checkpoints
//! and a run update. Those are what a case here binds.
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

/// A run that returned nothing must fold to an ABSENT output, not to an inline
/// JSON null.
///
/// The decoder normalizes a bare `RunCompleted` by writing `output: null` into
/// the outcome, so the two spellings a runtime can send arrive here identical.
/// Everything downstream reads that as the absence of a result: the runner
/// stages an object for a value a run returned and none for a run that returned
/// nothing, and `frontier::apply` refuses an inline run output outright. A
/// `Some(Value::Null)` here would make a run that returned nothing
/// indistinguishable from one whose result the executor failed to stage.
#[test]
fn a_run_that_returned_nothing_folds_to_an_absent_output() {
    for value in [
        json!({"outcomes": [{"kind": "RunCompleted"}]}),
        json!({"outcomes": [{"kind": "RunCompleted", "output": null}]}),
    ] {
        let (_, run_update) = fold(value.clone()).unwrap();
        assert!(
            matches!(
                run_update,
                RunUpdate::Completed {
                    output: None,
                    output_ref: None
                }
            ),
            "{value}: {run_update:?}"
        );
    }
    // The control: the same fold over a run that DID return a value carries it,
    // so the absence above is the batch and not a fold that drops every output.
    let (_, carried) = fold(json!({"outcomes": [{"kind": "RunCompleted", "output": true}]})).unwrap();
    assert!(
        matches!(carried, RunUpdate::Completed { output: Some(Value::Bool(true)), .. }),
        "{carried:?}"
    );
}

/// A child's input reaches the fold as a DESCRIPTOR or not at all.
///
/// A child's input is a run's input, and a generation row keeps no inline slot
/// for one, so a value here is a child this journal could not admit. The runner
/// stages every value a body passes a child, which is why no executor reaches
/// the refusal; an executor that did would otherwise have its value silently
/// dropped and the child started from nothing.
///
/// The refusal names the child's input rather than the batch, because that is
/// the creator-facing thing that has to change.
#[test]
fn an_inline_child_input_is_refused_by_name() {
    let refused = fold(json!({"outcomes": [{
        "kind": "Child", "ordinal": 0, "name": "risk",
        "childWorkflowName": "Risk", "input": {"orderId": "ord_1"}
    }]}))
    .expect_err("an inline child input must not fold");
    assert!(
        matches!(
            &refused,
            WorkflowServiceError::InvalidRequest(message)
                if message == "workflow child input must be a staged reference"
        ),
        "{refused:?}"
    );

    // The controls. The same batch naming an OBJECT folds and carries the
    // descriptor, and the same batch naming nothing folds to no descriptor, so
    // the refusal above is the inline value and not the `Child` arm refusing
    // every child handed to it.
    let reference = json!({
        "hash": "a".repeat(64), "size": 17, "contentType": "application/json"
    });
    let (checkpoints, _) = fold(json!({"outcomes": [{
        "kind": "Child", "ordinal": 0, "name": "risk",
        "childWorkflowName": "Risk", "inputRef": reference
    }]}))
    .unwrap();
    assert_eq!(
        checkpoints[0]
            .child_input_ref
            .as_ref()
            .map(|carried| carried.hash.as_str()),
        Some("a".repeat(64).as_str()),
    );
    let (bare, _) = fold(json!({"outcomes": [{
        "kind": "Child", "ordinal": 0, "name": "risk", "childWorkflowName": "Risk"
    }]}))
    .unwrap();
    assert!(bare[0].child_input_ref.is_none());
    // A JSON null is how a body that passed nothing arrives, and it is the same
    // child as the bare batch above rather than an inline value to refuse.
    let (empty, _) = fold(json!({"outcomes": [{
        "kind": "Child", "ordinal": 0, "name": "risk",
        "childWorkflowName": "Risk", "input": null
    }]}))
    .unwrap();
    assert!(empty[0].child_input_ref.is_none());
}

/// The pair a completion actually runs through: the shared runtime decoder, then
/// the shared fold `frontier::apply` applies to an authorized claim's run.
fn fold(value: Value) -> Result<(Vec<StepCheckpoint>, RunUpdate), WorkflowServiceError> {
    let execution = WorkflowExecution::from_runtime_value(value)?;
    fold_outcomes(&execution.outcomes).map_err(WorkflowServiceError::InvalidRequest)
}
