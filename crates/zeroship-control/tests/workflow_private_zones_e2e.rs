//! The private-zone process contract: the decisive proof of the cutover.
//!
//! Two PostgreSQL servers, not two schemas. Control, the workflow manager and
//! the gateway hold a login on the PLATFORM database; the worker holds one on
//! the CREATOR database. A run is started the way an end user starts one - an
//! ordinary HTTP request to the app through the gateway, reaching
//! `env.workflows` in a request isolate - and completes only because the
//! manager delivered a durable job back to the worker that owns the app.
//!
//! What each arm would catch:
//!
//! - If Control could still reach a creator journal, its connection would
//!   resolve the app's journal relation. It cannot, because that relation is
//!   in another server.
//! - If the worker could still read Control's catalog, its login would resolve
//!   `zeroship.apps`. It cannot, for the same reason.
//! - Each of those is paired with its own control, one variable apart: Control
//!   DOES resolve `zeroship.apps` and the worker DOES resolve its own app
//!   schema, so neither refusal can be a dead connection reading as a fence.
//! - A handle the app cloned out of `env.workflows` and kept is revoked with
//!   the app rather than outliving its retired generation.
//! - The worker is stopped the way an orchestrator stops it, and the joined
//!   shutdown must exit zero.
//!
//! Native library availability proves none of this: every seam here is a
//! process boundary, and the two zones are two servers.

use crate::workflow_fleet::Fleet;

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

thread_local! {
    /// The most recent ingress answer, so a poll that never gets what it
    /// waits for reports what it kept getting instead of only a deadline.
    static LAST: std::cell::RefCell<Option<(u16, Value)>> = const {
        std::cell::RefCell::new(None)
    };
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

/// Whether `relation` resolves on the connection `url` opens.
///
/// `to_regclass` answers for the CONNECTION, which is the whole point: it is
/// null both when the relation is absent and when the login cannot see it, and
/// either is a refusal. Each call site pairs it with a control that must
/// resolve, so a connection that resolves nothing cannot pass as a fence.
async fn resolves(url: &str, relation: &str) -> bool {
    let (client, connection) = compio_postgres::connect(url, NoTls)
        .await
        .unwrap_or_else(|error| panic!("open {url} to probe {relation}: {error}"));
    let driver = compio::runtime::spawn(connection.run());
    let found: bool = client
        .query_one("SELECT to_regclass($1) IS NOT NULL", &[&relation])
        .await
        .expect("probe a relation")
        .get(0);
    drop(client);
    driver.await.expect("probe task").expect("probe driver");
    found
}

#[compio::test]
async fn the_two_zones_run_a_workflow_without_reaching_each_other() {
    let mut fleet = Fleet::with_workflow_manager();
    let journal = format!("{}.__zeroship_workflow_runs", fleet.app_id.as_str());

    // ZONE ARM 1, with its control. Control's login resolves the platform
    // catalog and does not resolve the app's journal.
    let control_url = fleet.database.url();
    assert!(
        resolves(&control_url, "zeroship.apps").await,
        "the control-plane connection must resolve its own catalog, or the \
         refusal below is a dead connection rather than a boundary",
    );
    assert!(
        !resolves(&control_url, &journal).await,
        "Control reached a creator journal: {journal}",
    );

    // ZONE ARM 2, with its control. The worker's own login resolves its app's
    // journal and does not resolve the platform catalog.
    let worker_url = fleet.creator_role_url("zeroship_worker");
    assert!(
        resolves(&worker_url, &journal).await,
        "the worker's login must resolve the journal it owns, or the refusal \
         below is a broken login rather than a boundary",
    );
    assert!(
        !resolves(&worker_url, "zeroship.apps").await,
        "the worker reached a platform table",
    );

    // The host registers under the identity it enrolled with at boot; nothing
    // here hands it a credential and nothing here nominates a worker. Until
    // the host has taken its placement, prepared the app from this process's
    // own creator resources and published its backend, a start is refused, so
    // this poll is the readiness assertion for the whole chain.
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

    // The journal is in the CREATOR server, which is where the completed run
    // has to be for the two arms above to be about the same run.
    let (creator, connection) = compio_postgres::connect(&fleet.database.creator_url(), NoTls)
        .await
        .expect("open the creator database");
    let creator_driver = compio::runtime::spawn(connection.run());
    let runs: i64 = creator
        .query_one(
            &format!("SELECT count(*) FROM {journal} WHERE id = $1"),
            &[&run.as_str()],
        )
        .await
        .expect("read the creator journal")
        .get(0);
    assert_eq!(runs, 1, "the completed run must live in the creator server");

    // THE RETAINED HANDLE. The app clones a handle out of `env.workflows` in
    // one request and keeps it; it answers while the app is published.
    let (status, body) = ingress(
        &fleet,
        &format!("/__host/retain/HostIngressWorkflow/{run}"),
    )
    .await;
    assert_eq!(status, 200, "retaining a handle: {body}");
    let (status, body) = ingress(&fleet, "/__host/retained").await;
    assert_eq!(
        (status, &body["state"]),
        (200, &Value::String("completed".into())),
        "a retained handle must answer while its generation is published: {body}",
    );

    // Removing the app is what withdraws that generation. Control marks the
    // deletion in the platform catalog; the manager's recovery lane abandons
    // the app, the placement is released, and the worker retires the backend
    // synchronously. The retained handle must go with it.
    let (platform, connection) = compio_postgres::connect(&fleet.database.url(), NoTls)
        .await
        .expect("open the platform database");
    let platform_driver = compio::runtime::spawn(connection.run());
    platform
        .execute(
            "UPDATE zeroship.apps SET deleted_at = now() WHERE id = $1",
            &[&fleet.app_id.as_str()],
        )
        .await
        .expect("mark the app deleted the way Control does");

    let refusal = until(
        &mut fleet,
        "a retained handle outlived the app's retired generation",
        async |fleet| {
            let (status, body) = ingress(fleet, "/__host/retained").await;
            (status == 503).then(|| body["code"].as_str().unwrap_or_default().to_owned())
        },
    )
    .await;
    assert!(
        !refusal.is_empty(),
        "the revoked handle must name why it refused",
    );

    drop(creator);
    drop(platform);
    creator_driver
        .await
        .expect("creator probe task")
        .expect("creator probe driver");
    platform_driver
        .await
        .expect("platform probe task")
        .expect("platform probe driver");

    // Stopped the way an orchestrator stops it: HTTP drains, the host closes
    // its bindings and joins, and only then does the instance retire.
    let exit = fleet.terminate("worker");
    assert!(
        exit.success(),
        "a worker running a workflow host must exit cleanly on SIGTERM, got {exit}; see {}",
        fleet.logs.display()
    );
}
