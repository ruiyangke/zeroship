//! The journal-scoped hold pair taken by the service that holds the journal.
//!
//! Nothing here places an app on a worker, and the first contract asserts the
//! worker registry is empty. That is the point: a journal hold decided by the
//! workflow service has no placement for Control to verify, so a hold that
//! succeeds against an unreachable coordinator is what the trust decision
//! bought. The queue-scoped pair beside it proves the same for a different
//! scope; this one proves it for `HoldScope::for_app`, which is the scope
//! `require_journal_hold` reads.

use super::*;
use futures::future::LocalBoxFuture;
use std::{rc::Rc, time::Instant};
use zeroship_core::{
    workflow_jobs::{DeploymentId, JobId, JobOperation, JobOutcome, JobSpec},
    workflow_policy::AppPolicy,
};
use zeroship_workflow::{
    deployment_holds::ServiceHolds,
    service::{AppDeployments, maintenance::MaintenanceOptions},
};
use zeroship_workflow_manager::{
    Error as ManagerError, Queue,
    deployments::DeploymentHolds,
    policy::{PolicyObservation, PolicySource},
    recovery::Options as RecoveryOptions,
};
use zeroship_workflow_server::sweeps::{MaintenanceLane, Swept};

/// One observation, so binding installs a real lease rather than a stub.
#[derive(Debug)]
struct Source(PolicyObservation);
impl PolicySource for Source {
    fn observe<'a>(
        &'a self,
        app: &'a AppId,
    ) -> LocalBoxFuture<'a, Result<PolicyObservation, ManagerError>> {
        Box::pin(async move {
            if self.0.app_id() == app {
                Ok(self.0.clone())
            } else {
                Err(ManagerError::Denied)
            }
        })
    }
    fn revalidate(&self, observation: &PolicyObservation) -> Result<Instant, ManagerError> {
        Ok(observation.expires_at())
    }
}

/// A catalog deployment for an app the workflow fixtures already seeded.
///
/// `Fixture::deployment` mints its own app; the lane contract needs one the
/// queue and the journal also know, so the app comes from there and only the
/// catalog row is added here.
async fn catalog_deploy(fixture: &Fixture, app: &AppId) -> (String, String) {
    let deployment = typed_id::generate("dep");
    let mut manifest = zeroship_bundle::Manifest::default();
    let hash =
        zeroship_bundle::deployment_manifest_hash(&serde_json::to_vec(&manifest).unwrap()).unwrap();
    manifest.deploy_hash = Some(hash.clone());
    let manifest_json = serde_json::to_string(&manifest).unwrap();
    let inserted = fixture
        .platform
        .admin
        .execute(
            "INSERT INTO zeroship.app_deploys(id,app_id,deploy_hash,manifest_json) \
             VALUES($1,$2,$3,$4)",
            &[&deployment, &app.as_str(), &hash, &manifest_json],
        )
        .await
        .unwrap();
    assert_eq!(inserted, 1);
    (deployment, hash)
}

async fn workers(fixture: &Fixture) -> i64 {
    fixture
        .platform
        .admin
        .query_one(
            "SELECT COUNT(*)::bigint FROM zeroship.worker_instances",
            &[],
        )
        .await
        .unwrap()
        .get(0)
}

