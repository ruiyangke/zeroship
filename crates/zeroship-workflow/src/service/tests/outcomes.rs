#![expect(
    clippy::future_not_send,
    reason = "receipt contracts use native compio journals"
)]

use super::*;
use crate::service::{
    delivery::{self, JobAcceptance, JobReceipt},
    publication::JobPublisher,
    reconciliation::ReconciliationOptions,
    AppWorkflows, IntervalAnchor, ScheduleOverlap, ScheduleRegistration, ScheduleTiming,
};
use std::{
    cell::Cell,
    collections::BTreeMap,
    time::{Duration, Instant},
};
use zeroship_core::{
    workflow_coordination::{ManagementOutcome, RunId, WorkerId},
    workflow_jobs::{Delivery, DeploymentId, JobId, JobLease, JobOperation, JobOutcome, JobSpec},
    workflow_schedules::ScheduleId,
};
use zeroship_data_orm::Value;

mod fixture;
use fixture::{Case, Fixture, Lease};

macro_rules! paired {
    ($sqlite:ident, $postgres:ident, $contract:ident) => {
        #[compio::test]
        async fn $sqlite() {
            let directory = tempfile::tempdir().unwrap();
            Box::pin($contract(Rc::new(
                sqlite_store(&directory.path().join("creator.sqlite")).await,
            )))
            .await;
        }

        #[compio::test]
        async fn $postgres() {
            let fixture = PostgresFixture::start().await;
            Box::pin($contract(Rc::new(fixture.store.clone()))).await;
        }
    };
}

paired!(
    sqlite_job_receipt_outcomes_reject_corruption_and_recover_after_repair,
    postgres_job_receipt_outcomes_reject_corruption_and_recover_after_repair,
    stored_outcomes
);
paired!(
    sqlite_job_finish_refuses_cross_family_before_writing,
    postgres_job_finish_refuses_cross_family_before_writing,
    producer
);

const fn management() -> JobOutcome {
    JobOutcome::Management {
        outcome: ManagementOutcome::Denied {},
    }
}

type Snapshot = BTreeMap<&'static str, Vec<Value>>;

async fn snapshot(tx: &Transaction, app: &AppId) -> Snapshot {
    let mut result = BTreeMap::new();
    for table in [
        "runs",
        "tasks",
        "generations",
        "outbox",
        "job_receipts",
        "job_publications",
        "occurrences",
        "activations",
        "reconciliation_scans",
        "deployment_holds",
        "management_receipts",
    ] {
        let filter = if table == "reconciliation_scans" {
            json!({"id":app.as_str()})
        } else {
            json!({"app_id":app.as_str()})
        };
        result.insert(
            table,
            journal_rows(tx, table, filter)
                .await
                .into_iter()
                .map(|row| row.0)
                .collect(),
        );
    }
    result.insert(
        "activation_scopes",
        journal_rows(tx, "activation_scopes", json!({"id":app.as_str()}))
            .await
            .into_iter()
            .map(|row| row.0)
            .collect(),
    );
    result
}

async fn persisted(service: &WorkflowService, app: &AppId) -> Snapshot {
    let tx = service.begin().await.unwrap();
    let result = snapshot(&tx, app).await;
    tx.commit().await.unwrap();
    result
}

async fn replace_outcome(service: &WorkflowService, job: &JobSpec, outcome: serde_json::Value) {
    let tx = service.begin().await.unwrap();
    journal_update(
        &tx,
        "job_receipts",
        json!({"app_id":job.app_id.as_str(),"id":job.id.as_str()}),
        json!({"outcome":serde_json::to_string(&outcome).unwrap()}),
    )
    .await;
    tx.commit().await.unwrap();
}

async fn stored_outcomes(store: Rc<OrmStore>) {
    let fixture = Box::pin(Fixture::new(store)).await;
    let original = persisted(&fixture.service, fixture.app.app_id()).await;
    assert!(!original["job_receipts"].is_empty());
    assert!(!original["tasks"].is_empty());
    for case in &fixture.cases {
        let mut forbidden = vec![management()];
        forbidden.extend(case.forbidden.iter().copied());
        for outcome in forbidden {
            let fabricated = JobReceipt {
                job: case.lease.delivery.job.clone(),
                outcome,
            };
            assert!(matches!(
                fabricated.settlement(&case.lease),
                Err(WorkflowServiceError::Internal(_))
            ));
            assert_corruption(&fixture, case, serde_json::to_value(outcome).unwrap()).await;
        }
        for outcome in &case.wrong_linkage {
            assert_corruption(&fixture, case, serde_json::to_value(outcome).unwrap()).await;
        }
        assert_corruption(&fixture, case, json!("completed")).await;
        replace_outcome(
            &fixture.service,
            &case.receipt.job,
            serde_json::to_value(case.receipt.outcome).unwrap(),
        )
        .await;
        assert_eq!(
            fixture.app.job_receipt(&case.receipt.job).await.unwrap(),
            Some(case.receipt.clone())
        );
        assert_eq!(Box::pin(case.replay(&fixture)).await.unwrap(), case.receipt);
        assert_eq!(
            case.receipt.settlement(&case.lease).unwrap().outcome,
            case.receipt.outcome
        );
        assert_eq!(
            persisted(&fixture.service, fixture.app.app_id()).await,
            original
        );
    }
}

async fn assert_corruption(fixture: &Fixture, case: &Case, outcome: serde_json::Value) {
    replace_outcome(&fixture.service, &case.receipt.job, outcome).await;
    let damaged = persisted(&fixture.service, fixture.app.app_id()).await;
    let calls = fixture.publisher.calls.get();
    assert!(fixture.app.job_receipt(&case.receipt.job).await.is_err());
    assert!(Box::pin(case.replay(fixture)).await.is_err());
    assert_eq!(fixture.publisher.calls.get(), calls);
    assert_eq!(
        persisted(&fixture.service, fixture.app.app_id()).await,
        damaged
    );
}

async fn producer(store: Rc<OrmStore>) {
    let (service, app, _, _platform) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let lease = Lease::job(scope.pending_jobs(None, 1).await.unwrap().remove(0));
    let JobAcceptance::Execute(task) = scope.accept_job(&lease).await.unwrap() else {
        panic!("advance must retain a pending execution receipt")
    };
    assert!(scope
        .job_receipt(&lease.delivery.job)
        .await
        .unwrap()
        .is_none());
    let original = persisted(&service, &app).await;
    let mut tx = scope.service.begin().await.unwrap();
    crate::service::app::lock_app(&mut tx, &app).await.unwrap();
    let before = snapshot(&tx, &app).await;
    let now = tx.now().await.unwrap();
    assert!(matches!(
        delivery::finish(&tx, &lease.delivery.job, management(), now).await,
        Err(WorkflowServiceError::Internal(_))
    ));
    assert_eq!(snapshot(&tx, &app).await, before);
    drop(tx);
    assert_eq!(persisted(&service, &app).await, original);
    let completed = scope
        .complete_job(&task, &lease, execution(json!([{"kind":"RunCompleted"}])))
        .await
        .unwrap();
    assert_eq!(completed.outcome, JobOutcome::Completed {});
    assert_eq!(
        scope.job_receipt(&lease.delivery.job).await.unwrap(),
        Some(completed)
    );
}
