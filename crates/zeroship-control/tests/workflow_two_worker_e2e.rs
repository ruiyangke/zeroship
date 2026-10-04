//! A worker that never ran an app serves and executes it.
//!
//! One worker, W1, is enrolled and runs the app by pulling its claimable work
//! from the zone queue. A second worker, W2, is reserved and NOT started;
//! the gateway's route list names only W2. Phase 1 reaches W1 through the
//! gateway's own dispatch frame and completes a run. Phase 2 stops W1 and
//! asserts the queue holds NO claimable `advance` row for the app, so W2 has
//! nothing of it to pull. Phase 3 starts W2 and reaches it through the gateway: the run
//! W1 started is readable, a fresh start is admitted, and the fresh run
//! completes. Only the worker's ZONE can admit that start, because W2 holds no
//! job of the app and the queue has none to give it.

use crate::support::workflow_fleet::{self, Fleet};

use std::time::{Duration, Instant};

use compio_postgres::NoTls;
use serde_json::Value;

const DEADLINE: Duration = Duration::from_secs(180);
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

/// The gateway's own dispatch hop directly to W1, without its route table.
///
/// With the gateway's route list naming only the not-yet-started W2, this is
/// how phase 1 reaches W1: the same path, the same dispatch frame and the same
/// per-call peer credential the gateway mints.
async fn dispatch(fleet: &Fleet, plan: &str, path: &str) -> (u16, Value) {
    let frame = zeroship_core::dispatch_frame::encode_dispatch_frame(
        "GET",
        &format!("http://{}.zeroship.localhost{path}", Fleet::APP_NAME),
        &[],
        b"",
    )
    .expect("dispatch frame");
    let response = cyper::Client::new()
        .post(format!(
            "{}/dispatch/{}",
            fleet.worker_url,
            fleet.app_id.as_str()
        ))
        .expect("worker dispatch request")
        .header("x-app-id", fleet.app_id.as_str())
        .expect("app id header")
        .header("x-plan-id", plan)
        .expect("plan header")
        .header("x-request-id", uuid::Uuid::new_v4().to_string())
        .expect("request id header")
        .header(
            "authorization",
            workflow_fleet::worker_dispatch_authorization(),
        )
        .expect("peer credential")
        .body(frame)
        .send()
        .await
        .expect("worker dispatch exchange");
    let status = response.status().as_u16();
    let body = response.bytes().await.expect("worker dispatch body");
    let value: Value = serde_json::from_slice(&body)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into_owned()));
    LAST.with(|last| *last.borrow_mut() = Some((status, value.clone())));
    (status, value)
}

thread_local! {
    /// The most recent app answer, so a poll that never gets what it waits for
    /// reports what it kept getting instead of only a deadline.
    static LAST: std::cell::RefCell<Option<(u16, Value)>> = const {
        std::cell::RefCell::new(None)
    };
}

/// Poll `step` until it answers `Some`, failing with `what` at the deadline.
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

/// The app's plan, needed for the worker dispatch frame.
async fn plan_of(fleet: &Fleet) -> String {
    let (client, connection) = compio_postgres::connect(&fleet.database.url(), NoTls)
        .await
        .expect("open the platform database");
    let driver = compio::runtime::spawn(connection.run());
    let plan: String = client
        .query_one(
            "SELECT plan_id FROM zeroship.apps WHERE id = $1",
            &[&fleet.app_id.as_str()],
        )
        .await
        .expect("read the app's plan")
        .get(0);
    drop(client);
    driver.await.expect("platform task").expect("platform driver");
    plan
}

/// The workers that settled the app's `advance` jobs.
///
/// A settled row keeps the delivery fence that settled it, so its `worker_id` is
/// the worker that executed it. Ready rows are left out: a row given back or not
/// yet claimed names nobody, and job ids are not ordered by time, so "the latest
/// row" is not a question the table answers.
async fn settled_advance_workers(fleet: &Fleet) -> std::collections::BTreeSet<String> {
    let (client, connection) = compio_postgres::connect(&fleet.database.url(), NoTls)
        .await
        .expect("open the platform database");
    let driver = compio::runtime::spawn(connection.run());
    let workers = client
        .query(
            "SELECT DISTINCT worker_id FROM workflow_manager.jobs \
             WHERE app_id = $1 AND operation_kind = 'advance' AND state = 'settled'",
            &[&fleet.app_id.as_str()],
        )
        .await
        .expect("read the app's settled advance jobs")
        .into_iter()
        .map(|row| row.get::<_, Option<String>>(0).expect("a settled advance job names its worker"))
        .collect();
    drop(client);
    driver.await.expect("platform task").expect("platform driver");
    workers
}

