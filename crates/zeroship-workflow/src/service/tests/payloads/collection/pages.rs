use super::*;
use crate::service::models::{collection_pages, job_receipts};

pub(super) async fn pages(store: Rc<OrmStore>) {
    let fixture = Fixture::new(store).await;
    let mut old = Vec::new();
    for _ in 0..3 {
        let id = fixture.stage().await;
        fixture.expire(&id).await;
        old.push(id);
    }
    old.sort();
    let future = fixture.stage().await;
    fixture
        .service
        .fixture_app(fixture.other.clone())
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let foreign_task = fixture
        .service
        .poll(&fixture.worker)
        .await
        .unwrap()
        .unwrap();
    let foreign = fixture
        .service
        .stage_payload(
            &fixture.worker,
            &foreign_task.id,
            &foreign_task.token,
            &RequestId::mint(),
            reference(b"foreign"),
            body(b"foreign"),
        )
        .await
        .unwrap();
    fixture.expire(&foreign.id).await;
    let first = Grant::new(fixture.scope.app_id());
    assert_eq!(
        fixture
            .scope
            .collect_job(&first, options(2))
            .await
            .unwrap()
            .outcome,
        JobOutcome::Waiting {}
    );
    assert_eq!(fixture.backend.calls(), old[..2]);
    let captured = fixture.scan().await;
    assert_eq!(captured.after_id.as_deref(), Some(old[1].as_str()));
    assert_eq!(captured.upper_id.as_deref(), Some(old[2].as_str()));
    let late = fixture.stage().await;
    fixture.expire(&late).await;
    // The earlier observation owns eligibility even if the object becomes due later.
    fixture
        .set_expiry(&future, captured.observed_at.unwrap() + 1)
        .await;
    let reopened = fixture.reopen(true).await;
    let second = Grant::new(fixture.scope.app_id());
    assert_eq!(
        reopened
            .collect_job(&second, options(2))
            .await
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );
    assert_eq!(fixture.backend.calls(), old);
    assert!(fixture.exists(fixture.scope.app_id(), &future).await);
    assert!(fixture.exists(fixture.scope.app_id(), &late).await);
    assert!(fixture.exists(&fixture.other, &foreign.id).await);
    let closed = fixture.scan().await;
    assert!(closed.after_id.is_none() && closed.upper_id.is_none() && closed.observed_at.is_none());
    fixture.expire(&future).await;
    assert_eq!(
        reopened
            .collect_job(&Grant::new(fixture.scope.app_id()), options(2))
            .await
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );
    assert!(!fixture.exists(fixture.scope.app_id(), &future).await);
    assert!(!fixture.exists(fixture.scope.app_id(), &late).await);
    assert_eq!(fixture.payload(&foreign.id).await.state, "staged");
    assert!(fixture
        .scope
        .job_receipt(&first.delivery.job)
        .await
        .unwrap()
        .is_some());
    assert_eq!(fixture.scan().await.revision, closed.revision + 1);
}

