//! Exercise the internal dispatch boundary with a real fleet and reverse proxy.
use crate::workflow_fleet::{self, Fleet};
use serde_json::{json, Value};
use testcontainers::{core::WaitFor, runners::SyncRunner, GenericImage, ImageExt};
use zeroship_core::service_peers::service_issuer;
use zeroship_workflow::{
    app_scoped_token, HttpWorkflowBackend, WorkflowBackend, WorkflowClientConfig,
};

fn backend(fleet: &Fleet) -> HttpWorkflowBackend {
    let app = fleet.app_id.to_string();
    HttpWorkflowBackend::new(WorkflowClientConfig::new(
        &fleet.control_url,
        &app,
        app_scoped_token(workflow_fleet::CONTROL_KEY, &app),
    ))
}

async fn post(
    url: &str,
    host: &str,
    body: &Value,
    authorization: Option<String>,
) -> (u16, Vec<u8>) {
    let request = cyper::Client::new()
        .post(url)
        .unwrap()
        .header("host", host)
        .unwrap()
        .header("content-type", "application/json")
        .unwrap();
    let request = if let Some(authorization) = authorization {
        request.header("authorization", authorization).unwrap()
    } else {
        request
    };
    let response = request
        .body(serde_json::to_vec(body).unwrap())
        .send()
        .await
        .unwrap();
    (
        response.status().as_u16(),
        response.bytes().await.unwrap().to_vec(),
    )
}

async fn step_count(fleet: &Fleet, run: &str) -> i64 {
    let (client, connection) =
        compio_postgres::connect(&fleet.database.url(), compio_postgres::NoTls)
            .await
            .unwrap();
    let driver = compio::runtime::spawn(connection.run());
    let tables = zeroship_workflow::store::pg::WorkflowTables::for_app_id(&fleet.app_id);
    let count = client
        .query_one(
            &format!(
                "SELECT COUNT(*)::bigint FROM {} WHERE run_id = $1",
                tables.steps
            ),
            &[&run],
        )
        .await
        .unwrap()
        .get(0);
    drop(client);
    driver.await.unwrap().unwrap();
    count
}

async fn start(fleet: &Fleet) -> String {
    let result = backend(fleet)
        .start(
            "ChildEchoWorkflow".into(),
            json!({"input":{"value":"authorization-probe"}}),
        )
        .await
        .unwrap();
    result["id"]
        .as_str()
        .expect("created workflow run id")
        .into()
}

fn control_assertion() -> Option<String> {
    workflow_fleet::service_auth("control")
        .authorization_for(&service_issuer("svc/gateway").unwrap())
}

#[compio::test]
async fn the_gateway_requires_a_control_service_assertion_before_app_lookup() {
    let mut fleet = Fleet::start();
    let run = start(&fleet).await;
    let request = json!({"runId":run,"appId":fleet.app_id});
    let url = format!("{}/__zeroship/internal/workflow-advance", fleet.gateway_url);
    let denied = post(&url, "localhost", &request, None).await;
    assert_eq!(denied.0, 401);
    let unknown = json!({"runId":"run_missing","appId":uuid::Uuid::new_v4()});
    assert_eq!(
        post(&url, "localhost", &unknown, None).await,
        denied,
        "unauthenticated requests must not reveal app existence"
    );
    for credential in [
        "Bearer legacy-worker-key".to_owned(),
        format!(
            "Bearer {}",
            app_scoped_token(workflow_fleet::CONTROL_KEY, &fleet.app_id.to_string())
        ),
        workflow_fleet::service_auth("worker")
            .authorization_for(&service_issuer("svc/gateway").unwrap())
            .unwrap(),
    ] {
        let response = post(&url, "localhost", &request, Some(credential)).await;
        assert!(
            [401, 403].contains(&response.0),
            "unexpected unauthorized response: {response:?}"
        );
    }
    for host in ["workflow-fixture.zeroship.localhost", "evil.example.com"] {
        assert_eq!(post(&url, host, &request, None).await.0, 404);
    }
    assert_eq!(
        backend(&fleet).status(run.clone()).await.unwrap()["state"],
        "queued"
    );
    assert_eq!(step_count(&fleet, &run).await, 0);

    // Reproduce the repository's wildcard host routing with an owned proxy.
    let proxy_port = workflow_fleet::port();
    let config = format!("{{\n auto_https off\n admin off\n}}\nhttp://*.zeroship.localhost:{proxy_port} {{\n reverse_proxy {}\n}}\n", fleet.gateway_url);
    let _proxy = GenericImage::new("caddy", "2")
        .with_wait_for(WaitFor::message_on_stderr("serving initial configuration"))
        .with_network("host")
        .with_copy_to("/etc/caddy/Caddyfile", config.into_bytes())
        .start()
        .expect("workflow edge tests require Caddy through Testcontainers");
    let proxy = format!("http://127.0.0.1:{proxy_port}/__zeroship/internal/workflow-advance");
    assert_eq!(
        post(
            &proxy,
            &format!("evil.zeroship.localhost:{proxy_port}"),
            &request,
            None
        )
        .await
        .0,
        404
    );
    let dotless = post(&proxy, "localhost", &request, None).await;
    assert_ne!(
        dotless, denied,
        "the wildcard proxy must not forward a dotless Host"
    );
    assert_eq!(step_count(&fleet, &run).await, 0);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let response = post(&url, "localhost", &request, control_assertion()).await;
        if response.0 == 200 {
            let body: Value = serde_json::from_slice(&response.1).unwrap();
            if body["ack"] == true && step_count(&fleet, &run).await > 0 {
                break;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "authorized workflow did not advance: {response:?}; logs: {}",
            fleet.logs.display()
        );
        compio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    fleet.assert_alive();
}

#[compio::test]
async fn the_worker_default_refuses_advancement_even_from_an_authenticated_gateway() {
    let mut fleet = Fleet::with_advance(false);
    let run = start(&fleet).await;
    let app = zeroship_core::app_id::AppId::from_uuid(&fleet.app_id);
    let url = format!(
        "{}/workflow-advance-unsigned/{}",
        fleet.worker_url,
        app.as_str()
    );
    let credential = workflow_fleet::service_auth("gateway")
        .authorization_for(&service_issuer("svc/worker").unwrap());
    let response = post(
        &url,
        "localhost",
        &json!({"runId":run,"appId":fleet.app_id}),
        credential,
    )
    .await;
    assert_eq!(response.0, 403);
    assert!(String::from_utf8_lossy(&response.1).contains("workflow advance unsigned disabled"));
    assert_eq!(
        backend(&fleet).status(run.clone()).await.unwrap()["state"],
        "queued"
    );
    assert_eq!(step_count(&fleet, &run).await, 0);
    fleet.assert_alive();
}
