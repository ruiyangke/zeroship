use crate::{
    engine::StepCheckpoint,
    service::{continuations::HistoricalMember, journal, models::GenerationOutcome},
};
use serde_json::json;

/// A child that reached a terminal state without reporting an error of its own
/// leaves the parent step carrying the platform's own verdict. The spelling
/// under `type` is the identity the V8 bridge reconstructs a class from, so a
/// replayed result only matches the recorded step when both sides agree on it.
fn cancelled_member() -> HistoricalMember {
    HistoricalMember {
        id: "mem".into(),
        head_id: "head".into(),
        revision: 1,
        run_id: "child".into(),
        generation: 0,
        state: "cancelled".into(),
        outcome: GenerationOutcome { output: None, output_ref: None, error: None },
    }
}

fn step_with_error(error: serde_json::Value) -> StepCheckpoint {
    serde_json::from_value(json!({
        "ordinal": 0,
        "name": "call",
        "nameOccurrence": 0,
        "kind": "child",
        "state": "failed",
        "error": error,
        "childRunId": "child",
    }))
    .unwrap()
}

#[test]
fn a_cancelled_child_is_named_under_the_key_the_bridge_reads() {
    let step = step_with_error(json!({
        "type": "ChildCancelledError",
        "message": "child workflow was cancelled",
    }));
    assert!(journal::validate_child_result(&step, &cancelled_member()).is_ok());
}

/// The control, differing from the case above in the recorded error alone. A
/// step carrying the identity under any other key is not the verdict this
/// engine writes, so replay must refuse it rather than accept a value the
/// bridge would hand the creator as a bare `Error`.
#[test]
fn a_cancelled_child_named_under_another_key_is_refused() {
    let step = step_with_error(json!({
        "name": "ChildWorkflowError",
        "message": "child workflow was cancelled",
    }));
    assert!(journal::validate_child_result(&step, &cancelled_member()).is_err());
}