pub(super) async fn replay(store: Rc<OrmStore>) {
    let fixture = Fixture::new(store).await;
    let id = fixture.stage().await;
    fixture.expire(&id).await;
    let grant = Grant::new(fixture.scope.app_id());
    let receipt = fixture.scope.collect_job(&grant, options(2)).await.unwrap();
    let original = fixture.page(&grant.delivery.job).await;
    let before = fixture.scan().await;
    let plan: serde_json::Value = serde_json::from_str(&original.plan).unwrap();
    let mut unknown = plan.clone();
    unknown["unknown"] = json!(true);
    let mut wrong_outcome = plan.clone();
    wrong_outcome["more"] = json!(true);
    let mut future_revision = plan;
    future_revision["revision"] = json!(before.revision + 1);
    for damaged in [
        unknown.to_string(),
        wrong_outcome.to_string(),
        future_revision.to_string(),
        "invalid".into(),
    ] {
        set_plan(&fixture, &grant, &damaged, original.next_index).await;
        assert!(fixture
            .scope
            .job_receipt(&grant.delivery.job)
            .await
            .is_err());
        assert!(fixture
            .scope
            .collect_job(&grant.retry(), options(2))
            .await
            .is_err());
        assert_eq!(fixture.scan().await, before);
        assert_eq!(fixture.backend.calls(), std::slice::from_ref(&id));
    }
    set_plan(&fixture, &grant, &original.plan, 0).await;
    assert!(fixture
        .scope
        .collect_job(&grant.retry(), options(2))
        .await
        .is_err());
    set_plan(&fixture, &grant, &original.plan, original.next_index).await;
    set_outcome(&fixture, &grant, &JobOutcome::Rejected {}).await;
    assert!(fixture
        .scope
        .collect_job(&grant.retry(), options(2))
        .await
        .is_err());
    set_outcome(&fixture, &grant, &receipt.outcome).await;
    assert_eq!(
        fixture
            .scope
            .collect_job(&grant.retry(), options(2))
            .await
            .unwrap(),
        receipt
    );
    let mut changed = grant.retry();
    changed.delivery.job.available_at = 1.try_into().unwrap();
    assert!(fixture
        .scope
        .collect_job(&changed, options(2))
        .await
        .is_err());
    changed.delivery.job = grant.delivery.job.clone();
    changed.delivery.job.app_id = fixture.other.clone();
    assert!(matches!(
        fixture.scope.collect_job(&changed, options(2)).await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    let tx = fixture.store.begin().await.unwrap();
    tx.database()
        .entity::<collection_pages::Entity>()
        .unwrap()
        .delete_many(
            collection_pages::id
                .eq(grant.delivery.job.id.as_str())
                .unwrap(),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(fixture
        .scope
        .collect_job(&grant.retry(), options(2))
        .await
        .is_err());
    assert_eq!(fixture.backend.calls(), [id]);
}

async fn set_plan(fixture: &Fixture, grant: &Grant, plan: &str, next: i64) {
    let tx = fixture.store.begin().await.unwrap();
    assert_eq!(
        tx.database()
            .entity::<collection_pages::Entity>()
            .unwrap()
            .update_many(
                collection_pages::id
                    .eq(grant.delivery.job.id.as_str())
                    .unwrap(),
                collection_pages::plan
                    .set(plan)
                    .unwrap()
                    .and(collection_pages::next_index.set(next).unwrap())
                    .unwrap()
            )
            .await
            .unwrap(),
        1
    );
    tx.commit().await.unwrap();
}

async fn set_outcome(fixture: &Fixture, grant: &Grant, outcome: &JobOutcome) {
    let tx = fixture.store.begin().await.unwrap();
    assert_eq!(
        tx.database()
            .entity::<job_receipts::Entity>()
            .unwrap()
            .update_many(
                job_receipts::id.eq(grant.delivery.job.id.as_str()).unwrap(),
                job_receipts::outcome
                    .set(Some(serde_json::to_string(outcome).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap(),
        1
    );
    tx.commit().await.unwrap();
}

pub(super) async fn fairness(store: Rc<OrmStore>) {
    let fixture = Fixture::new(store).await;
    let malformed = fixture.stage().await;
    fixture.expire(&malformed).await;
    Box::pin(fixture.damage_identity(&malformed)).await;
    let mut valid = Vec::new();
    for _ in 0..3 {
        let id = fixture.stage().await;
        fixture.expire(&id).await;
        valid.push(id);
    }
    valid.sort();
    fixture
        .backend
        .faults
        .lock()
        .unwrap()
        .extend([Fault::Fail(valid[0].clone()), Fault::Hang(valid[1].clone())]);
    let grant = Grant::new(fixture.scope.app_id());
    assert_eq!(
        fixture
            .scope
            .collect_job(&grant, options(4))
            .await
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );
    assert_eq!(fixture.backend.calls(), valid);
    assert_eq!(fixture.page(&grant.delivery.job).await.next_index, 4);
    assert_eq!(fixture.payload("").await.state, "staged");
    assert_eq!(fixture.payload(&valid[0]).await.state, "deleting");
    assert_eq!(fixture.payload(&valid[1]).await.state, "deleting");
    assert_eq!(fixture.payload(&valid[2]).await.state, "deleted");
    assert!(fixture.exists(fixture.scope.app_id(), &valid[0]).await);
    assert!(!fixture.exists(fixture.scope.app_id(), &valid[2]).await);
    let calls = fixture.backend.calls();
    fixture
        .reopen(true)
        .await
        .collect_job(&grant.retry(), options(1))
        .await
        .unwrap();
    assert_eq!(fixture.backend.calls(), calls);
    fixture
        .scope
        .collect_job(&Grant::new(fixture.scope.app_id()), options(4))
        .await
        .unwrap();
    assert_eq!(fixture.payload(&valid[0]).await.state, "deleted");
    assert_eq!(fixture.payload(&valid[1]).await.state, "deleted");
    assert_eq!(fixture.payload("").await.state, "staged");
}

pub(super) async fn expired_replay(store: Rc<OrmStore>) {
    let fixture = Fixture::new(store).await;
    let id = fixture.stage().await;
    fixture.expire(&id).await;
    let grant = Grant::new(fixture.scope.app_id());
    let receipt = fixture.scope.collect_job(&grant, options(2)).await.unwrap();
    let payload = fixture.payload(&id).await;
    let scan = fixture.scan().await;
    let binding = fixture
        .service
        .policies
        .current_binding(fixture.scope.app_id())
        .unwrap();
    binding
        .begin_refresh()
        .unwrap()
        .install(
            PolicySnapshot::lease(2.try_into().unwrap(), AppPolicy::default(), Instant::now())
                .unwrap(),
        )
        .unwrap();
    let reopened = fixture.reopen(false).await;
    let mut retry = grant.retry();
    retry.expires = Instant::now();
    assert_eq!(
        reopened.collect_job(&retry, options(1)).await.unwrap(),
        receipt
    );
    assert_eq!(
        reopened.job_receipt(&retry.delivery.job).await.unwrap(),
        Some(receipt)
    );
    assert!(reopened
        .collect_job(&Grant::new(fixture.scope.app_id()), options(1))
        .await
        .is_err());
    assert_eq!(fixture.payload(&id).await, payload);
    assert_eq!(fixture.scan().await, scan);
    assert_eq!(fixture.backend.calls(), [id]);
}
