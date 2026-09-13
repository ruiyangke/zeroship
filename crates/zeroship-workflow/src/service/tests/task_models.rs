use super::*;
use crate::service::{models, WorkerIdentity};
use zeroship_data_orm::{
    orm::{Entity, FindOptions},
    value,
};

#[compio::test]
async fn sqlite_recovery_refuses_a_foreign_task_reference() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
    foreign_reference(Rc::new(store)).await;
}

#[compio::test]
async fn postgres_recovery_refuses_a_foreign_task_reference() {
    let fixture = PostgresFixture::start().await;
    foreign_reference(Rc::new(fixture.store.clone())).await;
}

async fn foreign_reference(store: Rc<OrmStore>) {
    let (service, app, foreign, _deployments) = registered_service(store.clone()).await;
    let worker = WorkerIdentity::new("task-model-worker".into()).unwrap();
    let mut assignments = Vec::new();
    for app_id in [&app, &foreign] {
        let started = service
            .for_app(app_id.clone())
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await
            .unwrap();
        let task = service.poll(&worker).await.unwrap().unwrap();
        assert_eq!(task.invocation.run_id, started.id);
        assignments.push(task);
    }
    let local = &assignments[0];
    let other = &assignments[1];
    let tx = store.begin().await.unwrap();
    tx.database()
        .collection(models::runs::Entity::COLLECTION)
        .unwrap()
        .update(
            value!({"app_id":app.as_str(), "id":local.invocation.run_id.clone()}),
            value!({"task_id":other.id.clone(), "due_at":0}),
        )
        .await
        .unwrap();
    tx.database()
        .collection(models::tasks::Entity::COLLECTION)
        .unwrap()
        .update(value!({"id":other.id.clone()}), value!({"deadline":0}))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let reopened = WorkflowService::open(store.clone(), Arc::new(HostPolicies::default()))
        .await
        .unwrap();
    reopened
        .register_app(&app, configured_policy(1, AppPolicy::default()))
        .await
        .unwrap();
    assert!(matches!(
        reopened.poll(&worker).await,
        Err(WorkflowServiceError::Internal(_))
    ));
    let heartbeat = reopened.heartbeat(&worker, &other.id, &other.token).await;
    assert!(
        matches!(heartbeat, Err(WorkflowServiceError::NotFound(_))),
        "a task outside the host's apps must be invisible: {heartbeat:?}"
    );
    let mut tx = store.begin().await.unwrap();
    for assignment in &assignments {
        let task = tx
            .database()
            .entity::<models::tasks::Entity>()
            .unwrap()
            .find::<models::TaskRecord>(
                models::tasks::id.eq(assignment.id.as_str()).unwrap(),
                FindOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(task.len(), 1);
        assert_eq!(task[0].state, "leased");
        assert_eq!(task[0].epoch, assignment.epoch);
        assert_eq!(task[0].app_id, assignment.invocation.app_id);
        assert_eq!(task[0].run_id, assignment.invocation.run_id);
    }
    let run = super::super::app::lock_run(&mut tx, &app, &local.invocation.run_id)
        .await
        .unwrap();
    assert_eq!(run.text("task_id").unwrap(), other.id);
    assert_eq!(run.integer("due_at").unwrap(), 0);
    tx.commit().await.unwrap();
}
