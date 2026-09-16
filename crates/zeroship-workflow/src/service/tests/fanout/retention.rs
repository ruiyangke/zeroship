use super::*;
use crate::service::models::job_publications;

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
    let tx = service.begin().await.unwrap();
    assert_eq!(
        tx.database()
            .entity::<job_publications::Entity>()
            .unwrap()
            .update_many(
                job_publications::id.eq(advance.id.as_str()).unwrap(),
                job_publications::deploy_id.set(None::<&str>).unwrap(),
            )
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
    assert_eq!(
        tx.database()
            .entity::<job_publications::Entity>()
            .unwrap()
            .update_many(
                job_publications::id.eq(advance.id.as_str()).unwrap(),
                job_publications::deploy_id
                    .set(Some(deployment.as_str()))
                    .unwrap(),
            )
            .await
            .unwrap(),
        1
    );
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
