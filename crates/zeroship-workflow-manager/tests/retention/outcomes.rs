use super::*;
use std::{collections::BTreeMap, future::ready};
use zeroship_core::workflow_coordination::{ManagementOutcome, RequestId};

case!(
    sqlite_queue_rejects_cross_family_outcomes_and_preserves_exact_replay,
    postgres_queue_rejects_cross_family_outcomes_and_preserves_exact_replay,
    outcome_families
);

pub async fn refused(
    fixture: &Fixture,
    queue: &Queue,
    authority: &Assignment,
    settlement: &Settlement,
    expected: Error,
) {
    let before = snapshot(fixture).await;
    let authorizations = Cell::new(0);
    let replays = Cell::new(0);
    assert_eq!(
        queue
            .settle_authorized(
                &authority.into(),
                settlement,
                |_| {
                    authorizations.set(authorizations.get() + 1);
                    ready(Ok(authority.clone()))
                },
                |_| {
                    replays.set(replays.get() + 1);
                    ready(Ok(authority.worker_id.clone()))
                },
            )
            .await,
        Err(expected)
    );
    assert_eq!(authorizations.get(), 0);
    assert_eq!(replays.get(), 0);
    assert_eq!(snapshot(fixture).await, before);
}

async fn snapshot(fixture: &Fixture) -> BTreeMap<&'static str, Vec<Value>> {
    let mut result = BTreeMap::new();
    for table in [
        "jobs",
        "queue_scopes",
        "deployment_holds",
        "management",
        "management_scopes",
        "recovery_scopes",
        "recovery_duties",
        "schedule_occurrences",
    ] {
        let Output::Rows { rows, .. } = fixture
            .database()
            .await
            .collection(table)
            .unwrap()
            .find(value!({}), value!({"orderBy":{"id":1},"limit":256}))
            .await
            .unwrap()
        else {
            panic!("expected settlement metadata")
        };
        result.insert(table, rows);
    }
    result
}

async fn outcome_families(fixture: &Fixture) {
    let faults = FaultClient::new(support::synthetic_holds());
    let queue = queue(fixture, faults.clone()).await;
    for outcome in [
        JobOutcome::Completed {},
        JobOutcome::Waiting {},
        JobOutcome::Rejected {},
    ] {
        let app = AppId::mint();
        queue.register_scope(&app).await.unwrap();
        let authority = assignment(&app);
        let spec = job(&app, &DeploymentId::mint());
        queue.submit(&spec).await.unwrap();
        let granted = queue.claim(&authority).await.unwrap().unwrap();
        assert_eq!(granted.delivery().job, spec);
        let acquired = faults.acquired.get();
        assert!(acquired > 0);
        let released = faults.released.get();
        let successor = JobSpec {
            id: JobId::mint(),
            app_id: app.clone(),
            operation: JobOperation::Collect {},
            available_at: 0.try_into().unwrap(),
        };
        let mut attempt = Settlement {
            delivery: granted.delivery().clone(),
            outcome: JobOutcome::Management {
                outcome: ManagementOutcome::Conflict {},
            },
            successors: vec![job(&app, &DeploymentId::mint())],
        };
        refused(fixture, &queue, &authority, &attempt, Error::Invalid).await;
        attempt.outcome = outcome;
        attempt.successors = vec![successor.clone()];
        let receipt = queue.settle(&authority, &attempt).await.unwrap();
        assert_eq!(receipt.outcome, outcome);
        assert_eq!(queue.settle(&authority, &attempt).await.unwrap(), receipt);
        let stored = rows(fixture, "jobs", value!({"id":spec.id.as_str()})).await;
        let encoded: serde_json::Value =
            serde_json::from_str(stored[0]["outcome"].as_str().unwrap()).unwrap();
        assert!(encoded.is_object());
        assert_eq!(
            serde_json::from_value::<JobOutcome>(encoded).unwrap(),
            outcome
        );

        let mut changed = attempt.clone();
        changed.outcome = if matches!(outcome, JobOutcome::Waiting {}) {
            JobOutcome::Completed {}
        } else {
            JobOutcome::Waiting {}
        };
        refused(fixture, &queue, &authority, &changed, Error::Conflict).await;
        changed.outcome = JobOutcome::Management {
            outcome: ManagementOutcome::Denied {},
        };
        refused(fixture, &queue, &authority, &changed, Error::Invalid).await;
        changed = attempt.clone();
        changed.successors = vec![job(&app, &DeploymentId::mint())];
        refused(fixture, &queue, &authority, &changed, Error::Conflict).await;
        assert_eq!(queue.settle(&authority, &attempt).await.unwrap(), receipt);
        finish(&queue, &authority, &successor).await;
        assert_eq!(faults.acquired.get(), acquired);
        assert_eq!(faults.released.get(), released);
    }

    reject_before_lookup(fixture, &queue).await;
}

async fn reject_before_lookup(fixture: &Fixture, queue: &Queue) {
    let unknown = AppId::mint();
    let authority = assignment(&unknown);
    let mut spec = job(&unknown, &DeploymentId::mint());
    spec.operation = JobOperation::Management {
        request_id: RequestId::mint(),
        run_id: RunId::mint(),
        revision: 1.try_into().unwrap(),
        command: zeroship_core::workflow_jobs::ManagementCommand::RestartStarted { from: None },
    };
    let attempt = Settlement {
        delivery: zeroship_core::workflow_jobs::Delivery {
            job: spec,
            worker_id: authority.worker_id.clone(),
            assignment_revision: authority.revision,
            attempt: 1.try_into().unwrap(),
            deadline: authority.expires_at,
        },
        outcome: JobOutcome::Completed {},
        successors: vec![],
    };
    // Outcome-family validation precedes even app registration and delivery lookup.
    refused(fixture, queue, &authority, &attempt, Error::Invalid).await;
}
