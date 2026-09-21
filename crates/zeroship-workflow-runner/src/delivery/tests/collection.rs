use super::*;
use zeroship_core::workflow_jobs::JobId;
pub(super) async fn has_task(fixture: &Fixture) -> bool {
    let tx = fixture.service.begin().await.unwrap();
    let Output::Count(count) = tx
        .database()
        .collection("__zeroship_workflow_tasks")
        .unwrap()
        .count(value!({"app_id":fixture.app.app_id().as_str()}), value!({}))
        .await
        .unwrap()
    else {
        panic!("expected journal count")
    };
    tx.commit().await.unwrap();
    count > 0
}

#[compio::test]
async fn collect_lost_ack_replays_without_executor_or_artifacts() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    assert!(!has_task(&fixture).await);
    let before = fixture.app.pending_jobs(None, 1).await.unwrap();
    assert_eq!(before, std::slice::from_ref(&fixture.job));
    let mut lease = fixture.lease.clone();
    lease.delivery.job.id = JobId::mint();
    lease.delivery.job.operation = JobOperation::Collect {};
    fixture.metadata.lose_ack.set(true);
    let mut slot = fixture.slot(Duration::from_secs(5));
    let DeliveryOutcome::Settled { creator, manager } =
        Box::pin(slot.run(&fixture.app, lease.clone()))
            .await
            .unwrap()
    else {
        panic!("collection settles a bounded empty page")
    };
    assert_eq!(creator.outcome, JobOutcome::Completed {});
    assert_eq!(manager.outcome, JobOutcome::Completed {});
    assert_eq!(fixture.metadata.requests.borrow().len(), 2);
    assert_eq!(
        fixture.metadata.requests.borrow()[0],
        fixture.metadata.requests.borrow()[1]
    );
    lease.delivery.attempt = 2.try_into().unwrap();
    lease.expires = Instant::now();
    let DeliveryOutcome::Settled {
        creator: replay, ..
    } = Box::pin(slot.run(&fixture.app, lease.clone()))
        .await
        .unwrap()
    else {
        panic!("collection must replay its receipt")
    };
    assert_eq!(creator, replay);
    assert_eq!(
        fixture.app.job_receipt(&lease.delivery.job).await.unwrap(),
        Some(*creator)
    );
    assert_eq!(fixture.probe.starts.get(), 0);
    assert_eq!(fixture.probe.stops.get(), 0);
    assert_eq!(fixture.metadata.renewals.get(), 0);
    assert!(!has_task(&fixture).await);
    assert_eq!(fixture.app.pending_jobs(None, 1).await.unwrap(), before);
    assert!(fixture
        .metadata
        .requests
        .borrow()
        .iter()
        .all(|request| request.successors.is_empty()));
}

/// Collection bounds a slot cannot honour are refused where they are declared,
/// and a refused duty leaves no receipt and no settlement, so the manager
/// redelivers it rather than treating it as discharged.
#[compio::test]
async fn collect_invalid_bounds_are_refused_and_do_not_acknowledge() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    let mut lease = fixture.lease.clone();
    lease.delivery.job.id = JobId::mint();
    lease.delivery.job.operation = JobOperation::Collect {};
    for invalid in [
        CollectionOptions {
            page_size: 0,
            ..Default::default()
        },
        CollectionOptions {
            item_timeout: Duration::ZERO,
            ..Default::default()
        },
        CollectionOptions {
            page_size: u32::MAX,
            ..Default::default()
        },
    ] {
        assert!(matches!(
            fixture.slot_collecting(Duration::from_secs(5), invalid),
            Err(WorkflowServiceError::InvalidRequest(_))
        ));
    }
    assert!(fixture
        .app
        .job_receipt(&lease.delivery.job)
        .await
        .unwrap()
        .is_none());
    for invalid in [
        CollectionOptions {
            page_size: 0,
            ..Default::default()
        },
        CollectionOptions {
            item_timeout: Duration::ZERO,
            ..Default::default()
        },
        CollectionOptions {
            page_size: u32::MAX,
            ..Default::default()
        },
    ] {
        assert!(matches!(
            fixture
                .app
                .payloads(&fixture.objects)
                .collect_job(&lease, invalid)
                .await,
            Err(WorkflowServiceError::InvalidRequest(_))
        ));
    }
    assert_eq!(fixture.probe.starts.get(), 0);
    assert!(fixture.metadata.requests.borrow().is_empty());
}
