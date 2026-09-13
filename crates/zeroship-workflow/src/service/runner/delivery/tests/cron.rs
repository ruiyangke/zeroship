use super::*;
use crate::service::{IntervalAnchor, ScheduleRegistration, ScheduleTiming};
use zeroship_core::{
    workflow_coordination::RunId,
    workflow_jobs::{DeploymentId, JobId},
    workflow_schedules::ScheduleId,
};

#[compio::test]
async fn cron_lost_ack_and_redelivery_publish_once_without_starting_executor() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    let registration = DeployRegistration {
        id: typed_id::generate("dep"),
        hash: String::new(),
        workflows: ["Example".into()].into(),
        schedules: vec![ScheduleRegistration {
            name: "periodic".into(),
            workflow_name: "Example".into(),
            schedule: ScheduleTiming::Interval {
                interval_ms: 60_000,
                anchor: IntervalAnchor::Epoch,
            },
            input: json!({"scheduled":true}),
            overlap: Default::default(),
            catch_up: Default::default(),
        }],
    };
    let deploy = fixture
        .deployments
        .publish(
            fixture.app.app_id(),
            &registration,
            &deployments::Sources::default(),
        )
        .await
        .unwrap();
    let mut lease = fixture.lease.clone();
    lease.delivery.job.id = JobId::mint();
    lease.delivery.job.deployment_id = DeploymentId::parse(&deploy.id).unwrap();
    lease.delivery.job.operation = JobOperation::Activate {
        revision: 1.try_into().unwrap(),
    };
    fixture.app.activate_job(&lease).await.unwrap();
    let before = fixture.app.pending_jobs(None, 10).await.unwrap();
    let run_id = RunId::mint();
    lease.delivery.job.id = JobId::mint();
    lease.delivery.job.operation = JobOperation::Cron {
        schedule_id: ScheduleId::mint(),
        schedule_name: "periodic".into(),
        request_id: RequestId::mint(),
        run_id: run_id.clone(),
        revision: 1.try_into().unwrap(),
        scheduled_at: 1000.try_into().unwrap(),
    };
    fixture.metadata.lose_ack.set(true);
    let mut slot = fixture.slot(Duration::from_secs(5));
    let DeliveryOutcome::Settled { creator, manager } =
        Box::pin(slot.run(&fixture.app, lease.clone()))
            .await
            .unwrap()
    else {
        panic!("cron acceptance must settle its receipt");
    };
    assert_eq!(creator.outcome, JobOutcome::Completed);
    assert_eq!(manager.job_id, lease.delivery.job.id);
    let publications = fixture.app.pending_jobs(None, 10).await.unwrap();
    assert_eq!(publications.len(), before.len() + 1);
    assert!(publications.iter().any(|job| matches!(&job.operation, JobOperation::Advance { run_id: id, generation: 0, .. } if id == &run_id)));
    {
        let requests = fixture.metadata.requests.borrow();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0], requests[1]);
        assert!(requests[0].successors.is_empty());
    }
    fixture
        .deployments
        .source
        .delete_manifest(fixture.app.app_id(), &deploy.hash)
        .await
        .unwrap();
    lease.delivery.attempt = 2.try_into().unwrap();
    lease.expires = Instant::now();
    drop(slot);
    let mut slot = fixture.slot(Duration::from_secs(5));
    let DeliveryOutcome::Settled {
        creator: replayed, ..
    } = Box::pin(slot.run(&fixture.app, lease)).await.unwrap()
    else {
        panic!("lost cron ACK must replay the retained outcome");
    };
    assert_eq!(replayed, creator);
    assert_eq!(
        fixture.app.pending_jobs(None, 10).await.unwrap(),
        publications
    );
    assert_eq!(fixture.probe.starts.get(), 0);
    assert_eq!(fixture.probe.cancels.get(), 0);
    assert_eq!(fixture.probe.stops.get(), 0);
    assert_eq!(fixture.metadata.renewals.get(), 0);
}
