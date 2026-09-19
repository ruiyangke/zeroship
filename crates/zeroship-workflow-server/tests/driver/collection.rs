//! Collection duty survives process loss without creator access or executable holds.

use super::{no_workers, platform, policy_fixture, private_schema, ready, server_process, until};
use ntex::{client::Client, http::StatusCode};
use serde::Serialize;
use serde_json::{json, Value};
use std::{future::Future, path::PathBuf, pin::Pin, rc::Rc};
use zeroship_core::{
    app_id::AppId,
    schema_name::SchemaName,
    service_assertion::{ServiceAssertionMinter, ServiceIssuer, ServiceSigningKey},
    service_identity::{endpoints, ServiceEndpoint},
    service_peers::{service_issuer, CONTROL_SERVICE_NAME},
    workflow_coordination::{AssignedScope, WorkerId, AUDIENCE},
    workflow_deployments::{HoldGeneration, HoldReceipt},
    workflow_jobs::{
        Delivery, DeliveryLease, DeploymentId, JobId, JobOperation, JobOutcome, JobSpec,
        Settlement, SettlementReceipt,
    },
    workflow_policy::AppPolicy,
};
use zeroship_data_orm::binding::DbBinding;
use zeroship_workflow_manager::{
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

struct Actor {
    issuer: ServiceIssuer,
    key: ServiceSigningKey,
}

impl Actor {
    fn token(&self) -> String {
        format!(
            "Bearer {}",
            ServiceAssertionMinter::new(self.issuer.clone(), self.key.key_id(), &self.key)
                .unwrap()
                .mint(&ServiceIssuer::parse(AUDIENCE).unwrap())
                .unwrap()
        )
    }
}

fn control(platform: &platform::Platform) -> (Actor, PathBuf) {
    let actor = Actor {
        issuer: service_issuer(CONTROL_SERVICE_NAME).unwrap(),
        key: ServiceSigningKey::generate(),
    };
    let peers = platform.work.path().join("collection-peers.json");
    platform::write_private(
        &peers,
        serde_json::to_vec(&json!({"keys":[{
            "iss":actor.issuer.as_str(),"x":actor.key.public_jwk_x()
        }]}))
        .unwrap(),
    );
    (actor, peers)
}

async fn post<T: Serialize>(
    http: &Client,
    url: &str,
    actor: &Actor,
    endpoint: ServiceEndpoint,
    body: &T,
) -> Value {
    let response = http
        .post(format!("{url}{}", endpoint.path_template()))
        .header("authorization", actor.token())
        .send_json(body)
        .await
        .unwrap();
    let status = response.status();
    let body = response.body().await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

async fn enroll(
    platform: &platform::Platform,
    http: &Client,
    url: &str,
    _control: &Actor,
    app: &AppId,
) -> (Actor, AssignedScope) {
    let worker = WorkerId::mint();
    let actor = Actor {
        issuer: ServiceIssuer::parse(&format!(
            "spiffe://zeroship.ai/svc/worker/{}",
            worker.as_str()
        ))
        .unwrap(),
        key: ServiceSigningKey::generate(),
    };
    platform.admin.execute(
        "INSERT INTO zeroship.worker_instances(id,ring_key,public_key,advertise_host,advertise_port,status,join_signer_id,join_token_id,execution_zone_id,expires_at) \
         VALUES($1,$2,$3,'127.0.0.1',8080,'active',$4,'tok_testfixturedefault','ezn_default000000000000000000',now() + interval '1 hour')",
        &[&worker.as_str(), &vec![1_u8], &actor.key.verifying_key_bytes().to_vec(), &platform.default_join_signer_id],
    ).await.unwrap();
    policy_fixture::provision(platform, app, &AppPolicy::default()).await;
    post(
        http,
        url,
        &actor,
        endpoints::WORKFLOW_REGISTER,
        &json!({"capacity":1,"state":"ready"}),
    )
    .await;
    let assignment = platform
        .seed_placement(app, &worker, std::time::Duration::from_secs(30))
        .await;
    (
        actor,
        AssignedScope {
            app_id: app.clone(),
            assignment_revision: assignment.revision,
        },
    )
}

async fn claim(http: &Client, url: &str, actor: &Actor, scope: &AssignedScope) -> Delivery {
    let lease: DeliveryLease =
        serde_json::from_value(post(http, url, actor, endpoints::WORKFLOW_JOB_CLAIM, scope).await)
            .unwrap();
    assert_eq!(lease.delivery.job.app_id, scope.app_id);
    assert_eq!(
        lease.delivery.assignment_revision,
        scope.assignment_revision
    );
    lease.delivery
}

async fn settle(
    http: &Client,
    url: &str,
    actor: &Actor,
    command: &Settlement,
) -> SettlementReceipt {
    let receipt: SettlementReceipt = serde_json::from_value(
        post(http, url, actor, endpoints::WORKFLOW_JOB_SETTLE, command).await,
    )
    .unwrap();
    assert_eq!(receipt.job_id, command.delivery.job.id);
    assert_eq!(receipt.app_id, command.delivery.job.app_id);
    assert_eq!(receipt.attempt, command.delivery.attempt);
    assert_eq!(receipt.outcome, command.outcome);
    receipt
}

const fn completed(delivery: Delivery) -> Settlement {
    Settlement {
        delivery,
        outcome: JobOutcome::Completed {},
        successors: Vec::new(),
    }
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

async fn setup(platform: &platform::Platform) -> (Recovery, JobSpec) {
    let queue = Queue::connect(
        DbBinding::new(
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
    let recovery = Recovery::new(queue, RecoveryOptions::default()).unwrap();
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
    (recovery, reconcile)
}

#[ntex::test]
async fn collection_is_independent_without_workers_and_continues_from_exact_settlements() {
    let platform = platform::Platform::new().await;
    private_schema(&platform).await;
    Box::pin(collection_contract(&platform)).await;
}

async fn collection_contract(platform: &platform::Platform) {
    let (recovery, reconcile) = Box::pin(setup(platform)).await;
    let app = &reconcile.app_id;
    let broken = duty(platform, app, "reconcile").await;
    let (control, peers) = control(platform);
    let http = Client::new().await;
    no_workers(platform).await;
    no_holds(platform, app).await;
    let mut server = server_process::ServerProcess::start(
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

    Box::pin(settlement_contract(
        platform,
        &http,
        &server.url,
        &control,
        &reconcile,
        &first,
    ))
    .await;
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
    http: &Client,
    url: &str,
    control: &Actor,
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
    let (worker, scope) = enroll(platform, http, url, control, app).await;
    let delivery = claim(http, url, &worker, &scope).await;
    assert_eq!(&delivery.job, reconcile);
    settle(http, url, &worker, &completed(delivery)).await;
    let delivery = claim(http, url, &worker, &scope).await;
    assert_eq!(&delivery.job.id, first);
    assert_eq!(delivery.job.operation, JobOperation::Collect {});
    assert_eq!(delivery.job.deployment_id(), None);
    let original = Settlement {
        delivery,
        outcome: JobOutcome::Waiting {},
        successors: Vec::new(),
    };
    defer_collection(platform, app).await;
    let receipt = settle(http, url, &worker, &original).await;
    let next = pending_collect(platform, app, Some(first)).await;
    let before_replay = duty(platform, app, "collect").await;
    assert_eq!(settle(http, url, &worker, &original).await, receipt);
    assert_eq!(duty(platform, app, "collect").await, before_replay);

    // Reconciliation may have become due while the worker acknowledged its
    // earlier job. Leave that delivery occupied while selecting collection.
    let delivery = claim(http, url, &worker, &scope).await;
    let delivery = if matches!(delivery.job.operation, JobOperation::Reconcile {}) {
        claim(http, url, &worker, &scope).await
    } else {
        delivery
    };
    assert_eq!(delivery.job.id, next);
    defer_collection(platform, app).await;
    let before_completion = duty(platform, app, "collect").await;
    settle(http, url, &worker, &completed(delivery)).await;
    let completed = duty(platform, app, "collect").await;
    assert_eq!(completed, before_completion);
    assert_eq!(completed["pending_job_id"], next.as_str());
    assert_eq!(platform.admin.execute(
        "UPDATE workflow_manager.recovery_duties SET next_due_at=0 WHERE app_id=$1 AND kind='collect'",
        &[&app.as_str()],
    ).await.unwrap(), 1);
    let periodic = pending_collect(platform, app, Some(&next)).await;
    assert_ne!(&periodic, first);
    assert_eq!(settle(http, url, &worker, &original).await, receipt);
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
