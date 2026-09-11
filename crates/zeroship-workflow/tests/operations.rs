use serde_json::json;
use zeroship_workflow::operations::{RestartOptions, RunOperation, RunStatus, StartOptions};

#[test]
fn app_operations_cannot_supply_another_app_scope() {
    assert!(serde_json::from_value::<StartOptions>(json!({
        "appId": "another-app",
        "input": null
    }))
    .is_err());
    assert!(serde_json::from_value::<RestartOptions>(json!({
        "from": { "name": "charge", "appId": "another-app" }
    }))
    .is_err());
}

#[test]
fn invalid_policies_and_lifecycle_operations_are_rejected_at_decode() {
    assert!(serde_json::from_value::<StartOptions>(json!({ "onConflict": "overwrite" })).is_err());
    assert!(serde_json::from_value::<RestartOptions>(json!({ "deploy": "arbitrary-deploy" })).is_err());
    assert!(serde_json::from_value::<RestartOptions>(json!({
        "from": { "name": "charge", "occurrence": -1 }
    }))
    .is_err());
    assert!(serde_json::from_value::<RunOperation>(json!("delete")).is_err());
}

#[test]
fn status_response_rejects_an_unknown_persisted_state() {
    assert!(serde_json::from_value::<RunStatus>(json!({
        "state": "replaced", "output": null, "error": null
    }))
    .is_err());
}
