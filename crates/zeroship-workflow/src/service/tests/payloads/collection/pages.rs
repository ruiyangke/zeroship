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
            fixture.objects.upload(b"foreign"),
        )
        .await
        .unwrap();
    fixture.expire(&foreign.id).await;
    let first = Grant::new(fixture.scope.app_id());
    assert_eq!(
        fixture
            .scope
            .collect_job(&first, options(2), &fixture.objects)
            .await
            .unwrap()
            .outcome,
        JobOutcome::Waiting {}
    );
    assert_eq!(fixture.objects.deletes(), old[..2]);
    let captured = fixture.scan().await;
    assert_eq!(
        captured.collection_after_id.as_deref(),
        Some(old[1].as_str())
    );
    assert_eq!(
        captured.collection_upper_id.as_deref(),
        Some(old[2].as_str())
    );
    let late = fixture.stage().await;
    fixture.expire(&late).await;
    // The earlier observation owns eligibility even if the object becomes due later.
    fixture
        .set_expiry(&future, captured.collection_observed_at.unwrap() + 1)
        .await;
    let reopened = fixture.reopen().await;
    let second = Grant::new(fixture.scope.app_id());
    assert_eq!(
        reopened
            .collect_job(&second, options(2), &fixture.objects)
            .await
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );
    assert_eq!(fixture.objects.deletes(), old);
    assert!(fixture.exists(fixture.scope.app_id(), &future));
    assert!(fixture.exists(fixture.scope.app_id(), &late));
    assert!(fixture.exists(&fixture.other, &foreign.id));
    let closed = fixture.scan().await;
    assert!(
        closed.collection_after_id.is_none()
            && closed.collection_upper_id.is_none()
            && closed.collection_observed_at.is_none()
    );
    fixture.expire(&future).await;
    assert_eq!(
        reopened
            .collect_job(
                &Grant::new(fixture.scope.app_id()),
                options(2),
                &fixture.objects
            )
            .await
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );
    assert!(!fixture.exists(fixture.scope.app_id(), &future));
    assert!(!fixture.exists(fixture.scope.app_id(), &late));
    assert_eq!(fixture.payload(&foreign.id).await.state, "staged");
    assert!(fixture
        .scope
        .job_receipt(&first.delivery.job)
        .await
        .unwrap()
        .is_some());
    assert_eq!(
        fixture.scan().await.collection_revision,
        closed.collection_revision + 1
    );
}

