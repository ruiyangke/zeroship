use chrono::Utc;
use serde_json::{json, Value};
use zeroship_workflow::engine::{RunUpdate, StepRequest, WorkflowEngineConfig};
use zeroship_workflow::{WorkflowExecution, WorkflowInvocation, WorkflowServiceError};

#[test]
fn replay_input_keeps_claim_credentials_in_the_host() {
    let config = WorkflowEngineConfig::default();
    let request = StepRequest {
        run_id: "run_claimed".into(),
        app_id: uuid::Uuid::now_v7(),
        workflow_name: "Checkout".into(),
        deploy_id: "dep_pinned".into(),
        deploy_hash: "snapshot-pinned".into(),
        dispatch_nonce: "lease-secret".into(),
        phase: "running".into(),
        input: Some(json!({"order": "checkout"})),
        started_at: Utc::now(),
        journal: vec![],
        owner_id: "trusted-worker".into(),
        stuck_strike_limit: config.stuck_strike_limit,
        max_child_depth: config.max_child_depth,
        max_live_descendants: config.max_live_descendants,
        max_start_many_batch: config.max_start_many_batch,
        journal_limits: config.journal_limits,
    };
    let encoded = serde_json::to_value(WorkflowInvocation::from(&request)).unwrap();
    for private in ["nonce", "dispatchNonce", "ownerId", "journalLimits"] {
        assert!(encoded.get(private).is_none(), "exposed {private}");
    }
    let text = encoded.to_string();
    assert!(!text.contains(&request.dispatch_nonce));
    assert!(!text.contains(&request.owner_id));
    assert_eq!(encoded["deployHash"], request.deploy_hash);
    assert_eq!(encoded["trigger"]["runId"], request.run_id);
    assert_eq!(encoded["trigger"]["input"], request.input.unwrap());
}

#[test]
fn runtime_output_cannot_select_another_run_or_lease() {
    let returned = json!({
        "runId": "run_other_app",
        "dispatchNonce": "other-lease",
        "nonce": "other-lease",
        "outcomes": [{"kind": "RunCompleted", "output": "done"}]
    });
    let result = WorkflowExecution::from_runtime_json(&returned.to_string())
        .unwrap()
        .into_step_result("run_claimed".into(), "claimed-lease".into())
        .unwrap();
    assert_eq!(result.run_id, "run_claimed");
    assert_eq!(result.dispatch_nonce, "claimed-lease");
    assert_eq!(result.run_update.output(), Some(json!("done")));
}

#[test]
fn untimed_wait_and_signal_age_use_the_shared_decoder() {
    let result = decode(json!({"outcomes": [{
        "kind": "Wait", "ordinal": 0, "name": "approved",
        "signalType": "approved", "maxSignalAge": "2m"
    }]}))
    .unwrap();
    assert!(matches!(
        result.run_update,
        RunUpdate::Waiting { wake_at: None }
    ));
    assert_eq!(result.checkpoints[0].max_signal_age_ms, Some(120_000));
    assert_eq!(
        result.checkpoints[0].signal_type.as_deref(),
        Some("approved")
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
        assert!(decode(value.clone()).is_err(), "accepted {value}");
    }
    assert!(WorkflowExecution { outcomes: vec![] }
        .into_step_result("run_claimed".into(), "lease".into())
        .is_err());
}

fn decode(value: Value) -> Result<zeroship_workflow::engine::StepResult, WorkflowServiceError> {
    WorkflowExecution::from_runtime_value(value)?
        .into_step_result("run_claimed".into(), "lease".into())
}
