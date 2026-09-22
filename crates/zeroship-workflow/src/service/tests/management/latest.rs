#![expect(
    clippy::future_not_send,
    reason = "Latest delivery tests own compio artifact and journal I/O"
)]

use super::atomic_application::persisted;
use super::*;
use crate::service::{delivery::ATTEMPT_IO_CEILING, AppDeployments};
use zeroship_core::workflow_jobs::{JobLease, JobOutcome};
use zeroship_data_orm::Value;

mod artifacts;
use artifacts::Artifacts;

macro_rules! case {
    ($sqlite:ident, $postgres:ident, $contract:ident) => {
        #[compio::test]
        async fn $sqlite() {
            let directory = tempfile::tempdir().unwrap();
            Box::pin($contract(Rc::new(
                sqlite_store(&directory.path().join("workflow.sqlite")).await,
            )))
            .await;
        }
        #[compio::test]
        async fn $postgres() {
            let fixture = PostgresFixture::start().await;
            Box::pin($contract(Rc::new(fixture.store.clone()))).await;
        }
    };
}

case!(
    sqlite_management_latest_keeps_explicit_target_across_artifact_wait,
    postgres_management_latest_keeps_explicit_target_across_artifact_wait,
    exact_target
);
case!(
    sqlite_management_latest_refuses_missing_corrupt_bundle_then_retries,
    postgres_management_latest_refuses_missing_corrupt_bundle_then_retries,
    missing_bundle
);
case!(
    sqlite_management_latest_prepares_lifecycle_before_deployment_io,
    postgres_management_latest_prepares_lifecycle_before_deployment_io,
    preliminary
);
case!(
    sqlite_management_latest_fences_policy_and_original_hold_generation,
    postgres_management_latest_fences_policy_and_original_hold_generation,
    fences
);
case!(
    sqlite_management_latest_ends_a_stalled_attempt_at_the_io_ceiling,
    postgres_management_latest_ends_a_stalled_attempt_at_the_io_ceiling,
    io_ceiling
);

fn gated(
    service: WorkflowService,
    platform: &Deployments,
    app: &AppId,
) -> (WorkflowService, Arc<Artifacts>) {
    let source = Arc::new(Artifacts::new(platform.source.clone()));
    let service = service.with_deployments(
        AppDeployments::new(source.clone(), 1024 * 1024)
            .unwrap()
            .with_hold_client(Rc::new(platform.client(app))),
    );
    (service, source)
}

async fn assert_deployment(
    service: &WorkflowService,
    app: &AppId,
    run: &str,
    deployment: &DeployRegistration,
) {
    let mut tx = service.begin().await.unwrap();
    let row = app::lock_run(&mut tx, app, run).await.unwrap();
    assert_eq!(row.integer("generation").unwrap(), 1);
    assert_eq!(row.text("deploy_id").unwrap(), deployment.id);
    tx.commit().await.unwrap();
}

async fn exact_target(store: Rc<OrmStore>) {
    let (service, app_id, _, platform) = registered_service(store.clone()).await;
    let run = start(&service, &app_id).await;
    let target = platform.deploy(&app_id).await;
    let newer = platform.deploy(&app_id).await;
    let command = fixture::latest(&app_id, &run, 1, &target);
    let (service, source) = gated(service, &platform, &app_id);
    let scope = service.fixture_app(app_id.clone());
    let gate = source.block();
    let (result, ()) = futures::join!(scope.management_job(&command), async {
        gate.entered.recv_async().await.unwrap();
        service.activate_deploy(&app_id, &newer).await.unwrap();
        gate.resume.send_async(()).await.unwrap();
    });
    let original = result.unwrap();
    assert_eq!(
        original.outcome,
        JobOutcome::Management {
            outcome: restarted(&target.id)
        }
    );
    assert_deployment(&service, &app_id, &run, &target).await;
    let mut tx = service.begin().await.unwrap();
    assert_eq!(
        app::active_deploy(&mut tx, &app_id).await.unwrap().id,
        newer.id
    );
    tx.commit().await.unwrap();
    platform.assert_held(&app_id, &target.id).await;
    platform
        .source
        .delete_manifest(&app_id, &target.hash)
        .await
        .unwrap();
    let reopened = WorkflowService::open(store, Arc::new(HostPolicies::default()))
        .await
        .unwrap();
    let mut expired = command.retry();
    expired.expires = Instant::now();
    assert_eq!(
        reopened
            .fixture_app(app_id)
            .management_job(&expired)
            .await
            .unwrap(),
        original
    );
}

