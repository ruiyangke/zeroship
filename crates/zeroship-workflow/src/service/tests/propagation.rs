#![expect(
    clippy::future_not_send,
    reason = "propagation tests use native compio creator journals"
)]

use super::{graph, *};
use crate::{
    operations::{RunOperation, RunState},
    service::{
        app, delivery::JobReceipt, propagation::PropagationOptions, store::Row, AppWorkflows,
        WorkerIdentity,
    },
};
use std::time::Instant;
use zeroship_core::workflow_jobs::{JobLease, JobOperation, JobOutcome, JobSpec};
use zeroship_data_orm::sql::MAX_ROW_LIMIT;

mod cascade;
mod corruption;
mod fence;
mod notify;
mod rollback;

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
    sqlite_cascade_pages_past_the_row_limit_and_replay_exactly,
    postgres_cascade_pages_past_the_row_limit_and_replay_exactly,
    cascade::pages
);
paired!(
    sqlite_delayed_cascade_page_leaves_a_restarted_source_untouched,
    postgres_delayed_cascade_page_leaves_a_restarted_source_untouched,
    cascade::restarted_source
);
paired!(
    sqlite_notify_pages_past_the_row_limit_wake_each_parent_once,
    postgres_notify_pages_past_the_row_limit_wake_each_parent_once,
    notify::pages
);
paired!(
    sqlite_delayed_notify_page_after_head_restart_is_superseded,
    postgres_delayed_notify_page_after_head_restart_is_superseded,
    notify::superseded
);
paired!(
    sqlite_fence_cancels_continuation_and_children_created_mid_propagation,
    postgres_fence_cancels_continuation_and_children_created_mid_propagation,
    fence::mid_propagation
);
paired!(
    sqlite_restart_waits_for_propagation_and_then_overrides_it,
    postgres_restart_waits_for_propagation_and_then_overrides_it,
    fence::restart
);
paired!(
    sqlite_delivered_renewal_reports_a_fenced_cancellation,
    postgres_delivered_renewal_reports_a_fenced_cancellation,
    fence::delivered_renewal
);
paired!(
    sqlite_damaged_propagation_journal_fails_closed_without_effects,
    postgres_damaged_propagation_journal_fails_closed_without_effects,
    corruption::damage
);

/// Seed runs in batches so fixture setup stays within one transaction's bound.
async fn seed_runs(
    service: &WorkflowService,
    app: &AppId,
    name: &str,
    count: usize,
) -> Vec<String> {
    let mut runs = Vec::with_capacity(count);
    while runs.len() < count {
        let mut tx = service.begin().await.unwrap();
        app::lock_app(&mut tx, app).await.unwrap();
        for _ in 0..(count - runs.len()).min(100) {
            runs.push(graph::seed_run(&mut tx, app, name, None).await);
        }
        tx.commit().await.unwrap();
    }
    runs
}

async fn update_runs(
    service: &WorkflowService,
    app: &AppId,
    runs: &[String],
    patch: serde_json::Value,
) {
    for chunk in runs.chunks(100) {
        let tx = service.begin().await.unwrap();
        for run in chunk {
            journal_update(
                &tx,
                "runs",
                json!({"app_id":app.as_str(), "id":run}),
                patch.clone(),
            )
            .await;
        }
        tx.commit().await.unwrap();
    }
}

async fn run_row(service: &WorkflowService, app: &AppId, run: &str) -> Row {
    let tx = service.begin().await.unwrap();
    let mut rows = journal_rows(&tx, "runs", json!({"app_id":app.as_str(), "id":run})).await;
    tx.commit().await.unwrap();
    assert_eq!(rows.len(), 1);
    rows.remove(0)
}

async fn rows(service: &WorkflowService, table: &str, filter: serde_json::Value) -> Vec<Row> {
    let tx = service.begin().await.unwrap();
    let rows = journal_rows(&tx, table, filter).await;
    tx.commit().await.unwrap();
    rows
}

/// Every table a propagation page can change, for exact rollback and replay checks.
async fn snapshot(service: &WorkflowService, app: &AppId) -> Vec<Vec<zeroship_data_orm::Value>> {
    let tx = service.begin().await.unwrap();
    let mut snapshot = Vec::new();
    for table in [
        "runs",
        "job_publications",
        "job_receipts",
        "propagations",
        "propagation_pages",
        "tasks",
    ] {
        snapshot.push(
            journal_rows(&tx, table, json!({"app_id":app.as_str()}))
                .await
                .into_iter()
                .map(|row| row.0)
                .collect(),
        );
    }
    tx.commit().await.unwrap();
    snapshot
}

