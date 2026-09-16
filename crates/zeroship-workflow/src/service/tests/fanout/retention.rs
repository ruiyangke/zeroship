use super::*;
use crate::service::models::advance_publications;

pub(super) async fn projection(store: Rc<OrmStore>) {
    let (service, app, _, platform) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let advance = scope.pending_jobs(None, 1).await.unwrap().remove(0);
    let deployment = advance.deployment_id().unwrap().clone();
    broadcast(&scope, "code-free intent").await;
    let unused = platform.deploy(&app).await;
    let client = platform.client(&app);
    service
        .acquire_deployment_hold(&app, &unused.id, &unused.hash, &client)
        .await
        .unwrap();
    // An intent whose advance projection is gone names no operation, and the
    // retention gate reads every pending specification, so it refuses rather
    // than releasing code the intent still depends on.
    let tx = service.begin().await.unwrap();
    let stored = journal_rows(
        &tx,
        "advance_publications",
        json!({"app_id":app.as_str(), "id":advance.id.as_str()}),
    )
    .await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].text("deploy_id").unwrap(), deployment.as_str());
    let projection = json!({
        "id": advance.id.as_str(), "app_id": app.as_str(),
        "deploy_id": deployment.as_str(),
        "run_id": stored[0].text("run_id").unwrap(),
        "generation": stored[0].integer("generation").unwrap(),
        "frontier_revision": stored[0].integer("frontier_revision").unwrap(),
        "available_at": stored[0].integer("available_at").unwrap(),
    });
    assert_eq!(
        tx.database()
            .entity::<advance_publications::Entity>()
            .unwrap()
            .delete_many(advance_publications::id.eq(advance.id.as_str()).unwrap())
            .await
            .unwrap(),
        1
    );
    tx.commit().await.unwrap();
    assert!(matches!(
        service
            .release_deployment_hold(&app, &unused.id, &client)
            .await,
        Err(WorkflowServiceError::Internal(_))
    ));
    platform.assert_held(&app, &unused.id).await;
    let tx = service.begin().await.unwrap();
    journal_insert(&tx, "advance_publications", projection)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    service
        .release_deployment_hold(&app, &unused.id, &client)
        .await
        .unwrap();
    assert!(scope
        .pending_jobs(None, 100)
        .await
        .unwrap()
        .iter()
        .any(|job| matches!(job.operation, JobOperation::Fanout { .. })));
}
