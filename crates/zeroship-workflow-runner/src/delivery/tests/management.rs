use super::*;
use zeroship_core::{
    workflow_coordination::{ManagementOutcome, RunOperation},
    workflow_jobs::{JobId, ManagementCommand},
};

#[compio::test]
async fn management_lost_ack_replays_without_executing_app_code() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    let JobOperation::Advance { run_id, .. } = &fixture.job.operation else {
        panic!("advance fixture");
    };
    let mut lease = fixture.lease.clone();
    lease.delivery.job.id = JobId::mint();
    lease.delivery.job.operation = JobOperation::Management {
        request_id: RequestId::mint(),
        run_id: run_id.clone(),
        revision: 1.try_into().unwrap(),
        command: ManagementCommand::Transition {
            operation: RunOperation::Pause,
        },
    };
    fixture.metadata.lose_ack.set(true);
    let mut slot = fixture.slot(Duration::from_secs(5));
    let DeliveryOutcome::Settled { creator, manager } =
        Box::pin(slot.run(&fixture.app, lease.clone()))
            .await
            .unwrap()
    else {
        panic!("management must settle the lifecycle result");
    };
    let expected = JobOutcome::Management {
        outcome: ManagementOutcome::Applied {
            state: RunState::Paused,
        },
    };
    assert_eq!(creator.outcome, expected);
    assert_eq!(manager.outcome, expected);
    assert_eq!(fixture.metadata.requests.borrow().len(), 2);
    assert_eq!(
        fixture.metadata.requests.borrow()[0],
        fixture.metadata.requests.borrow()[1]
    );
    lease.delivery.attempt = 2.try_into().unwrap();
    lease.expires = Instant::now();
    let DeliveryOutcome::Settled {
        creator: replay,
        manager,
    } = Box::pin(slot.run(&fixture.app, lease.clone()))
        .await
        .unwrap()
    else {
        panic!("management must replay the receipt");
    };
    assert_eq!(creator, replay);
    assert_eq!(manager.attempt, lease.delivery.attempt);
    assert_eq!(
        fixture.app.job_receipt(&lease.delivery.job).await.unwrap(),
        Some(*creator)
    );
    assert_eq!(fixture.probe.starts.get(), 0);
    assert_eq!(fixture.probe.stops.get(), 0);
    assert_eq!(fixture.metadata.renewals.get(), 0);
    assert!(fixture
        .metadata
        .requests
        .borrow()
        .iter()
        .all(|request| request.successors.is_empty()));
}

#[compio::test]
async fn management_denial_settles_without_executor_or_lifecycle_mutation() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    let JobOperation::Advance { run_id, .. } = &fixture.job.operation else {
        panic!("advance fixture");
    };
    fixture
        .service
        .fixture_install(
            fixture.app.app_id(),
            PolicySnapshot::configuration(
                2.try_into().unwrap(),
                AppPolicy {
                    admission: false,
                    ..Default::default()
                },
            )
            .unwrap(),
        )
        .unwrap();
    let before = run_state(&fixture, run_id.as_str()).await;
    let mut lease = fixture.lease.clone();
    lease.delivery.job.id = JobId::mint();
    lease.delivery.job.operation = JobOperation::Management {
        request_id: RequestId::mint(),
        run_id: run_id.clone(),
        revision: 1.try_into().unwrap(),
        command: ManagementCommand::RestartStarted { from: None },
    };
    let mut slot = fixture.slot(Duration::from_secs(5));
    let DeliveryOutcome::Settled { creator, manager } =
        Box::pin(slot.run(&fixture.app, lease)).await.unwrap()
    else {
        panic!("denial must settle");
    };
    let expected = JobOutcome::Management {
        outcome: ManagementOutcome::Denied {},
    };
    assert_eq!(creator.outcome, expected);
    assert_eq!(manager.outcome, expected);
    assert_eq!(fixture.probe.starts.get(), 0);
    assert_eq!(fixture.metadata.renewals.get(), 0);
    assert_eq!(run_state(&fixture, run_id.as_str()).await, before);
}

async fn run_state(fixture: &Fixture, run_id: &str) -> (String, i64) {
    let tx = fixture.service.begin().await.unwrap();
    let Output::Rows { rows, .. } = tx
        .database()
        .collection("__zeroship_workflow_runs")
        .unwrap()
        .find(
            value!({"app_id":fixture.app.app_id().as_str(),"id":run_id}),
            value!({}),
        )
        .await
        .unwrap()
    else {
        panic!("expected run rows");
    };
    assert_eq!(rows.len(), 1);
    let result = (
        rows[0]["state"].as_str().unwrap().to_owned(),
        rows[0]["generation"].as_i64().unwrap(),
    );
    tx.commit().await.unwrap();
    result
}
