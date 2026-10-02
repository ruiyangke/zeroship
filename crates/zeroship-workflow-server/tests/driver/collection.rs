//! Collection duty survives process loss without creator access or executable holds.

use super::{no_workers, platform, private_schema, ready, server_process, until};
use ntex::client::Client;
use serde_json::{json, Value};
use std::{future::Future, path::PathBuf, pin::Pin, rc::Rc};
use zeroship_core::{
    app_id::AppId,
    schema_name::SchemaName,
    service_assertion::ServiceSigningKey,
    service_peers::{service_issuer, CONTROL_SERVICE_NAME},
    workflow_coordination::WorkerId,
    workflow_deployments::{HoldGeneration, HoldReceipt},
    workflow_jobs::{
        Delivery, DeploymentId, JobId, JobOperation, JobOutcome, JobReceipt, JobSpec,
        JournalSettlement, SettlementReceipt,
    },
    workflow_policy::AppPolicy,
};
use zeroship_data_orm::binding::DbBinding;
use zeroship_workflow_manager::{
    maintenance::MaintenanceAuthority,
    recovery::{DutyKind, Options as RecoveryOptions, Recovery},
    retention::HoldClient,
    Error, Options, Queue,
};

#[derive(Debug)]
struct NoHolds;

impl HoldClient for NoHolds {
    fn acquire<'a>(
        &'a self,
        _app: &'a AppId,
        _deployment: &'a DeploymentId,
        _generation: HoldGeneration,
    ) -> Pin<Box<dyn Future<Output = Result<HoldReceipt, Error>> + 'a>> {
        panic!("maintenance must not acquire executable retention")
    }

    fn release<'a>(
        &'a self,
        _app: &'a AppId,
        _deployment: &'a DeploymentId,
        _generation: HoldGeneration,
    ) -> Pin<Box<dyn Future<Output = Result<HoldReceipt, Error>> + 'a>> {
        panic!("maintenance must not release executable retention")
    }
}

/// The Control peer the spawned service trusts. Nothing in this case signs a
/// request with it; the service refuses to start without a peer to trust.
fn control(platform: &platform::Platform) -> PathBuf {
    let issuer = service_issuer(CONTROL_SERVICE_NAME).unwrap();
    let key = ServiceSigningKey::generate();
    let peers = platform.work.path().join("collection-peers.json");
    platform::write_private(
        &peers,
        serde_json::to_vec(&json!({"keys":[{
            "iss":issuer.as_str(),"x":key.public_jwk_x()
        }]}))
        .unwrap(),
    );
    peers
}

/// Take the next sweep off this app's queue, in process, under the authority the
/// service's own maintenance lane asserts.
///
/// THERE IS NO WIRE CLAIM FOR A SWEEP. `WORKFLOW_JOB_CLAIM` claims as
/// `Claimant::Placed` (`Coordinator::claim_job`), and that claimant admits
/// `advance` alone, so reconciliation and collection are the lane's rows. The
/// lane is driven here, rather than left running in the service, so that this
/// case is the claimant of every page whose settlement it measures.
async fn claim(queue: &Queue, lane: &MaintenanceAuthority) -> Delivery {
    let delivery = lane
        .claim(queue, Ok(AppPolicy::default().max_delivery_attempts))
        .await
        .unwrap()
        .expect("the queue holds a sweep for the lane to claim")
        .delivery()
        .clone();
    assert_eq!(&delivery.job.app_id, lane.app());
    delivery
}

/// Discharge a sweep the way its lane does, in process: a sweep's outcome
/// reaches the queue from the process that committed it, never over a worker's
/// settlement.
async fn settle(
    queue: &Queue,
    lane: &MaintenanceAuthority,
    command: &JournalSettlement,
) -> SettlementReceipt {
    let receipt = lane.settle(queue, command).await.unwrap();
    assert_eq!(receipt.job_id, command.delivery().job.id);
    assert_eq!(receipt.app_id, command.delivery().job.app_id);
    assert_eq!(receipt.attempt, command.delivery().attempt);
    assert_eq!(receipt.outcome, *command.outcome());
    receipt
}

fn completed(delivery: Delivery) -> JournalSettlement {
    JournalSettlement::from_receipt(
        &JobReceipt {
            job: delivery.job.clone(),
            outcome: JobOutcome::Completed {},
        },
        &delivery,
    )
    .unwrap()
}

