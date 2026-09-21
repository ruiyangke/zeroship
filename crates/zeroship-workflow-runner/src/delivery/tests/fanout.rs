use super::*;

pub(super) async fn accepted(fixture: &Fixture) -> JobSpec {
    let broadcast = fixture
        .app
        .broadcast(
            &RequestId::mint(),
            "updates",
            zeroship_workflow::operations::SignalOptions {
                signal_type: "news".into(),
                payload: json!("accepted"),
            },
        )
        .await
        .unwrap();
    fixture.app.pending_jobs(None, 100).await.unwrap().into_iter().find(|job| matches!(&job.operation, JobOperation::Fanout { broadcast_id, .. } if broadcast_id.as_str() == broadcast.id)).expect("committed Fanout publication")
}

#[compio::test]
async fn fanout_lost_ack_replays_without_executor_or_storage() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    let mut lease = fixture.lease.clone();
    lease.delivery.job = accepted(&fixture).await;
    let pending = fixture.app.pending_jobs(None, 100).await.unwrap();
    fixture.metadata.lose_ack.set(true);
    let mut slot = fixture.slot(Duration::from_secs(5));
    let DeliveryOutcome::Settled { creator, manager } =
        Box::pin(slot.run(&fixture.app, lease.clone()))
            .await
            .unwrap()
    else {
        panic!("native fanout settles without execution")
    };
    assert_eq!(creator.outcome, JobOutcome::Completed {});
    assert_eq!(manager.outcome, creator.outcome);
    assert_eq!(fixture.metadata.requests.borrow().len(), 2);
    assert_eq!(
        fixture.metadata.requests.borrow()[0],
        fixture.metadata.requests.borrow()[1]
    );
    lease.delivery.attempt = 2.try_into().unwrap();
    lease.expires = Instant::now();
    let DeliveryOutcome::Settled {
        creator: replay, ..
    } = Box::pin(slot.run(&fixture.app, lease)).await.unwrap()
    else {
        panic!("committed fanout replay")
    };
    assert_eq!(creator, replay);
    assert_eq!(fixture.probe.starts.get(), 0);
    assert_eq!(fixture.probe.stops.get(), 0);
    assert_eq!(fixture.metadata.renewals.get(), 0);
    assert!(!super::collection::has_task(&fixture).await);
    assert_eq!(fixture.app.pending_jobs(None, 100).await.unwrap(), pending);
    assert!(fixture
        .metadata
        .requests
        .borrow()
        .iter()
        .all(|request| request.successors.is_empty()));
}

#[compio::test]
async fn fanout_later_broadcast_defers_without_settlement() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    let first = accepted(&fixture).await;
    let mut lease = fixture.lease.clone();
    lease.delivery.job = accepted(&fixture).await;
    let mut slot = fixture.slot(Duration::from_secs(5));
    assert!(matches!(
        Box::pin(slot.run(&fixture.app, lease.clone()))
            .await
            .unwrap(),
        DeliveryOutcome::Deferred
    ));
    assert!(fixture.metadata.requests.borrow().is_empty());
    assert!(fixture
        .app
        .job_receipt(&lease.delivery.job)
        .await
        .unwrap()
        .is_none());
    let mut predecessor = fixture.lease.clone();
    predecessor.delivery.job = first;
    fixture
        .app
        .fanout_job(
            &predecessor,
            zeroship_workflow::service::fanout::FanoutOptions::default(),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        Box::pin(slot.run(&fixture.app, lease)).await.unwrap(),
        DeliveryOutcome::Settled { .. }
    ));
    assert_eq!(fixture.probe.starts.get(), 0);
}