/// The committed Advance intent for a run's current generation and frontier.
async fn frontier_job(scope: &AppWorkflows, run: &str) -> JobSpec {
    let row = run_row(&scope.service, scope.app_id(), run).await;
    let current = (
        row.integer("generation").unwrap(),
        row.integer("frontier_revision").unwrap(),
    );
    let mut after = None;
    loop {
        let page = scope.pending_jobs(after.as_ref(), 100).await.unwrap();
        after = Some(
            page.last()
                .expect("the run's frontier is published")
                .id
                .clone(),
        );
        if let Some(job) = page.into_iter().find(|job| {
            matches!(&job.operation, JobOperation::Advance { run_id, generation, revision, .. }
                if run_id.as_str() == run && (i64::from(*generation), revision.get()) == current)
        }) {
            return job;
        }
    }
}

/// Cancel an idle run through its delivered Advance job, which settles it.
async fn cancel_idle(scope: &AppWorkflows, run: &str) -> JobReceipt {
    scope
        .transition(&RequestId::mint(), run, RunOperation::Cancel)
        .await
        .unwrap();
    let advance = frontier_job(scope, run).await;
    let crate::service::delivery::JobAcceptance::Settled(receipt) =
        scope.accept_job(&JobGrant::new(&advance)).await.unwrap()
    else {
        panic!("a cancelled idle run settles without execution")
    };
    *receipt
}

/// The single committed page of one obligation that has no receipt yet.
async fn open_page(scope: &AppWorkflows) -> JobSpec {
    let mut open = open_propagations(scope).await;
    assert_eq!(open.len(), 1, "exactly one open propagation page");
    open.remove(0)
}

async fn page_results(service: &WorkflowService, app: &AppId) -> Vec<serde_json::Value> {
    let mut pages = rows(service, "propagation_pages", json!({"app_id":app.as_str()})).await;
    pages.sort_by_key(|page| page.integer("revision").unwrap());
    pages
        .into_iter()
        .map(|page| serde_json::from_str(&page.text("result").unwrap()).unwrap())
        .collect()
}

/// Replay a committed page on a fresh attempt and with exhausted authority.
async fn assert_exact_replay(
    service: &WorkflowService,
    scope: &AppWorkflows,
    job: &JobSpec,
    receipt: &JobReceipt,
) {
    let before = snapshot(service, scope.app_id()).await;
    let grant = JobGrant::new(job);
    assert_eq!(
        &scope
            .propagation_job(&grant.retry(), PropagationOptions { page_size: 1 })
            .await
            .unwrap(),
        receipt
    );
    let mut expired = grant.retry();
    expired.expires = Instant::now();
    assert!(expired.remaining().is_none());
    assert_eq!(
        &scope
            .propagation_job(&expired, PropagationOptions::default())
            .await
            .unwrap(),
        receipt
    );
    assert_eq!(
        scope.job_receipt(job).await.unwrap().as_ref(),
        Some(receipt)
    );
    assert_eq!(snapshot(service, scope.app_id()).await, before);
}

fn row_limit() -> usize {
    usize::try_from(MAX_ROW_LIMIT).unwrap()
}

/// Deliver an obligation page by page, checking the committed chain: every
/// page but the last waits for its exact successor, and cursors link pages.
async fn deliver_chain(
    service: &WorkflowService,
    scope: &AppWorkflows,
    options: PropagationOptions,
) -> Vec<(JobSpec, JobReceipt, serde_json::Value)> {
    let mut chain: Vec<(JobSpec, JobReceipt, serde_json::Value)> = Vec::new();
    while let Some(job) = open_propagations(scope).await.into_iter().next() {
        let receipt = scope
            .propagation_job(&JobGrant::new(&job), options)
            .await
            .unwrap();
        let page = rows(service, "propagation_pages", json!({"id":job.id.as_str()}))
            .await
            .remove(0);
        let result: serde_json::Value =
            serde_json::from_str(&page.text("result").unwrap()).unwrap();
        assert!(result["affected"].as_i64().unwrap() <= i64::from(options.page_size));
        if let Some((previous, previous_receipt, previous_result)) = chain.last() {
            assert_eq!(previous_receipt.outcome, JobOutcome::Waiting {});
            assert_eq!(previous_result["after"], result["before"]);
            let JobOperation::Propagate { revision, .. } = &job.operation else {
                panic!("propagation page")
            };
            let JobOperation::Propagate {
                revision: earlier, ..
            } = &previous.operation
            else {
                panic!("propagation page")
            };
            assert_eq!(revision.get(), earlier.get() + 1);
        } else {
            assert!(result["before"].is_null());
        }
        chain.push((job, receipt, result));
    }
    let (_, last, result) = chain.last().expect("at least one page");
    assert_eq!(last.outcome, JobOutcome::Completed {});
    assert_eq!(result["finished"], json!(true));
    chain
}

fn affected(chain: &[(JobSpec, JobReceipt, serde_json::Value)]) -> i64 {
    chain
        .iter()
        .map(|(_, _, result)| result["affected"].as_i64().unwrap())
        .sum()
}