/// The same, for a page that keeps the obligation open.
fn waiting(delivery: Delivery) -> JournalSettlement {
    JournalSettlement::from_receipt(
        &JobReceipt {
            job: delivery.job.clone(),
            outcome: JobOutcome::Waiting {},
        },
        &delivery,
    )
    .unwrap()
}

async fn duty(platform: &platform::Platform, app: &AppId, kind: &str) -> Value {
    let row = platform
        .admin
        .query_one(
            "SELECT to_jsonb(d)::text FROM workflow_manager.recovery_duties d \
             WHERE app_id=$1 AND kind=$2",
            &[&app.as_str(), &kind],
        )
        .await
        .unwrap();
    serde_json::from_str(row.get::<_, &str>(0)).unwrap()
}

async fn pending_collect(
    platform: &platform::Platform,
    app: &AppId,
    previous: Option<&JobId>,
) -> JobId {
    until("publish the next collection page", async || {
        let row = duty(platform, app, "collect").await;
        let pending = row["pending_job_id"].as_str()?;
        let id = JobId::parse(pending).unwrap();
        (Some(&id) != previous).then_some(id)
    })
    .await
}

async fn no_holds(platform: &platform::Platform, app: &AppId) {
    let row = platform
        .admin
        .query_one(
            "SELECT (SELECT count(*) FROM workflow_manager.deployment_holds WHERE app_id=$1), \
                (SELECT count(*) FROM zeroship.app_deploy_holds WHERE app_id=$1)",
            &[&app.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), 0);
    assert_eq!(row.get::<_, i64>(1), 0);
}

async fn setup(platform: &platform::Platform) -> (Queue, Recovery, JobSpec) {
    let queue = Queue::connect(
        DbBinding::platform(
            "workflow_manager",
            "collection-driver",
            SchemaName::new("workflow_manager").unwrap(),
        ),
        &platform.runtime_url,
        Options::default(),
        Rc::new(NoHolds),
    )
    .await
    .unwrap();
    let recovery = Recovery::new(queue.clone(), RecoveryOptions::default()).unwrap();
    let app = AppId::mint();
    recovery
        .ensure(&app, &DeploymentId::mint(), 1.try_into().unwrap())
        .await
        .unwrap();
    let reconcile = recovery
        .dispatch(&app, DutyKind::Reconcile)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reconcile.operation, JobOperation::Reconcile {});
    assert_eq!(
        platform
            .admin
            .execute(
                "UPDATE workflow_manager.jobs SET operation='{' WHERE app_id=$1 AND id=$2",
                &[&app.as_str(), &reconcile.id.as_str()],
            )
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        platform
            .admin
            .execute(
                "UPDATE workflow_manager.recovery_duties SET next_due_at=0 WHERE app_id=$1",
                &[&app.as_str()],
            )
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        recovery.dispatch(&app, DutyKind::Reconcile).await,
        Err(Error::Storage)
    );
    (queue, recovery, reconcile)
}

#[ntex::test]
async fn collection_is_independent_without_workers_and_continues_from_exact_settlements() {
    let platform = platform::Platform::new().await;
    private_schema(&platform).await;
    Box::pin(collection_contract(&platform)).await;
}

