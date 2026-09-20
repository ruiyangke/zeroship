use super::*;

pub(super) async fn damaged_specification(store: Rc<OrmStore>) {
    let (service, app, _, platform) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let advance = scope.pending_jobs(None, 1).await.unwrap().remove(0);
    broadcast(&scope, "code-free intent").await;
    let unused = platform.deploy(&app).await;
    let client = platform.client(&app);
    service
        .acquire_deployment_hold(&app, &unused.id, &unused.hash, &client)
        .await
        .unwrap();
    // An intent whose specification no longer derives the key it is stored
    // under names no operation this journal recorded, and the retention gate
    // reads every pending specification, so it refuses rather than releasing
    // code the intent still depends on.
    let tx = service.begin().await.unwrap();
    let original = damage_specification(&tx, &app, advance.id.as_str()).await;
    tx.commit().await.unwrap();
    assert!(matches!(
        service
            .release_deployment_hold(&app, &unused.id, &client)
            .await,
        Err(WorkflowServiceError::Internal(_))
    ));
    platform.assert_held(&app, &unused.id).await;
    let tx = service.begin().await.unwrap();
    write_specification(&tx, advance.id.as_str(), &original).await;
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
