//! The worker process's workflow host, against real services.
//!
//! A real worker binary, a real workflow manager, real Control and an owned
//! Testcontainers PostgreSQL. The run is started the way an end user starts
//! one - an ordinary HTTP request to the app through the gateway, reaching
//! `env.workflows` in a request isolate - and it completes only if the host
//! registered, took its placement, prepared the app from this process's own
//! resources, published its backend to the request thread, and executed the
//! job the manager delivered back. Then the worker is stopped the way an
//! orchestrator stops it.
//!
//! Native library availability proves none of that: every seam here is a
//! process boundary.

use crate::workflow_fleet::{self, Fleet};

use std::time::{Duration, Instant};

use serde_json::Value;
use zeroship_core::workflow_coordination::{AssignScope, RequestId, WorkerId, WorkerPage};
use zeroship_workflow_client::{ControlCoordinator, Options};

const DEADLINE: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_millis(250);

/// One ordinary app request through the gateway, by subdomain.
async fn ingress(fleet: &Fleet, path: &str) -> (u16, Value) {
    let response = cyper::Client::new()
        .get(format!("{}{path}", fleet.gateway_url))
        .expect("app ingress request")
        .header("host", format!("{}.zeroship.localhost", Fleet::APP_NAME))
        .expect("app host header")
        .send()
        .await
        .expect("app ingress exchange");
    let status = response.status().as_u16();
    let body = response.bytes().await.expect("app ingress body");
    let value: Value = serde_json::from_slice(&body)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into_owned()));
    LAST.with(|last| *last.borrow_mut() = Some((status, value.clone())));
    (status, value)
}

/// Poll `step` until it answers `Some`, failing with `what` at the deadline.
/// Every pass first fails on a service that died, so a crashed process is
/// reported as itself rather than as a timeout.
async fn until<T>(
    fleet: &mut Fleet,
    what: &str,
    mut step: impl AsyncFnMut(&Fleet) -> Option<T>,
) -> T {
    let deadline = Instant::now() + DEADLINE;
    loop {
        fleet.assert_alive();
        if let Some(value) = step(fleet).await {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "{what}; last app answer {:?}; see {}",
            LAST.with(|last| last.borrow().clone()),
            fleet.logs.display()
        );
        compio::time::sleep(POLL).await;
    }
}

thread_local! {
    /// The most recent ingress answer, so a poll that never gets what it
    /// waits for reports what it kept getting instead of only a deadline.
    static LAST: std::cell::RefCell<Option<(u16, Value)>> = const {
        std::cell::RefCell::new(None)
    };
}

/// The enrolled instance the worker registered with the manager under.
async fn registered_worker(fleet: &mut Fleet, manager: &ControlCoordinator) -> WorkerId {
    until(
        fleet,
        "the worker never registered with the manager",
        async |_| {
            manager
                .ready_workers(&WorkerPage { after: None })
                .await
                .ok()
                .and_then(|workers| workers.into_iter().next())
                .map(|worker| worker.worker_id)
        },
    )
    .await
}

#[compio::test]
async fn a_worker_host_runs_a_run_started_through_ordinary_app_ingress() {
    let mut fleet = Fleet::with_workflow_manager();
    let manager = ControlCoordinator::new(
        fleet.manager_url.as_deref().expect("a fleet manager"),
        workflow_fleet::service_auth("control"),
        Options::default(),
    )
    .expect("manager client");

    // The host registers under the identity it enrolled with at boot; nothing
    // in this test hands it a credential.
    let worker = registered_worker(&mut fleet, &manager).await;

    // Before any placement the app is unknown to this worker, so a request
    // isolate is refused rather than reaching some other backend. The poll is
    // only for the gateway's route table, which it pulls on its own interval:
    // the first answer the app itself produces is the assertion.
    let (status, body) = until(
        &mut fleet,
        "app ingress never reached the worker",
        async |fleet| {
            let (status, body) = ingress(fleet, "/__host/start/HostIngressWorkflow").await;
            (body.get("code").is_some() || body.get("id").is_some()).then_some((status, body))
        },
    )
    .await;
    assert_eq!(status, 503, "an unassigned app must refuse: {body}");
    assert_eq!(
        body["code"], "workflow_unavailable",
        "the refusal must be retryable: {body}"
    );

    // Placement is Control's to authorize. The worker cannot nominate itself.
    let app = fleet.app_id.clone();
    manager
        .assign(&AssignScope {
            request_id: RequestId::mint(),
            app_id: app.clone(),
            worker_id: worker.clone(),
            expected_revision: None,
        })
        .await
        .expect("place the app on the enrolled worker");

    // The host prepares the app and publishes its backend; Control's lifecycle
    // publisher registers and activates the deployment with the manager, which
    // delivers the activation here. Until both have happened a start is still
    // refused, so this poll is the readiness assertion.
    let run = until(
        &mut fleet,
        "the app never became ready on the worker",
        async |fleet| {
            let (status, body) = ingress(fleet, "/__host/start/HostIngressWorkflow").await;
            (status == 200)
                .then(|| body["id"].as_str().map(str::to_owned))
                .flatten()
        },
    )
    .await;
    assert!(run.starts_with("run_"), "{run}");

    // Nothing in this process advances the run: it completes only through the
    // manager delivering the job back to this worker's consumer.
    let output = until(
        &mut fleet,
        "the delivered run never completed",
        async |fleet| {
            let (status, body) =
                ingress(fleet, &format!("/__host/status/HostIngressWorkflow/{run}")).await;
            (status == 200 && body["state"] == "completed").then(|| body["output"].clone())
        },
    )
    .await;
    assert_eq!(
        output["echoed"]["via"], "ingress",
        "the delivered execution must carry the ingress input: {output}"
    );

    // Stopped the way an orchestrator stops it: HTTP drains, the host closes
    // its bindings and joins, and only then does the instance retire.
    let exit = fleet.terminate("worker");
    assert!(
        exit.success(),
        "a worker running a workflow host must exit cleanly on SIGTERM, got {exit}; see {}",
        fleet.logs.display()
    );
}
