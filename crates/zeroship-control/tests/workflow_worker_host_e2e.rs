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

use crate::workflow_fleet::Fleet;

use std::time::{Duration, Instant};

use serde_json::Value;

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

#[compio::test]
async fn a_worker_host_runs_a_run_started_through_ordinary_app_ingress() {
    let mut fleet = Fleet::with_workflow_manager();

    // The host registers under the identity it enrolled with at boot; nothing
    // in this test hands it a credential, and nothing here nominates a worker.
    // Control's lifecycle publisher registers and activates the deployment with
    // the manager, whose placement lane gives the app an owner because it has
    // claimable work. Until the host has taken that placement, prepared the app
    // and published its backend, a start is refused, so this poll is the
    // readiness assertion for the whole chain.
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
