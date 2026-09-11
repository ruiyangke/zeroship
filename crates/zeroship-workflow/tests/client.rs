use serde_json::{json, Value};
use uuid::Uuid;
use zeroship_workflow::{app_scoped_token, WorkflowClientConfig, WorkflowHttpMethod};

const TEST_CONTROL_KEY: &str = "test-control-key";

#[test]
fn derives_the_app_scoped_token_with_the_shared_core_helper() {
    let app_id = Uuid::new_v4().to_string();
    let token = app_scoped_token(TEST_CONTROL_KEY, &app_id);
    assert_eq!(
        token,
        zeroship_core::auth::derive_app_scoped_control_token(TEST_CONTROL_KEY, &app_id)
    );
    assert!(zeroship_core::auth::validate_app_scoped_control_token(
        &token,
        TEST_CONTROL_KEY,
        &app_id
    ));
    assert!(!zeroship_core::auth::validate_app_scoped_control_token(
        &token,
        "wrong-key",
        &app_id
    ));
}

#[test]
fn builds_authenticated_workflow_instance_requests() {
    let cfg = WorkflowClientConfig::new(
        "http://control.test/",
        "app_123",
        app_scoped_token(TEST_CONTROL_KEY, "app_123"),
    );
    let start = zeroship_workflow::client::build_start_request(
        &cfg,
        "Checkout/Final",
        json!({ "input": { "orderId": 42 }, "key": "cart-42" }),
    )
    .expect("start request");
    assert_eq!(start.method, WorkflowHttpMethod::Post);
    assert_eq!(
        start.url,
        "http://control.test/internal/workflows/Checkout%2FFinal/runs"
    );
    assert_eq!(start.app_id_header, "app_123");
    assert_eq!(start.authorization, format!("Bearer {}", cfg.token()));
    assert_eq!(
        serde_json::from_slice::<Value>(start.body.as_deref().expect("body")).unwrap(),
        json!({ "input": { "orderId": 42 }, "key": "cart-42" })
    );

    let status = zeroship_workflow::client::build_get_status_request(&cfg, "run_abc/def")
        .expect("status request");
    assert_eq!(status.method, WorkflowHttpMethod::Get);
    assert_eq!(
        status.url,
        "http://control.test/internal/workflows/runs/run_abc%2Fdef"
    );
    assert!(status.body.is_none());

    let restart = zeroship_workflow::client::build_restart_request(
        &cfg,
        "run_abc/def",
        json!({ "from": { "name": "charge" } }),
    )
    .expect("restart request");
    assert_eq!(restart.method, WorkflowHttpMethod::Post);
    assert_eq!(
        restart.url,
        "http://control.test/internal/workflows/runs/run_abc%2Fdef/restart"
    );
    assert_eq!(
        serde_json::from_slice::<Value>(restart.body.as_deref().expect("body")).unwrap(),
        json!({ "from": { "name": "charge" } })
    );
}
