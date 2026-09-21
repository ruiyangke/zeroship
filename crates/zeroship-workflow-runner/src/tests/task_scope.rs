#![expect(
    clippy::future_not_send,
    reason = "scoped task contracts use creator ORM connections on their compio thread"
)]

use crate::{
    deployment_fixture::Deployments,
    journal_fixture::{leased_policy, registered_service, sqlite_store, PostgresFixture},
    service_binding::ServiceFixture,
    TaskPayloads, TaskTransport, WorkerBinding,
};
use std::rc::Rc;
use zeroship_core::typed_id;
use zeroship_workflow::{
    operations::StartOptions,
    service::{
        schema, store::OrmStore, AppPolicy, AppWorkflows, DeployRegistration, RequestId,
        TaskAssignment, WorkerIdentity, WorkflowService,
    },
    WorkflowServiceError,
};

async fn start(
    deployments: &Deployments,
    service: &WorkflowService,
    app: &AppWorkflows,
    worker: &WorkerIdentity,
) -> TaskAssignment {
    deployments
        .activate(
            service,
            app.app_id(),
            &DeployRegistration {
                id: typed_id::generate("dep"),
                hash: "b".repeat(64),
                workflows: ["Example".into()].into(),
                schedules: vec![],
            },
        )
        .await
        .unwrap();
    app.start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let task = app.tasks(worker.clone()).poll().await.unwrap().unwrap();
    assert_eq!(task.invocation.app_id, app.app_id().as_str());
    task
}

async fn contract(store: Rc<OrmStore>) {
    let (service, first, second, deployments) = registered_service(store).await;
    let first = service.fixture_app(first);
    let second = service.fixture_app(second);
    let worker = WorkerIdentity::new("scoped-task-worker".into()).unwrap();
    let task_a = start(&deployments, &service, &first, &worker).await;
    let task_b = start(&deployments, &service, &second, &worker).await;
    let tasks = first.tasks(worker.clone());

    tasks.heartbeat(&task_a.id, &task_a.token).await.unwrap();
    second
        .tasks(worker.clone())
        .heartbeat(&task_b.id, &task_b.token)
        .await
        .unwrap();
    assert!(matches!(
        tasks.heartbeat(&task_b.id, &task_b.token).await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    assert!(matches!(
        TaskPayloads::executable(&tasks, &task_b.id, &task_b.token).await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    assert!(matches!(
        tasks.release(&task_b.id, &task_b.token).await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    second
        .tasks(worker.clone())
        .heartbeat(&task_b.id, &task_b.token)
        .await
        .unwrap();

    // Replacing this app's host binding must not refresh retained task handles.
    let replacement = service.policies().bind(first.app_id().clone()).unwrap();
    replacement
        .begin_refresh()
        .unwrap()
        .install(leased_policy(2, AppPolicy::default()))
        .unwrap();
    let fresh = service.bind_app(&replacement).unwrap().tasks(worker);
    assert!(matches!(
        tasks.heartbeat(&task_a.id, &task_a.token).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert!(matches!(
        TaskPayloads::executable(&tasks, &task_a.id, &task_a.token).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    fresh.heartbeat(&task_a.id, &task_a.token).await.unwrap();
}

#[compio::test]
async fn sqlite_task_handles_retain_app_and_policy_generation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("creator.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    Box::pin(contract(Rc::new(sqlite_store(&path).await))).await;
}

#[compio::test]
async fn postgres_task_handles_retain_app_and_policy_generation() {
    let fixture = PostgresFixture::start().await;
    Box::pin(contract(Rc::new(fixture.store.clone()))).await;
}