async fn missing_bundle(store: Rc<OrmStore>) {
    let (service, app_id, _, platform) = registered_service(store).await;
    let run = start(&service, &app_id).await;
    let target = platform.deploy(&app_id).await;
    let scope = service.fixture_app(app_id.clone());
    let command = fixture::latest(&app_id, &run, 1, &target);
    let manifest = platform
        .source
        .get_manifest(&app_id, &target.hash)
        .await
        .unwrap();
    let before = persisted(&service, &app_id).await;
    platform
        .source
        .delete_manifest(&app_id, &target.hash)
        .await
        .unwrap();
    assert!(scope.management_job(&command).await.is_err());
    assert_eq!(persisted(&service, &app_id).await, before);
    platform
        .source
        .put_manifest(&app_id, &target.hash, b"{}")
        .await
        .unwrap();
    assert!(scope.management_job(&command.retry()).await.is_err());
    assert_eq!(persisted(&service, &app_id).await, before);
    platform
        .source
        .put_manifest(&app_id, &target.hash, &manifest)
        .await
        .unwrap();
    scope.management_job(&command.retry()).await.unwrap();
    assert_deployment(&service, &app_id, &run, &target).await;
}

async fn preliminary(store: Rc<OrmStore>) {
    let (service, app_id, _, platform) = registered_service(store.clone()).await;
    let run = start(&service, &app_id).await;
    let missing_target = platform.deploy(&app_id).await;
    let journal = WorkflowService::open(store, service.policies.clone())
        .await
        .unwrap();
    let scope = journal.fixture_app(app_id.clone());
    let missing = fixture::latest(&app_id, RunId::mint().as_str(), 1, &missing_target);
    assert_eq!(
        scope.management_outcome(&missing).await.unwrap(),
        ManagementOutcome::NotFound {}
    );
    service
        .policies
        .fixture_install(
            &app_id,
            leased_policy(
                2,
                AppPolicy {
                    admission: false,
                    ..Default::default()
                },
            ),
        )
        .unwrap();
    let denied = fixture::latest(&app_id, &run, 1, &missing_target);
    assert_eq!(
        scope.management_outcome(&denied).await.unwrap(),
        ManagementOutcome::Denied {}
    );
    service
        .policies
        .fixture_install(&app_id, leased_policy(3, AppPolicy::default()))
        .unwrap();
    let pending = fixture::latest(&app_id, &run, 2, &missing_target);
    assert!(scope.management_job(&pending).await.is_err());
    assert_eq!(receipt_count(&service, &pending).await, 0);
    assert_eq!(head(&service, &app_id, &run).await, (0, "queued".into()));
}

async fn hold_row(service: &WorkflowService, app: &AppId, target: &DeployRegistration) -> Value {
    let tx = service.begin().await.unwrap();
    let mut rows = journal_rows(
        &tx,
        "deployment_holds",
        json!({"app_id":app.as_str(), "deploy_id":target.id}),
    )
    .await;
    assert_eq!(rows.len(), 1);
    tx.commit().await.unwrap();
    rows.remove(0).0
}