pub(super) async fn replay(store: Rc<OrmStore>) {
    let fixture = Fixture::new(store).await;
    let id = fixture.stage().await;
    fixture.expire(&id).await;
    let grant = Grant::new(fixture.scope.app_id());
    let receipt = fixture
        .scope
        .collect_job(&grant, options(2), &fixture.objects)
        .await
        .unwrap();
    let original = fixture.page(&grant.delivery.job).await;
    let before = fixture.scan().await;
    let plan: serde_json::Value = serde_json::from_str(&original.plan).unwrap();
    let mut unknown = plan.clone();
    unknown["unknown"] = json!(true);
    let mut wrong_outcome = plan.clone();
    wrong_outcome["more"] = json!(true);
    let mut future_revision = plan;
    future_revision["revision"] = json!(before.collection_revision + 1);
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
            .collect_job(&grant.retry(), options(2), &fixture.objects)
            .await
            .is_err());
        assert_eq!(fixture.scan().await, before);
        assert_eq!(fixture.objects.deletes(), std::slice::from_ref(&id));
    }
    set_plan(&fixture, &grant, &original.plan, 0).await;
    assert!(fixture
        .scope
        .collect_job(&grant.retry(), options(2), &fixture.objects)
        .await
        .is_err());
    set_plan(&fixture, &grant, &original.plan, original.next_index).await;
    set_outcome(&fixture, &grant, &JobOutcome::Rejected {}).await;
    assert!(fixture
        .scope
        .collect_job(&grant.retry(), options(2), &fixture.objects)
        .await
        .is_err());
    set_outcome(&fixture, &grant, &receipt.outcome).await;
    assert_eq!(
        fixture
            .scope
            .collect_job(&grant.retry(), options(2), &fixture.objects)
            .await
            .unwrap(),
        receipt
    );
    let mut changed = grant.retry();
    changed.delivery.job.available_at = 1.try_into().unwrap();
    assert!(fixture
        .scope
        .collect_job(&changed, options(2), &fixture.objects)
        .await
        .is_err());
    changed.delivery.job = grant.delivery.job.clone();
    changed.delivery.job.app_id = fixture.other.clone();
    assert!(matches!(
        fixture
            .scope
            .collect_job(&changed, options(2), &fixture.objects)
            .await,
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
        .collect_job(&grant.retry(), options(2), &fixture.objects)
        .await
        .is_err());
    assert_eq!(fixture.objects.deletes(), [id]);
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
    fixture.objects.fail(&valid[0]);
    fixture.objects.hang(&valid[1]);
    let grant = Grant::new(fixture.scope.app_id());
    assert_eq!(
        fixture
            .scope
            .collect_job(&grant, options(4), &fixture.objects)
            .await
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );
    assert_eq!(fixture.objects.deletes(), valid);
    assert_eq!(fixture.page(&grant.delivery.job).await.next_index, 4);
    assert_eq!(fixture.payload("").await.state, "staged");
    assert_eq!(fixture.payload(&valid[0]).await.state, "deleting");
    assert_eq!(fixture.payload(&valid[1]).await.state, "deleting");
    assert_eq!(fixture.payload(&valid[2]).await.state, "deleted");
    assert!(fixture.exists(fixture.scope.app_id(), &valid[0]));
    assert!(!fixture.exists(fixture.scope.app_id(), &valid[2]));
    let calls = fixture.objects.deletes();
    fixture
        .reopen()
        .await
        .collect_job(&grant.retry(), options(1), &fixture.objects)
        .await
        .unwrap();
    assert_eq!(fixture.objects.deletes(), calls);
    fixture
        .scope
        .collect_job(
            &Grant::new(fixture.scope.app_id()),
            options(4),
            &fixture.objects,
        )
        .await
        .unwrap();
    assert_eq!(fixture.payload(&valid[0]).await.state, "deleted");
    assert_eq!(fixture.payload(&valid[1]).await.state, "deleted");
    assert_eq!(fixture.payload("").await.state, "staged");
}

/// A deleter that refuses every object effect, so a sweep that reaches the
/// object store fails the run it is handed to.
struct NoDeletion;
#[async_trait::async_trait(?Send)]
impl crate::service::PayloadDeleter for NoDeletion {
    async fn delete(&self, _app: &AppId, _id: &str) -> Result<(), WorkflowServiceError> {
        panic!("a committed receipt replays without deleting an object");
    }
}

pub(super) async fn expired_replay(store: Rc<OrmStore>) {
    let fixture = Fixture::new(store).await;
    let id = fixture.stage().await;
    fixture.expire(&id).await;
    let grant = Grant::new(fixture.scope.app_id());
    let receipt = fixture
        .scope
        .collect_job(&grant, options(2), &fixture.objects)
        .await
        .unwrap();
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
    let reopened = fixture.reopen().await;
    let mut retry = grant.retry();
    retry.expires = Instant::now();
    assert_eq!(
        reopened
            .collect_job(&retry, options(1), &NoDeletion)
            .await
            .unwrap(),
        receipt
    );
    assert_eq!(
        reopened.job_receipt(&retry.delivery.job).await.unwrap(),
        Some(receipt)
    );
    assert!(reopened
        .collect_job(&Grant::new(fixture.scope.app_id()), options(1), &NoDeletion)
        .await
        .is_err());
    assert_eq!(fixture.payload(&id).await, payload);
    assert_eq!(fixture.scan().await, scan);
    assert_eq!(fixture.objects.deletes(), [id]);
}