/// Claimable `advance` rows this app holds.
async fn claimable_advance(fleet: &Fleet) -> i64 {
    let (client, connection) = compio_postgres::connect(&fleet.database.url(), NoTls)
        .await
        .expect("open the platform database");
    let driver = compio::runtime::spawn(connection.run());
    let rows: i64 = client
        .query_one(
            "SELECT count(*) FROM workflow_manager.jobs \
             WHERE app_id = $1 AND operation_kind = 'advance' AND state = 'ready'",
            &[&fleet.app_id.as_str()],
        )
        .await
        .expect("count the app's claimable advance rows")
        .get(0);
    drop(client);
    driver.await.expect("platform task").expect("platform driver");
    rows
}

#[compio::test(crate = "crate::support::live")]
async fn a_worker_that_never_ran_the_app_serves_and_executes_it() {
    let mut fleet = Fleet::with_deferred_second_worker();
    let plan = plan_of(&fleet).await;

    // PHASE 1. The gateway routes only to W2, which is not running, so reach W1
    // through its own dispatch frame. W1 pulled the app's work from its zone
    // queue once the deployment activation made it claimable.
    let run = until(&mut fleet, "W1 never served the app", async |fleet| {
        let (status, body) = dispatch(fleet, &plan, "/__host/start/HostIngressWorkflow").await;
        (status == 200)
            .then(|| body["id"].as_str().map(str::to_owned))
            .flatten()
    })
    .await;
    assert!(run.starts_with("run_"), "{run}");

    let output = until(&mut fleet, "W1 never completed the run", async |fleet| {
        let (status, body) = dispatch(
            fleet,
            &plan,
            &format!("/__host/status/HostIngressWorkflow/{run}"),
        )
        .await;
        (status == 200 && body["state"] == "completed").then(|| body["output"].clone())
    })
    .await;
    assert_eq!(
        output["echoed"]["via"], "ingress",
        "the delivered execution must carry the ingress input: {output}"
    );
    // Only W1 was running, so every settled advance job names W1 and nobody else.
    let phase_one = settled_advance_workers(&fleet).await;
    let [w1] = <[String; 1]>::try_from(phase_one.into_iter().collect::<Vec<_>>())
        .unwrap_or_else(|workers| panic!("phase 1 must be settled by one worker: {workers:?}"));

    // PHASE 2. Stop W1 the way an orchestrator does, and prove there is no
    // claimable row W2 could pull.
    let exit = fleet.terminate("worker");
    assert!(
        exit.success(),
        "W1 must exit cleanly on SIGTERM, got {exit}; see {}",
        fleet.logs.display()
    );
    assert_eq!(
        claimable_advance(&fleet).await,
        0,
        "a claimable advance row would hand W2 the app before the zone rule is exercised"
    );

    // PHASE 3. Start W2. The gateway now routes to it. The run W1 started is
    // readable, and a fresh start is admitted while W2 holds no job of the app;
    // the zone alone admits it.
    fleet.start_second_worker().await;

    let body = until(
        &mut fleet,
        "W2 never served the run W1 started",
        async |fleet| {
            let (status, body) = ingress(
                fleet,
                &format!("/__host/status/HostIngressWorkflow/{run}"),
            )
            .await;
            (status == 200).then_some(body)
        },
    )
    .await;
    assert_eq!(
        body["state"], "completed",
        "W2 must answer with the run W1 completed: {body}"
    );

    let started = until(&mut fleet, "W2 never admitted a start", async |fleet| {
        let (status, body) = ingress(fleet, "/__host/start/HostIngressWorkflow").await;
        (status == 200)
            .then(|| body["id"].as_str().map(str::to_owned))
            .flatten()
    })
    .await;
    assert!(started.starts_with("run_"), "{started}");

    let output = until(&mut fleet, "W2 never completed the run", async |fleet| {
        let (status, body) = ingress(
            fleet,
            &format!("/__host/status/HostIngressWorkflow/{started}"),
        )
        .await;
        (status == 200 && body["state"] == "completed").then(|| body["output"].clone())
    })
    .await;
    assert_eq!(
        output["echoed"]["via"], "ingress",
        "the delivered execution must carry the ingress input: {output}"
    );
    // W1 is gone, so the fresh run's jobs can only have been settled by W2: a
    // second worker now appears among the settled rows.
    let phase_three = settled_advance_workers(&fleet).await;
    assert!(
        phase_three.contains(&w1) && phase_three.len() == 2,
        "the fresh run was not executed by a second worker: {phase_three:?}"
    );

    let exit = fleet.terminate("worker-2");
    assert!(
        exit.success(),
        "W2 must exit cleanly on SIGTERM, got {exit}; see {}",
        fleet.logs.display()
    );
}