#[ntex::test]
async fn journal_holds_are_taken_by_the_service_role_without_any_placement() {
    let fixture = Fixture::new().await;
    let (app, deployment, hash) = fixture.deployment("journal-asserted").await;
    let (_, foreign_deployment, _) = fixture.deployment("journal-asserted-foreign").await;
    // No coordinator is listening: journal authority must not read a placement.
    let control_server = fixture.control("http://127.0.0.1:1/".into()).await;
    let client = RemoteDeploymentHolds::asserted(
        &origin(&control_server),
        fixture.workflow_role.clone(),
        app.clone(),
        Options::default(),
    )
    .unwrap();
    assert_eq!(workers(&fixture).await, 0);

    let first = client.acquire(&deployment, generation(1)).await.unwrap();
    assert_eq!(first.app_id, app);
    assert_eq!(first.deploy_hash, hash);
    assert_eq!(first.holder_id, HoldScope::for_app(app.clone()).holder());
    assert_eq!(first.state, HoldState::Held);
    assert_eq!(
        client.acquire(&deployment, generation(1)).await.unwrap(),
        first
    );
    let rows = fixture.rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, first.holder_id);
    assert_eq!(rows[0].3, "held");

    // What the placement check never covered, and what therefore must still
    // refuse: a deployment belonging to another app, and a stale generation.
    assert_eq!(
        client.acquire(&foreign_deployment, generation(1)).await,
        Err(WorkflowServiceError::PermissionDenied)
    );
    assert!(matches!(
        client.acquire(&deployment, generation(2)).await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    let released = client.release(&deployment, generation(1)).await.unwrap();
    assert_eq!(released.state, HoldState::Released);
    assert_eq!(released.holder_id, first.holder_id);
    let held = client.acquire(&deployment, generation(2)).await.unwrap();
    assert_eq!(held.state, HoldState::Held);
    assert_eq!(fixture.rows().await.len(), 1);
    assert_eq!(workers(&fixture).await, 0);

    // The service may not name a placement, and the worker must. The two halves
    // of one comparison, and neither is decided by the body: the principal that
    // authenticated decides which shape its body may have.
    let http = Client::new().await;
    let named = json!({
        "appId":app, "assignmentRevision":1, "deployId":deployment, "generation":2
    });
    for endpoint in [
        endpoints::CONTROL_DEPLOYMENT_HOLD_ACQUIRE,
        endpoints::CONTROL_DEPLOYMENT_HOLD_RELEASE,
    ] {
        let token = control_header(&fixture.workflow_role);
        let (status, failure) = post(
            &http,
            &origin(&control_server),
            endpoint,
            Some(&token),
            &named,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(failure, json!({"code":"invalid"}));
    }
    let (worker, worker_auth) = fixture.joined_worker(&http, &origin(&control_server)).await;
    assert_eq!(workers(&fixture).await, 1);
    let unnamed = json!({"appId":app, "deployId":deployment, "generation":2});
    for endpoint in [
        endpoints::CONTROL_DEPLOYMENT_HOLD_ACQUIRE,
        endpoints::CONTROL_DEPLOYMENT_HOLD_RELEASE,
    ] {
        let token = control_header(&worker_auth);
        let (status, failure) = post(
            &http,
            &origin(&control_server),
            endpoint,
            Some(&token),
            &unnamed,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{}", worker.as_str());
        assert_eq!(failure, json!({"code":"invalid"}));
    }
    // And the holds above are untouched by either refusal.
    let rows = fixture.rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].2, 2);
    assert_eq!(rows[0].3, "held");
}

/// The maintenance lane claims a row whose operation needs a journal hold, runs
/// it against the service's own journal, and Control records the release.
///
/// END TO END over the real journal, the real queue and the real Control
/// endpoint: the lane asserts its own authority to claim, the journal decides
/// the release under `HoldScope::for_app`, and the catalog row is the proof that
/// Control accepted the holder the journal names. The lane holds no artifact
/// store, and this operation needs none - it writes no `deploys` row.
#[ntex::test]
async fn the_lane_settles_a_hold_release_control_accepted() {
    let fixture = Fixture::new().await;
    let app = AppId::mint();
    fixture.platform.seed_app(&app).await;
    let (deployment, hash) = catalog_deploy(&fixture, &app).await;
    // The catalog hold the journal is about to give back. Seeded through the
    // ledger rather than over HTTP, so the exchange under test is the release.
    let catalog = DeploymentHolds::new(database(&fixture.control_url).await).unwrap();
    let journal_scope = HoldScope::for_app(app.clone());
    let acquired = catalog
        .acquire(&journal_scope, &deployment, generation(1))
        .await
        .unwrap();
    assert_eq!(acquired.state, HoldState::Held);

    let control_server = fixture.control("http://127.0.0.1:1/".into()).await;
    let service = Coordinator::connect(
        &fixture.platform.runtime_url,
        CoordinatorOptions::default(),
        Rc::new(zeroship_workflow_manager::retention::CatalogClient::new(
            DeploymentHolds::new(database(&fixture.control_url).await).unwrap(),
        )),
        Rc::new(
            zeroship_workflow_manager::eligibility::LocalEligibility::new(
                zeroship_workflow_manager::eligibility::ZoneId::default_zone(),
            ),
        ),
    )
    .await
    .unwrap();
    let queue: Queue = service.manager.queue().clone();
    queue.register_scope(&app).await.unwrap();
    // The journal rows the app needs to exist at all, and the held intent this
    // release gives back. The intent names the journal holder, which is what
    // Control has to accept from a caller that is not a worker.
    journal::seed_run(&fixture.platform, &app).await;
    let inserted = fixture
        .platform
        .admin
        .execute(
            "INSERT INTO workflow_manager.__zeroship_workflow_deployment_holds \
             (id,app_id,deploy_id,deploy_hash,holder_id,generation,state) \
             VALUES($1,$2,$3,$4,$5,1,'held')",
            &[
                &typed_id::generate("wjr"),
                &app.as_str(),
                &deployment,
                &hash,
                &journal_scope.holder(),
            ],
        )
        .await
        .unwrap();
    assert_eq!(inserted, 1);

    let runs = Rc::new(
        RunService::connect(
            &fixture.platform.runtime_url,
            service.recovery(RecoveryOptions::default()).unwrap(),
        )
        .await
        .unwrap()
        .with_deployments(AppDeployments::holds_only(Rc::new(ServiceHolds::new(
            origin(&control_server),
            fixture.workflow_role.clone(),
            Options::default(),
        )))),
    );
    let policies = Rc::new(Source(
        PolicyObservation::new(
            app.clone(),
            7.try_into().unwrap(),
            AppPolicy::default(),
            Instant::now() + Duration::from_secs(600),
        )
        .unwrap(),
    ));
    let lane = MaintenanceLane::new(
        queue.clone(),
        runs,
        policies as Rc<dyn PolicySource>,
        WorkerId::mint(),
        MaintenanceOptions::default(),
    )
    .unwrap();

    let release = JobSpec {
        id: JobId::mint(),
        app_id: app.clone(),
        operation: JobOperation::ReleaseHold {
            deployment_id: DeploymentId::parse(&deployment).unwrap(),
        },
        available_at: 0.try_into().unwrap(),
    };
    queue.submit(&release).await.unwrap();
    let swept = Box::pin(lane.sweep(&app)).await;
    let Ok(Swept::Settled(receipt)) = swept else {
        panic!("the lane settles a hold release: {swept:?}");
    };
    assert_eq!(receipt.job_id, release.id);
    assert_eq!(receipt.outcome, JobOutcome::Completed {});

    // Control's catalog, which only the release exchange could have moved.
    let journal_row = fixture
        .rows()
        .await
        .into_iter()
        .find(|row| row.1 == journal_scope.holder())
        .expect("the journal holder's catalog row");
    assert_eq!(journal_row.2, 1);
    assert_eq!(journal_row.3, "released");
    // And the journal's own intent, acknowledged by that receipt.
    let intent: String = fixture
        .platform
        .admin
        .query_one(
            "SELECT state FROM workflow_manager.__zeroship_workflow_deployment_holds \
             WHERE app_id=$1 AND deploy_id=$2",
            &[&app.as_str(), &deployment],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(intent, "released");
    assert_eq!(
        workers(&fixture).await,
        0,
        "no worker exists, so no placement authorized this hold"
    );
}