async fn collection_contract(platform: &platform::Platform) {
    let (queue, recovery, reconcile) = Box::pin(setup(platform)).await;
    let app = &reconcile.app_id;
    let broken = duty(platform, app, "reconcile").await;
    let peers = control(platform);
    let http = Client::new().await;
    no_workers(platform).await;
    no_holds(platform, app).await;
    // No sweep lane on this host. What the case measures is the collect duty --
    // that it is published with no worker in the deployment, survives process
    // loss, and continues from the EXACT settlement each page reported -- and
    // every one of those comes from the manager driver's recovery lane, which
    // stays on and reacts to the settlements this case makes. The sweep lane
    // only claims rows the driver has already published, so a running one would
    // take the very page this case has to be the claimant of and leave the
    // exactness unobservable.
    let mut server = server_process::ServerProcess::without_maintenance_sweeps(
        &platform.runtime_url,
        &peers,
        platform.work.path(),
        "collection",
        &http,
    )
    .await;
    let first = pending_collect(platform, app, None).await;
    assert_ne!(first, reconcile.id);
    assert_eq!(duty(platform, app, "reconcile").await, broken);
    assert_eq!(
        recovery.dispatch(app, DutyKind::Reconcile).await,
        Err(Error::Storage)
    );
    no_workers(platform).await;
    no_holds(platform, app).await;
    let recorded = duty(platform, app, "collect").await;
    let jobs = super::jobs(platform, app).await;
    server.restart(&http).await;
    assert_eq!(duty(platform, app, "collect").await, recorded);
    assert_eq!(duty(platform, app, "reconcile").await, broken);
    assert_eq!(super::jobs(platform, app).await, jobs);
    ready(&http, &server.url).await;
    no_workers(platform).await;
    no_holds(platform, app).await;

    Box::pin(settlement_contract(platform, &queue, &reconcile, &first)).await;
    no_holds(platform, app).await;
    ready(&http, &server.url).await;
    assert_eq!(
        platform
            .admin
            .query_one(
                "SELECT secret FROM driver_customer.__zeroship_workflow_history",
                &[],
            )
            .await
            .unwrap()
            .get::<_, &str>(0),
        "private history"
    );
}

async fn settlement_contract(
    platform: &platform::Platform,
    queue: &Queue,
    reconcile: &JobSpec,
    first: &JobId,
) {
    let app = &reconcile.app_id;
    // Repair metadata before executing either queue job. A broken duty cannot
    // suppress another duty, but damaged jobs remain refused by queue readers.
    assert_eq!(
        platform
            .admin
            .execute(
                "UPDATE workflow_manager.jobs SET operation=$3 WHERE app_id=$1 AND id=$2",
                &[
                    &app.as_str(),
                    &reconcile.id.as_str(),
                    &serde_json::to_string(&reconcile.operation).unwrap()
                ],
            )
            .await
            .unwrap(),
        1
    );
    let lane = MaintenanceAuthority::new(app.clone(), WorkerId::mint());
    let delivery = claim(queue, &lane).await;
    assert_eq!(&delivery.job, reconcile);
    settle(queue, &lane, &completed(delivery)).await;
    let delivery = claim(queue, &lane).await;
    assert_eq!(&delivery.job.id, first);
    assert_eq!(delivery.job.operation, JobOperation::Collect {});
    assert_eq!(delivery.job.deployment_id(), None);
    let original = waiting(delivery);
    defer_collection(platform, app).await;
    let receipt = settle(queue, &lane, &original).await;
    let next = pending_collect(platform, app, Some(first)).await;
    let before_replay = duty(platform, app, "collect").await;
    assert_eq!(settle(queue, &lane, &original).await, receipt);
    assert_eq!(duty(platform, app, "collect").await, before_replay);

    // Reconciliation may have become due while the lane acknowledged its
    // earlier job. Leave that delivery occupied while selecting collection.
    let delivery = claim(queue, &lane).await;
    let delivery = if matches!(delivery.job.operation, JobOperation::Reconcile {}) {
        claim(queue, &lane).await
    } else {
        delivery
    };
    assert_eq!(delivery.job.id, next);
    defer_collection(platform, app).await;
    let before_completion = duty(platform, app, "collect").await;
    settle(queue, &lane, &completed(delivery)).await;
    let completed = duty(platform, app, "collect").await;
    assert_eq!(completed, before_completion);
    assert_eq!(completed["pending_job_id"], next.as_str());
    assert_eq!(platform.admin.execute(
        "UPDATE workflow_manager.recovery_duties SET next_due_at=0 WHERE app_id=$1 AND kind='collect'",
        &[&app.as_str()],
    ).await.unwrap(), 1);
    let periodic = pending_collect(platform, app, Some(&next)).await;
    assert_ne!(&periodic, first);
    assert_eq!(settle(queue, &lane, &original).await, receipt);
    assert_eq!(
        duty(platform, app, "collect").await["pending_job_id"],
        periodic.as_str()
    );
}

async fn defer_collection(platform: &platform::Platform, app: &AppId) {
    assert_eq!(platform.admin.execute(
        "UPDATE workflow_manager.recovery_duties SET next_due_at=$2 WHERE app_id=$1 AND kind='collect'",
        &[&app.as_str(), &i64::MAX],
    ).await.unwrap(), 1);
}
