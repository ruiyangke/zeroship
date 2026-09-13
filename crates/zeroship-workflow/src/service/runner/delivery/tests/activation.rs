use super::*;
use zeroship_core::workflow_jobs::{DeploymentId, JobId, JobOperation};

async fn journal_count(fixture: &Fixture, table: &str) -> i64 {
    let tx = fixture.service.begin().await.unwrap();
    let Output::Count(count) = tx
        .database()
        .collection(&format!("__zeroship_workflow_{table}"))
        .unwrap()
        .count(value!({"app_id":fixture.app.app_id().as_str()}), value!({}))
        .await
        .unwrap()
    else {
        panic!("expected journal count")
    };
    tx.commit().await.unwrap();
    count
}

#[compio::test]
async fn activation_lost_ack_and_redelivery_never_start_the_executor() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    let deployment = fixture.deployments.deploy(fixture.app.app_id()).await;
    let mut lease = fixture.lease.clone();
    lease.delivery.job.id = JobId::mint();
    lease.delivery.job.deployment_id = DeploymentId::parse(&deployment.id).unwrap();
    lease.delivery.job.operation = JobOperation::Activate {
        revision: Revision::try_from(1).unwrap(),
    };
    let job = lease.delivery.job.clone();
    let mut original = BTreeMap::new();
    for table in ["runs", "tasks", "job_publications"] {
        original.insert(table, journal_count(&fixture, table).await);
    }
    fixture.metadata.lose_ack.set(true);
    let mut slot = fixture.slot(Duration::from_secs(5));
    let DeliveryOutcome::Settled { creator, manager } =
        Box::pin(slot.run(&fixture.app, lease.clone()))
            .await
            .unwrap()
    else {
        panic!("activation must settle its readiness receipt")
    };
    assert_eq!(creator.job, job);
    assert_eq!(creator.outcome, JobOutcome::Completed);
    assert_eq!(manager.job_id, job.id);
    assert_eq!(manager.outcome, JobOutcome::Completed);
    {
        let requests = fixture.metadata.requests.borrow();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0], requests[1]);
        assert!(requests[0].successors.is_empty());
    }
    fixture
        .deployments
        .assert_held(fixture.app.app_id(), &deployment.id)
        .await;
    fixture
        .deployments
        .source
        .delete_manifest(fixture.app.app_id(), &deployment.hash)
        .await
        .unwrap();
    drop(slot);
    lease.delivery.attempt = Revision::try_from(2).unwrap();
    lease.expires = Instant::now();
    let mut restarted = fixture.slot(Duration::from_secs(5));
    let DeliveryOutcome::Settled {
        creator: replayed,
        manager,
    } = Box::pin(restarted.run(&fixture.app, lease.clone()))
        .await
        .unwrap()
    else {
        panic!("committed activation must replay despite missing artifact")
    };
    assert_eq!(replayed, creator);
    assert_eq!(manager.attempt, lease.delivery.attempt);
    assert_eq!(fixture.metadata.requests.borrow().len(), 3);
    assert_eq!(fixture.probe.starts.get(), 0);
    assert_eq!(fixture.probe.stops.get(), 0);
    assert_eq!(fixture.probe.cancels.get(), 0);
    assert_eq!(fixture.metadata.renewals.get(), 0);
    assert_eq!(fixture.app.job_receipt(&job).await.unwrap(), Some(*creator));
    for (table, count) in original {
        assert_eq!(journal_count(&fixture, table).await, count);
    }
}
