#![expect(
    clippy::future_not_send,
    reason = "fanout tests use native compio creator journals"
)]

use super::*;
use crate::service::{fanout::FanoutOptions, AppWorkflows, WorkerIdentity};
use std::time::{Duration, Instant};
use zeroship_core::{
    workflow_coordination::WorkerId,
    workflow_jobs::{Delivery, JobLease, JobOperation, JobOutcome, JobSpec},
};

mod failures;
mod ordering;
mod retention;
mod rollback;

#[derive(Clone)]
pub(super) struct Grant {
    pub delivery: Delivery,
    pub expires: Instant,
}
impl Grant {
    pub fn new(job: &JobSpec) -> Self {
        assert!(matches!(job.operation, JobOperation::Fanout { .. }));
        Self {
            delivery: Delivery {
                job: job.clone(),
                worker_id: WorkerId::mint(),
                assignment_revision: 1.try_into().unwrap(),
                attempt: 1.try_into().unwrap(),
                deadline: 0.try_into().unwrap(),
            },
            expires: Instant::now() + Duration::from_secs(30),
        }
    }
    fn retry(&self) -> Self {
        let mut retry = self.clone();
        retry.delivery.attempt = (retry.delivery.attempt.get() + 1).try_into().unwrap();
        retry.expires = Instant::now() + Duration::from_secs(30);
        retry
    }
}
impl JobLease for Grant {
    fn delivery(&self) -> &Delivery {
        &self.delivery
    }
    fn remaining(&self) -> Option<Duration> {
        self.expires
            .checked_duration_since(Instant::now())
            .filter(|value| !value.is_zero())
    }
}

async fn jobs(scope: &AppWorkflows) -> Vec<JobSpec> {
    let mut after = None;
    let mut jobs = Vec::new();
    loop {
        let page = scope.pending_jobs(after.as_ref(), 100).await.unwrap();
        if page.is_empty() {
            return jobs;
        }
        after = page.last().map(|job| job.id.clone());
        jobs.extend(
            page.into_iter()
                .filter(|job| matches!(job.operation, JobOperation::Fanout { .. })),
        );
    }
}

pub(super) async fn job(scope: &AppWorkflows, broadcast: &str, page: i64) -> JobSpec {
    jobs(scope).await.into_iter().find(|job| matches!(&job.operation, JobOperation::Fanout { broadcast_id, revision } if broadcast_id.as_str() == broadcast && revision.get() == page)).expect("persisted explicit fanout publication")
}

pub(super) async fn deliver_topic_page(scope: &AppWorkflows) -> usize {
    for job in jobs(scope).await {
        if scope.job_receipt(&job).await.unwrap().is_some() {
            continue;
        }
        let before = count_signals(scope).await;
        if scope
            .fanout_job(&Grant::new(&job), FanoutOptions::default())
            .await
            .unwrap()
            .is_some()
        {
            return usize::try_from(count_signals(scope).await - before).unwrap();
        }
    }
    0
}

async fn count_signals(scope: &AppWorkflows) -> i64 {
    let tx = scope.service.begin().await.unwrap();
    let count = journal_count(&tx, "signals", json!({"app_id":scope.app_id().as_str()})).await;
    tx.commit().await.unwrap();
    count
}

async fn broadcast(scope: &AppWorkflows, payload: &str) -> crate::service::AcceptedBroadcast {
    scope
        .broadcast(
            &RequestId::mint(),
            "updates",
            SignalOptions {
                signal_type: "news".into(),
                payload: json!(payload),
            },
        )
        .await
        .unwrap()
}

async fn snapshot(scope: &AppWorkflows) -> Vec<Vec<zeroship_data_orm::Value>> {
    let tx = scope.service.begin().await.unwrap();
    let mut rows = Vec::new();
    for table in [
        "app_state",
        "broadcasts",
        "signals",
        "job_publications",
        "advance_publications",
        "fanout_publications",
        "propagation_publications",
        "job_receipts",
        "fanout_pages",
        "topics",
        "runs",
    ] {
        rows.push(
            journal_rows(&tx, table, json!({"app_id":scope.app_id().as_str()}))
                .await
                .into_iter()
                .map(|row| row.0)
                .collect(),
        );
    }
    tx.commit().await.unwrap();
    rows
}

macro_rules! paired {
    ($sqlite:ident, $postgres:ident, $case:path) => {
        #[compio::test]
        async fn $sqlite() {
            let directory = tempfile::tempdir().unwrap();
            Box::pin($case(Rc::new(
                sqlite_store(&directory.path().join("zs-workflow.sqlite")).await,
            )))
            .await;
        }
        #[compio::test]
        async fn $postgres() {
            let fixture = PostgresFixture::start().await;
            Box::pin($case(Rc::new(fixture.store.clone()))).await;
        }
    };
}
paired!(
    sqlite_fanout_defers_reordered_broadcast_and_replays_pages,
    postgres_fanout_defers_reordered_broadcast_and_replays_pages,
    ordering::pages
);
paired!(
    sqlite_signal_materialization_order_survives_clock_regression,
    postgres_signal_materialization_order_survives_clock_regression,
    ordering::signals
);
paired!(
    sqlite_fanout_rejects_damaged_publication_and_historical_page,
    postgres_fanout_rejects_damaged_publication_and_historical_page,
    failures::damage
);
paired!(
    sqlite_fanout_original_policy_expiry_rolls_back_after_refresh,
    postgres_fanout_original_policy_expiry_rolls_back_after_refresh,
    failures::expiry
);
paired!(
    sqlite_fanout_counter_exhaustion_rolls_back_all_effects,
    postgres_fanout_counter_exhaustion_rolls_back_all_effects,
    failures::counters
);
paired!(
    sqlite_pending_publication_validation_fences_code_release,
    postgres_pending_publication_validation_fences_code_release,
    retention::projection
);
