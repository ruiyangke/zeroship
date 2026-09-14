use super::*;

paired!(
    sqlite_pending_child_corruption_cannot_disappear_from_wakeup,
    postgres_pending_child_corruption_cannot_disappear_from_wakeup,
    pending_corruption
);

async fn pending_corruption(store: Rc<OrmStore>) {
    let (service, app_id, foreign, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app_id.clone());
    let worker = WorkerIdentity::new("continuation-corruption".into()).unwrap();
    let owner = parent(&service, &scope, &worker, None).await;
    let (child, accepted) = accepted(&scope, &owner).await;
    let task = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, child);
    let mut tx = scope.service.begin().await.unwrap();
    app::lock_app(&mut tx, &app_id).await.unwrap();
    assert!(matches!(
        continuations::by_id(&tx, &foreign, &accepted).await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    let member = continuations::by_id(&tx, &app_id, &accepted).await.unwrap();
    set_head_revision(&tx, &member.head_id, member.revision + 1).await;
    tx.commit().await.unwrap();
    refuses_completion(&service, &app_id, &worker, &task).await;
    let tx = service.begin().await.unwrap();
    set_head_revision(&tx, &member.head_id, member.revision).await;
    tx.commit().await.unwrap();
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([{"kind":"RunCompleted", "output":"repaired"}])),
        )
        .await
        .unwrap();
    let parent = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(parent.invocation.run_id, owner);
    assert_eq!(parent.invocation.journal[0].output, Some(json!("repaired")));
}

// PostgreSQL refuses a child checkpoint without its accepted member, so no
// pending parent can fall out of its child's head-directed wakeup. The
// migration compiler renders table checks for PostgreSQL only.
#[compio::test]
async fn postgres_child_checkpoints_require_their_accepted_member() {
    let fixture = PostgresFixture::start().await;
    let store = Rc::new(fixture.store.clone());
    let (service, app_id, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app_id.clone());
    let worker = WorkerIdentity::new("continuation-linkage".into()).unwrap();
    let owner = parent(&service, &scope, &worker, None).await;
    let (_, accepted) = accepted(&scope, &owner).await;
    let before = all_state(&service, &app_id).await;
    let unlink = |member: Option<String>| {
        let service = &service;
        let (app_id, owner) = (app_id.clone(), owner.clone());
        async move {
            let tx = service.begin().await.unwrap();
            let written = tx
                .database()
                .entity::<models::steps::Entity>()
                .unwrap()
                .update_many(
                    models::steps::app_id
                        .eq(app_id.as_str())
                        .unwrap()
                        .and(models::steps::run_id.eq(owner.as_str()).unwrap()),
                    models::steps::child_member_id
                        .set(member.as_deref())
                        .unwrap(),
                )
                .await;
            (tx, written)
        }
    };
    let (tx, cleared) = unlink(None).await;
    assert!(cleared.is_err(), "a child checkpoint must keep its member");
    drop(tx);
    // Control: the same write naming the accepted member is valid.
    let (tx, kept) = unlink(Some(accepted)).await;
    assert_eq!(kept.unwrap(), 1);
    tx.commit().await.unwrap();
    assert_eq!(all_state(&service, &app_id).await, before);
}

async fn set_head_revision(tx: &crate::service::store::Transaction, head: &str, revision: i64) {
    tx.database()
        .entity::<models::continuation_heads::Entity>()
        .unwrap()
        .update_many(
            models::continuation_heads::id.eq(head).unwrap(),
            models::continuation_heads::revision.set(revision).unwrap(),
        )
        .await
        .unwrap();
}

async fn refuses_completion(
    service: &WorkflowService,
    app: &AppId,
    worker: &WorkerIdentity,
    task: &crate::service::TaskAssignment,
) {
    let before = all_state(service, app).await;
    assert!(matches!(
        service
            .complete(
                worker,
                &task.id,
                &task.token,
                execution(json!([{"kind":"RunCompleted", "output":"must roll back"}]))
            )
            .await,
        Err(WorkflowServiceError::Internal(_))
    ));
    assert_eq!(all_state(service, app).await, before);
}
