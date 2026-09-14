use super::*;
use crate::service::models::{job_publications, propagation_pages, propagations};
use zeroship_data_orm::orm::{Entity, Filter, Patch};

/// Damaged projections, obligations and page records fail closed without
/// effects, and another app's scope cannot deliver a page.
pub(super) async fn damage(store: Rc<OrmStore>) {
    let (service, app, foreign, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let parent = seed_runs(&service, &app, "Example", 1).await.remove(0);
    let children = seed_runs(&service, &app, "Child", 2).await;
    update_runs(
        &service,
        &app,
        &children,
        json!({"parent_id":parent, "parent_generation":0, "cascade":1, "depth":1}),
    )
    .await;
    cancel_idle(&scope, &parent).await;
    let page = open_page(&scope).await;
    let grant = JobGrant::new(&page);
    let options = PropagationOptions { page_size: 1 };

    // Another app's scope cannot deliver or read this page.
    assert!(matches!(
        service
            .fixture_app(foreign.clone())
            .propagation_job(&grant, options)
            .await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    // A null page projection cannot pass as this or any other operation.
    refused_while_damaged(&service, &scope, &grant, |damaged| {
        (
            job_publications::id.eq(page.id.as_str()).unwrap(),
            job_publications::propagation_revision
                .set(if damaged { None } else { Some(1_i64) })
                .unwrap(),
        )
    })
    .await;
    assert!(scope.pending_jobs(None, 100).await.is_ok());
    // An obligation whose revision disagrees with its page refuses fresh work.
    refused_while_damaged(&service, &scope, &grant, |damaged| {
        (
            propagations::app_id.eq(app.as_str()).unwrap(),
            propagations::revision
                .set(if damaged { 2_i64 } else { 1_i64 })
                .unwrap(),
        )
    })
    .await;
    let receipt = scope.propagation_job(&grant, options).await.unwrap();
    assert_eq!(receipt.outcome, JobOutcome::Waiting {});
    let next = open_page(&scope).await;
    // A changed committed page record cannot replay or authorize its successor.
    let tx = service.begin().await.unwrap();
    let original = journal_rows(&tx, "propagation_pages", json!({"id":page.id.as_str()}))
        .await
        .remove(0)
        .text("result")
        .unwrap();
    tx.commit().await.unwrap();
    let mut changed: serde_json::Value = serde_json::from_str(&original).unwrap();
    changed["affected"] = json!(i64::MAX);
    let restore = original.clone();
    let tx = service.begin().await.unwrap();
    tx.database()
        .entity::<propagation_pages::Entity>()
        .unwrap()
        .update_many(
            propagation_pages::id.eq(page.id.as_str()).unwrap(),
            propagation_pages::result.set(changed.to_string()).unwrap(),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let before = snapshot(&service, &app).await;
    assert!(scope.job_receipt(&page).await.is_err());
    assert!(scope
        .propagation_job(&grant.retry(), options)
        .await
        .is_err());
    assert!(scope
        .propagation_job(&JobGrant::new(&next), options)
        .await
        .is_err());
    assert_eq!(snapshot(&service, &app).await, before);
    let tx = service.begin().await.unwrap();
    tx.database()
        .entity::<propagation_pages::Entity>()
        .unwrap()
        .update_many(
            propagation_pages::id.eq(page.id.as_str()).unwrap(),
            propagation_pages::result.set(restore).unwrap(),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        scope
            .propagation_job(&grant.retry(), options)
            .await
            .unwrap(),
        receipt
    );
    assert_eq!(deliver_propagations_with(&scope, options).await.len(), 2);
}

/// Apply one damaging change, check that the page fails closed without
/// effects, then repair it.
async fn refused_while_damaged<E: Entity>(
    service: &WorkflowService,
    scope: &AppWorkflows,
    grant: &JobGrant,
    change: impl Fn(bool) -> (Filter<E>, Patch<E>),
) {
    write(service, change(true)).await;
    let before = snapshot(service, scope.app_id()).await;
    assert!(matches!(
        scope
            .propagation_job(grant, PropagationOptions { page_size: 1 })
            .await,
        Err(WorkflowServiceError::Internal(_))
    ));
    assert_eq!(snapshot(service, scope.app_id()).await, before);
    write(service, change(false)).await;
}

async fn write<E: Entity>(service: &WorkflowService, (filter, patch): (Filter<E>, Patch<E>)) {
    let tx = service.begin().await.unwrap();
    assert_eq!(
        tx.database()
            .entity::<E>()
            .unwrap()
            .update_many(filter, patch)
            .await
            .unwrap(),
        1
    );
    tx.commit().await.unwrap();
}