/// Two sweeps of one app that both planned at the same scan revision, settling
/// in order. The compare-and-set lets exactly one of them advance the scan, so
/// the later sweep cannot write its stale cursor over the earlier one's. It
/// also reaches only the app it names: a second app in the same schema keeps
/// the sweep state its own registration left.
pub(super) async fn lost_update(store: Rc<OrmStore>) {
    let fixture = Fixture::new(store).await;
    let app = fixture.scope.app_id().clone();
    let id = fixture.stage().await;
    fixture.expire(&id).await;
    let opening = fixture.scan().await.collection_revision;
    let untouched = fixture.scan_for(&fixture.other).await;
    let (first_entered, first_resume) = fixture.objects.gate(&id);
    let (second_entered, second_resume) = fixture.objects.gate(&id);
    let other_host = fixture.reopen().await;
    let (finished, first_done) = flume::bounded(1);
    let first = async {
        let receipt = fixture
            .scope
            .collect_job(&Grant::new(&app), options(1), &fixture.objects)
            .await;
        finished.send_async(()).await.unwrap();
        receipt
    };
    // Plans while the first sweep is held in its object delete, so both pages
    // carry the revision the first sweep read.
    let second = async {
        first_entered.recv_async().await.unwrap();
        other_host
            .collect_job(&Grant::new(&app), options(1), &fixture.objects)
            .await
    };
    let order = async {
        second_entered.recv_async().await.unwrap();
        first_resume.send_async(()).await.unwrap();
        first_done.recv_async().await.unwrap();
        second_resume.send_async(()).await.unwrap();
    };
    let (first, second, ()) = futures::join!(first, second, order);
    assert_eq!(first.unwrap().outcome, JobOutcome::Completed {});
    assert_eq!(second.unwrap().outcome, JobOutcome::Completed {});
    let settled = fixture.scan().await;
    assert_eq!(settled.collection_revision, opening + 1);
    assert!(settled.collection_after_id.is_none() && settled.collection_upper_id.is_none());
    assert_eq!(fixture.scan_for(&fixture.other).await, untouched);
}

/// An unsettled page resumes on the item list its first attempt froze, even when
/// the retry asks for a different page size. Collecting an item takes it out of
/// the query the list was derived from, so a list re-derived at the retry's size
/// starts at a different item than the one the stored cursor counts to.
pub(super) async fn frozen_plan(store: Rc<OrmStore>) {
    let fixture = Fixture::new(store).await;
    let mut old = Vec::new();
    for _ in 0..3 {
        let id = fixture.stage().await;
        fixture.expire(&id).await;
        old.push(id);
    }
    old.sort();
    let (entered, resume) = fixture.objects.gate(&old[1]);
    let grant = Grant::new(fixture.scope.app_id());
    let replace = async {
        entered.recv_async().await.unwrap();
        fixture
            .service
            .policies
            .bind(fixture.scope.app_id().clone())
            .unwrap()
            .begin_refresh()
            .unwrap()
            .install(leased_policy(2, AppPolicy::default()))
            .unwrap();
    };
    let (result, ()) = futures::join!(
        fixture
            .scope
            .collect_job(&grant, options(3), &fixture.objects),
        replace
    );
    assert!(
        matches!(result, Err(WorkflowServiceError::Unavailable(_))),
        "{result:?}"
    );
    assert!(resume.is_disconnected());
    assert_eq!(fixture.objects.deletes(), old[..2]);
    let frozen = fixture.page(&grant.delivery.job).await;
    let plan: serde_json::Value = serde_json::from_str(&frozen.plan).unwrap();
    assert_eq!(plan["ids"], json!(old));
    assert_eq!(plan["more"], json!(false));
    assert_eq!(frozen.next_index, 2);
    // What makes the retry's page size observable: the collected item has left
    // the derivation and the reserved one has not, so a list re-derived at one
    // item opens on `old[1]` while the frozen cursor points at `old[2]`.
    let cutoff = plan["observed_at"].as_i64().unwrap();
    let collected = fixture.payload(&old[0]).await;
    assert_eq!(collected.state, "deleted");
    assert!(collected.expires_at > cutoff, "{collected:?}");
    let reserved = fixture.payload(&old[1]).await;
    assert_eq!(reserved.state, "deleting");
    assert!(reserved.expires_at <= cutoff, "{reserved:?}");
    let reopened = fixture.reopen().await;
    assert_eq!(
        reopened
            .collect_job(&grant.retry(), options(1), &fixture.objects)
            .await
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );
    assert_eq!(fixture.objects.deletes(), old);
    let settled = fixture.page(&grant.delivery.job).await;
    assert_eq!(settled.plan, frozen.plan);
    assert_eq!(settled.next_index, 3);
    assert_eq!(fixture.payload(&old[1]).await, reserved);
    assert!(fixture.exists(fixture.scope.app_id(), &old[1]));
    assert_eq!(fixture.payload(&old[2]).await.state, "deleted");
    assert!(!fixture.exists(fixture.scope.app_id(), &old[2]));
}
