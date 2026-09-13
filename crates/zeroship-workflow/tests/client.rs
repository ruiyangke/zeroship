use serde_json::{json, Value};
use zeroship_core::AppId;
use zeroship_workflow::operations::{RestartOptions, RestartTarget, StartOptions};
use zeroship_workflow::{app_scoped_token, WorkflowClientConfig, WorkflowHttpMethod};

const TEST_CONTROL_KEY: &str = "test-control-key";

#[test]
fn derives_the_app_scoped_token_with_the_shared_core_helper() {
    let app_id = AppId::mint();
    let token = app_scoped_token(TEST_CONTROL_KEY, app_id.as_str());
    assert_eq!(
        token,
        zeroship_core::auth::derive_app_scoped_control_token(TEST_CONTROL_KEY, app_id.as_str())
    );
    assert!(zeroship_core::auth::validate_app_scoped_control_token(
        &token,
        TEST_CONTROL_KEY,
        app_id.as_str()
    ));
    assert!(!zeroship_core::auth::validate_app_scoped_control_token(
        &token,
        "wrong-key",
        app_id.as_str()
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
        StartOptions {
            input: json!({ "orderId": 42 }),
            key: Some("cart-42".into()),
            ..Default::default()
        },
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
        RestartOptions {
            from: Some(RestartTarget {
                name: "charge".into(),
                occurrence: None,
            }),
            ..Default::default()
        },
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

#[test]
fn output_requests_keep_the_host_scope_and_escape_step_names() {
    use zeroship_workflow::client::build_read_step_output_request;
    let config = WorkflowClientConfig::new("http://control.test", "app_a", "scoped-token");
    let request = build_read_step_output_request(&config, "run_a", "part/next?other=1", 2).unwrap();
    assert_eq!(request.url, "http://control.test/internal/workflows/runs/run_a/steps/part%2Fnext%3Fother=1/output?occurrence=2");
    assert_eq!(request.app_id_header, "app_a");
    assert_eq!(request.authorization, "Bearer scoped-token");
    let escaped = build_read_step_output_request(&config, "run_a", "part\\next", 0).unwrap();
    assert!(escaped.url.contains("/steps/part%5Cnext/output"));
    for name in ["", ".", ".."] {
        assert!(build_read_step_output_request(&config, "run_a", name, 0).is_err());
        assert!(build_read_step_output_request(&config, name, "step", 0).is_err());
    }
}
