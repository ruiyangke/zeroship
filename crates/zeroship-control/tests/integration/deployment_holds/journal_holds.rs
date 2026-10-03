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
use zeroship_storage::StorageBackendConfig;
use zeroship_workflow::{
    deploy_registrations::{DeployRegistrationSource, RemoteDeployRegistrations},
    deployment_holds::ServiceHolds,
    service::{AppDeployments, maintenance::MaintenanceOptions},
};
use zeroship_workflow_manager::{
    Error as ManagerError, Queue,
    deployments::DeploymentHolds,
    policy::{PolicyObservation, PolicySource},
    recovery::Options as RecoveryOptions,
};
use zeroship_workflow_server::{
    payloads::ServicePayloads,
    sweeps::{MaintenanceLane, Swept},
};

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
    catalog_deploy_declaring(fixture, app, &[]).await
}

/// The same catalog row for a bundle declaring `workflows`.
///
/// The declarations go in the MANIFEST rather than into a column of their own,
/// because that is what Control stores and what its registration endpoint
/// re-derives from. A test that seeded a summary directly would be asserting
/// against its own arrangement.
async fn catalog_deploy_declaring(
    fixture: &Fixture,
    app: &AppId,
    workflows: &[&str],
) -> (String, String) {
    let deployment = typed_id::generate("dep");
    let mut manifest = zeroship_bundle::Manifest::default();
    if !workflows.is_empty() {
        manifest.workflows = Some(serde_json::json!(workflows));
    }
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

#[compio::test(crate = "crate::support::live::system")]
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
#[compio::test(crate = "crate::support::live::system")]
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
    // The store the lane holds. The release this case drives touches no object,
    // so the root stays empty; a lane without one could not be composed at all.
    let objects = tempfile::tempdir().unwrap();
    let lane = MaintenanceLane::new(
        queue.clone(),
        runs,
        policies as Rc<dyn PolicySource>,
        WorkerId::mint(),
        ServicePayloads::open(&StorageBackendConfig::Local(objects.path().to_owned())).unwrap(),
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

/// One delivery, so the sweep runs against a real lease rather than a stub.
struct Delivered(zeroship_core::workflow_jobs::Delivery, Instant);
impl zeroship_core::workflow_jobs::JobLease for Delivered {
    fn delivery(&self) -> &zeroship_core::workflow_jobs::Delivery {
        &self.0
    }
    fn remaining(&self) -> Option<Duration> {
        self.1
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
    }
}

fn activation(app: &AppId, deployment: &str) -> Delivered {
    Delivered(
        zeroship_core::workflow_jobs::Delivery {
            job: JobSpec {
                id: JobId::mint(),
                app_id: app.clone(),
                operation: JobOperation::Activate {
                    deployment_id: DeploymentId::parse(deployment).unwrap(),
                    revision: 1.try_into().unwrap(),
                },
                available_at: 0.try_into().unwrap(),
            },
            worker_id: WorkerId::mint(),
            assignment_revision: 1.try_into().unwrap(),
            attempt: 1.try_into().unwrap(),
            deadline: 0.try_into().unwrap(),
        },
        Instant::now() + Duration::from_secs(30),
    )
}

/// The activation sweep records its journal row from Control's ASSERTED
/// registration, with no blob store anywhere in the process.
///
/// END TO END over the real journal and the real Control endpoint: the workflow
/// role's own signer, the real `RemoteDeployRegistrations` client, Control's
/// handler, and the deployment catalog's stored manifest. `activate_job` writes
/// the FIRST `deploys` row, so it has no earlier row to verify against and the
/// declarations have to come from somewhere; what the journal ends up holding is
/// the proof of where.
///
/// The control is the same service one capability short: holds alone, which must
/// refuse by name rather than activate from nothing. The lane is deliberately
/// not used - its queue requires a manager-minted activation row, which is a
/// dispatch precondition rather than anything this exchange decides.
#[compio::test(crate = "crate::support::live::system")]
async fn the_activation_sweep_records_controls_asserted_registration() {
    let fixture = Fixture::new().await;
    let app = AppId::mint();
    fixture.platform.seed_app(&app).await;
    let (deployment, hash) = catalog_deploy_declaring(&fixture, &app, &["Example"]).await;
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
    journal::seed_run(&fixture.platform, &app).await;

    let holds = || {
        AppDeployments::holds_only(Rc::new(ServiceHolds::new(
            origin(&control_server),
            fixture.workflow_role.clone(),
            Options::default(),
        )))
    };
    let asserted = holds().with_registrations(Rc::new(
        RemoteDeployRegistrations::asserted(
            &origin(&control_server),
            fixture.workflow_role.clone(),
            Options::default(),
        )
        .unwrap(),
    ));
    let policies = Rc::new(Source(
        PolicyObservation::new(
            app.clone(),
            7.try_into().unwrap(),
            AppPolicy::default(),
            Instant::now() + Duration::from_secs(600),
        )
        .unwrap(),
    )) as Rc<dyn PolicySource>;
    let runs = |deployments: AppDeployments| {
        let runtime_url = fixture.platform.runtime_url.clone();
        let recovery = service.recovery(RecoveryOptions::default()).unwrap();
        async move {
            RunService::connect(&runtime_url, recovery)
                .await
                .unwrap()
                .with_deployments(deployments)
        }
    };

    // The control arm: retention authority and nothing else.
    let uncapable = runs(holds()).await;
    let refused = Box::pin(
        uncapable
            .app(policies.as_ref(), &app)
            .await
            .unwrap()
            .activate_job(&activation(&app, &deployment)),
    )
    .await
    .unwrap_err();
    let WorkflowServiceError::Unavailable(message) = &refused else {
        panic!("a host with no registration source refuses activation: {refused:?}");
    };
    assert!(
        message.contains("artifact store") && message.contains("registration source"),
        "the refusal names the missing capability: {message}"
    );
    let unactivated: i64 = fixture
        .platform
        .admin
        .query_one(
            "SELECT COUNT(*)::bigint FROM workflow_manager.__zeroship_workflow_deploys \
             WHERE app_id=$1 AND id=$2",
            &[&app.as_str(), &deployment],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(unactivated, 0, "the refused sweep wrote no deploys row");

    let capable = runs(asserted).await;
    let grant = activation(&app, &deployment);
    let receipt = Box::pin(
        capable
            .app(policies.as_ref(), &app)
            .await
            .unwrap()
            .activate_job(&grant),
    )
    .await
    .unwrap();
    assert_eq!(receipt.job, grant.0.job);
    assert_eq!(receipt.outcome, JobOutcome::Completed {});

    // The journal row, which only Control's assertion could have filled in: this
    // process holds no blob store, and the declared workflow is inside the
    // manifest Control parsed at publish.
    let row = fixture
        .platform
        .admin
        .query_one(
            "SELECT hash,manifest,state,active FROM workflow_manager.__zeroship_workflow_deploys \
             WHERE app_id=$1 AND id=$2",
            &[&app.as_str(), &deployment],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>("hash"), hash);
    assert_eq!(row.get::<_, String>("state"), "available");
    assert_eq!(row.get::<_, i64>("active"), 1);
    assert_eq!(
        serde_json::from_str::<Value>(&row.get::<_, String>("manifest")).unwrap(),
        json!({"id":deployment, "hash":hash, "workflows":["Example"], "schedules":[]})
    );
    // And Control's own catalog holds the journal-scoped hold the sweep took,
    // which is the other half of what this process may do without artifacts.
    let held = fixture
        .rows()
        .await
        .into_iter()
        .find(|row| row.1 == HoldScope::for_app(app.clone()).holder())
        .expect("the activation took a journal hold");
    assert_eq!(held.3, "held");
    assert_eq!(
        workers(&fixture).await,
        0,
        "no worker exists, so no placement authorized this activation"
    );
}

/// The endpoint answers the workflow ROLE about the app's own deployments, and
/// nothing else.
///
/// Three refusals, each differing from the accepted call in one variable: the
/// credential, the app the deployment belongs to, and the deployment's
/// existence. Without them a green above would only be saying the route exists.
#[compio::test(crate = "crate::support::live::system")]
async fn the_registration_endpoint_answers_only_the_workflow_role_about_its_own_app() {
    let fixture = Fixture::new().await;
    let app = AppId::mint();
    let other = AppId::mint();
    fixture.platform.seed_app(&app).await;
    fixture.platform.seed_app(&other).await;
    let (deployment, hash) = catalog_deploy_declaring(&fixture, &app, &["Example"]).await;
    let control_server = fixture.control("http://127.0.0.1:1/".into()).await;

    let client = RemoteDeployRegistrations::asserted(
        &origin(&control_server),
        fixture.workflow_role.clone(),
        Options::default(),
    )
    .unwrap();
    let id = DeploymentId::parse(&deployment).unwrap();
    let answered = client.registration(&app, &id).await.unwrap();
    assert_eq!(answered.id, deployment);
    assert_eq!(answered.hash, hash);
    assert_eq!(answered.workflows, ["Example".to_owned()].into());
    assert!(answered.schedules.is_empty());

    // Another app's deployment, and one no catalog row names.
    assert_eq!(
        client.registration(&other, &id).await,
        Err(WorkflowServiceError::PermissionDenied)
    );
    assert_eq!(
        client
            .registration(&app, &DeploymentId::mint())
            .await
            .unwrap_err(),
        WorkflowServiceError::PermissionDenied
    );

    // And a trusted platform role holding no grant for this operation. The
    // constructor refuses a non-workflow signer outright, so the refusal is
    // exercised over the wire the way a forged caller would reach it.
    let http = Client::new().await;
    let token = control_header(&fixture.gateway_role);
    let (status, failure) = post(
        &http,
        &origin(&control_server),
        endpoints::CONTROL_DEPLOY_REGISTRATION,
        Some(&token),
        &json!({"appId":app, "deployId":deployment}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(failure, json!({"code":"unauthenticated"}));
}