async fn hold_generation(service: &WorkflowService, id: &str, generation: i64) {
    let tx = service.begin().await.unwrap();
    tx.database()
        .collection("__zeroship_workflow_deployment_holds")
        .unwrap()
        .execute(Operation::Update {
            filter: value!({"id":id}),
            patch: value!({"generation":generation}),
            many: false,
        })
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

async fn fences(store: Rc<OrmStore>) {
    let (service, app_id, _, platform) = registered_service(store).await;
    let (service, source) = gated(service, &platform, &app_id);
    for change_policy in [true, false] {
        let run = start(&service, &app_id).await;
        let target = platform.deploy(&app_id).await;
        let command = fixture::latest(&app_id, &run, 1, &target);
        let scope = service.fixture_app(app_id.clone());
        let before = persisted(&service, &app_id).await;
        let gate = source.block();
        let (result, saved) = futures::join!(scope.management_job(&command), async {
            gate.entered.recv_async().await.unwrap();
            let saved = hold_row(&service, &app_id, &target).await;
            if change_policy {
                service
                    .policies
                    .fixture_install(&app_id, leased_policy(2, AppPolicy::default()))
                    .unwrap();
            } else {
                hold_generation(
                    &service,
                    saved["id"].as_str().unwrap(),
                    saved["generation"].as_i64().unwrap() + 1,
                )
                .await;
            }
            let _ = gate.resume.send_async(()).await;
            saved
        });
        assert!(result.is_err());
        assert_eq!(persisted(&service, &app_id).await, before);
        assert_eq!(receipt_count(&service, &command).await, 0);
        if !change_policy {
            hold_generation(
                &service,
                saved["id"].as_str().unwrap(),
                saved["generation"].as_i64().unwrap(),
            )
            .await;
        }
        scope.management_job(&command.retry()).await.unwrap();
        assert_deployment(&service, &app_id, &run, &target).await;
    }
}

/// One lifecycle attempt ends at the journal I/O ceiling, not at the end of the
/// authority it captured. The stall sits in the artifact source that verifying
/// a restart target reads, so the window measured here is the attempt's own
/// budget and not a journal wait.
///
/// This pins the composition, not the magnitude. Both arms move with
/// [`ATTEMPT_IO_CEILING`], so retuning the ceiling keeps them green; what fails
/// is dropping the ceiling term and handing one attempt its whole authority.
async fn io_ceiling(store: Rc<OrmStore>) {
    let (service, app_id, _, platform) = registered_service(store).await;
    let run = start(&service, &app_id).await;
    let target = platform.deploy(&app_id).await;
    let (service, source) = gated(service, &platform, &app_id);
    let scope = service.fixture_app(app_id.clone());

    let mut command = fixture::latest(&app_id, &run, 1, &target);
    command.expires = Instant::now() + ATTEMPT_IO_CEILING * 6;
    assert!(
        command.remaining().unwrap() > ATTEMPT_IO_CEILING * 3,
        "the fixture authority is narrower than the window asserted below, so it \
         would bound this attempt instead of the ceiling"
    );
    let held = source.block();
    let started = Instant::now();
    let result = scope.management_job(&command).await;
    let capped = started.elapsed();
    assert!(
        matches!(result, Err(WorkflowServiceError::Timeout)),
        "{result:?}"
    );
    assert!(
        capped >= ATTEMPT_IO_CEILING,
        "the attempt ended before the ceiling, so something other than its budget \
         stopped it and this measures nothing: {capped:?}"
    );
    assert!(
        capped < ATTEMPT_IO_CEILING * 3,
        "one attempt was handed authority beyond the ceiling: {capped:?}"
    );
    drop(held);

    // The control moves one variable. An authority narrower than the ceiling
    // binds the same stalled attempt instead, so the arm above is not a fixed
    // wait that would pass with the ceiling term removed.
    let narrow = ATTEMPT_IO_CEILING / 5;
    let mut short = command.retry();
    short.expires = Instant::now() + narrow;
    let held = source.block();
    let started = Instant::now();
    let result = scope.management_job(&short).await;
    let bounded = started.elapsed();
    assert!(
        matches!(result, Err(WorkflowServiceError::Timeout)),
        "{result:?}"
    );
    assert!(
        bounded >= narrow,
        "the control ended before its own authority, so it bounded nothing: {bounded:?}"
    );
    assert!(
        bounded < ATTEMPT_IO_CEILING,
        "the control was capped by the ceiling too, so the arm above proves \
         nothing: {bounded:?}"
    );
    drop(held);
    assert_eq!(receipt_count(&service, &command).await, 0);
    assert_eq!(head(&service, &app_id, &run).await, (0, "queued".into()));
}
