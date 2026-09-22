//! The journal a prepared creator backend reads and writes.
//!
//! `into_backend` names the store every call through the returned backend
//! reaches, while the app identity, its policy binding and its deployments stay
//! as the handle holds them. A backend can therefore answer for an app over a
//! journal other than the one its handle was bound from, and a journal opened
//! over another policy registry is refused where it is named.

use super::objects::{Objects, StepOutputs};
use super::*;
use crate::{backend::WorkflowBackend, service::AppDeployments};

/// A journal this app is registered on with `deploy` active, so its app lock is
/// takeable there. Both journals in a test carry the same deployment, so the
/// shared retention ledger holds one deployment for the app.
#[expect(
    clippy::future_not_send,
    reason = "fixture bindings own compio-local journals"
)]
async fn journal(
    store: Rc<OrmStore>,
    policies: &Arc<HostPolicies>,
    deployments: &Deployments,
    bound: &AppDeployments,
    app: &AppId,
    deploy: &DeployRegistration,
) -> WorkflowService {
    let service = WorkflowService::open(store, policies.clone())
        .await
        .unwrap()
        .with_deployments(bound.clone());
    service
        .fixture_register(app, leased_policy(1, AppPolicy::default()))
        .await
        .unwrap();
    deployments.activate(&service, app, deploy).await.unwrap();
    service
}

/// The run ids one journal holds for an app, in journal order.
#[expect(
    clippy::future_not_send,
    reason = "fixture bindings own compio-local journals"
)]
async fn run_ids(store: &Rc<OrmStore>, app: &AppId) -> Vec<String> {
    let tx = store.begin().await.unwrap();
    let rows = journal_rows(&tx, "runs", json!({ "app_id": app.as_str() })).await;
    tx.commit().await.unwrap();
    rows.iter().map(|row| row.text("id").unwrap()).collect()
}

#[compio::test]
async fn a_backend_reads_and_writes_the_journal_it_was_built_over() {
    let home_directory = tempfile::tempdir().unwrap();
    let away_directory = tempfile::tempdir().unwrap();
    let home_store = Rc::new(sqlite_store(&home_directory.path().join("zs-workflow.sqlite")).await);
    let away_store = Rc::new(sqlite_store(&away_directory.path().join("zs-workflow.sqlite")).await);
    let deployments = Deployments::new().await;
    let policies = Arc::new(HostPolicies::default());
    let app = AppId::mint();
    let bound = deployments.binding(&[&app]);
    let deploy = DeployRegistration {
        id: typed_id::generate("dep"),
        hash: "a".repeat(64),
        workflows: ["Example".into()].into(),
        schedules: Vec::new(),
    };
    let home = journal(
        home_store.clone(),
        &policies,
        &deployments,
        &bound,
        &app,
        &deploy,
    )
    .await;
    let away = journal(
        away_store.clone(),
        &policies,
        &deployments,
        &bound,
        &app,
        &deploy,
    )
    .await;

    // A run only the home journal holds. A read that lands on the handle's own
    // journal answers for it instead of reporting the run missing.
    let home_only = home
        .fixture_app(app.clone())
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();

    let objects = Objects::new();
    let client = home
        .fixture_app(app.clone())
        .into_backend(&away, StepOutputs::shared(&objects, 1024))
        .unwrap();
    let run = client
        .start("Example".into(), StartOptions::default())
        .await
        .unwrap();
    assert_eq!(
        client.status(run.id.clone()).await.unwrap().state,
        run.state,
        "the backend reads back the run it just started"
    );
    match client.status(home_only.id.clone()).await {
        Err(WorkflowServiceError::NotFound(_)) => {}
        other => panic!(
            "a backend naming the away journal must not read the home journal: {other:?}"
        ),
    }

    assert_eq!(
        run_ids(&away_store, &app).await,
        vec![run.id.clone()],
        "the started run belongs to the journal the backend names"
    );
    assert_eq!(
        run_ids(&home_store, &app).await,
        vec![home_only.id],
        "the handle's own journal keeps only what was started on it directly"
    );
}

#[compio::test]
async fn into_backend_refuses_a_journal_from_another_policy_registry() {
    let home_directory = tempfile::tempdir().unwrap();
    let away_directory = tempfile::tempdir().unwrap();
    let home_store = Rc::new(sqlite_store(&home_directory.path().join("zs-workflow.sqlite")).await);
    let away_store = Rc::new(sqlite_store(&away_directory.path().join("zs-workflow.sqlite")).await);
    let (home, app, _, _deployments) = registered_service(home_store).await;
    let objects = Objects::new();

    // The control differs from the refusal below only in the registry the
    // journal was opened over, so a second store is not what is refused.
    let shared = WorkflowService::open(away_store.clone(), home.policies.clone())
        .await
        .unwrap();
    home.fixture_app(app.clone())
        .into_backend(&shared, StepOutputs::shared(&objects, 1024))
        .expect("a journal over this handle's own registry binds");

    let foreign = WorkflowService::open(away_store, Arc::new(HostPolicies::default()))
        .await
        .unwrap();
    match home
        .fixture_app(app)
        .into_backend(&foreign, StepOutputs::shared(&objects, 1024))
    {
        Err(WorkflowServiceError::PermissionDenied) => {}
        other => panic!(
            "a journal from another policy registry must be refused where it is named: {other:?}"
        ),
    }
}
