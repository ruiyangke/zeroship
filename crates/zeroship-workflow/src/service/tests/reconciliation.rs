#![expect(clippy::future_not_send, reason = "creator recovery tests use compio")]

use super::*;
use crate::service::{
    publication::JobPublisher, reconciliation::ReconciliationOptions, AppWorkflows,
};
use std::{
    cell::{Cell, RefCell},
    time::{Duration, Instant},
};
use zeroship_core::{
    workflow_coordination::WorkerId,
    workflow_jobs::{Delivery, JobId, JobLease, JobOperation, JobOutcome, JobSpec},
};
use zeroship_data_orm::value;

struct Grant {
    delivery: Delivery,
    expires: Instant,
}
impl Grant {
    fn new(seed: &JobSpec) -> Self {
        let mut job = seed.clone();
        job.id = JobId::mint();
        job.operation = JobOperation::Reconcile {};
        Self {
            delivery: Delivery {
                job,
                worker_id: WorkerId::mint(),
                assignment_revision: 1.try_into().unwrap(),
                attempt: 1.try_into().unwrap(),
                deadline: 1.try_into().unwrap(),
            },
            expires: Instant::now() + Duration::from_secs(30),
        }
    }
    fn retry(&self) -> Self {
        let mut delivery = self.delivery.clone();
        delivery.attempt = (delivery.attempt.get() + 1).try_into().unwrap();
        Self {
            delivery,
            expires: Instant::now() + Duration::from_secs(30),
        }
    }
}
impl JobLease for Grant {
    fn delivery(&self) -> &Delivery {
        &self.delivery
    }
    fn remaining(&self) -> Option<Duration> {
        self.expires
            .checked_duration_since(Instant::now())
            .filter(|time| !time.is_zero())
    }
}

struct Publisher {
    app: AppId,
    manager: publication::Manager,
    calls: RefCell<Vec<JobId>>,
    hang: Option<JobId>,
    barrier: Option<(flume::Sender<()>, flume::Receiver<()>)>,
    lose_reply: Cell<bool>,
}
impl Publisher {
    async fn new(app: &AppId) -> Self {
        Self {
            app: app.clone(),
            manager: publication::Manager::new(app).await,
            calls: RefCell::new(Vec::new()),
            hang: None,
            barrier: None,
            lose_reply: Cell::new(false),
        }
    }
}
impl JobPublisher for Publisher {
    fn app_id(&self) -> &AppId {
        &self.app
    }
    async fn submit(&self, job: &JobSpec) -> Result<JobSpec, WorkflowServiceError> {
        self.calls.borrow_mut().push(job.id.clone());
        if self.hang.as_ref() == Some(&job.id) {
            std::future::pending::<()>().await;
        }
        let job = self
            .manager
            .queue
            .submit(job)
            .await
            .map_err(|_| WorkflowServiceError::Unavailable("test publication failed".into()))?;
        if let Some((entered, resume)) = &self.barrier {
            entered.send_async(()).await.unwrap();
            resume.recv_async().await.unwrap();
        }
        if self.lose_reply.replace(false) {
            return Err(WorkflowServiceError::Timeout);
        }
        Ok(job)
    }
}

fn options(page_size: u32) -> ReconciliationOptions {
    ReconciliationOptions {
        page_size,
        item_timeout: Duration::from_secs(2),
    }
}

async fn start(app: &AppWorkflows, count: usize) -> Vec<JobSpec> {
    for _ in 0..count {
        app.start(&RequestId::mint(), "Example", StartOptions::default())
            .await
            .unwrap();
    }
    app.pending_jobs(None, 100).await.unwrap()
}

async fn scans(service: &WorkflowService, app: &AppId) -> Vec<super::super::store::Row> {
    let tx = service.begin().await.unwrap();
    let rows = journal_rows(&tx, "reconciliation_scans", json!({"id":app.as_str()})).await;
    tx.commit().await.unwrap();
    rows
}

async fn confirmed(service: &WorkflowService, app: &AppId, job: &JobId) -> bool {
    let tx = service.begin().await.unwrap();
    let rows = journal_rows(
        &tx,
        "job_publications",
        json!({"app_id":app.as_str(), "id":job.as_str()}),
    )
    .await;
    let confirmed = rows[0].optional_integer("confirmed_at").unwrap().is_some();
    tx.commit().await.unwrap();
    confirmed
}

macro_rules! case {
    ($sqlite:ident, $postgres:ident, $contract:ident) => {
        #[compio::test]
        async fn $sqlite() {
            let dir = tempfile::tempdir().unwrap();
            Box::pin($contract(Rc::new(
                sqlite_store(&dir.path().join("creator.sqlite")).await,
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
mod deployment_holds;

case!(
    sqlite_reconciliation_pages_replay_without_code_and_preserve_scope,
    postgres_reconciliation_pages_replay_without_code_and_preserve_scope,
    pages
);
case!(
    sqlite_reconciliation_resumes_past_a_timed_out_item,
    postgres_reconciliation_resumes_past_a_timed_out_item,
    timeout_progress
);
case!(
    sqlite_reconciliation_keeps_failed_intents_for_the_next_sweep,
    postgres_reconciliation_keeps_failed_intents_for_the_next_sweep,
    failures
);
case!(
    sqlite_reconciliation_policy_replacement_cannot_confirm_a_waiting_submission,
    postgres_reconciliation_policy_replacement_cannot_confirm_a_waiting_submission,
    policy
);
case!(
    sqlite_reconciliation_concurrent_roots_do_not_regress_the_scan,
    postgres_reconciliation_concurrent_roots_do_not_regress_the_scan,
    concurrency
);

async fn pages(store: Rc<OrmStore>) {
    let (service, app, foreign, _deployments) = registered_service(store.clone()).await;
    let scoped = service.fixture_app(app.clone());
    let jobs = start(&scoped, 3).await;
    let other = start(&service.fixture_app(foreign.clone()), 1)
        .await
        .remove(0);
    let publisher = Publisher::new(&app).await;
    let first = Grant::new(&jobs[0]);
    assert!(first.delivery.job.deployment_id().is_none());
    let receipt = scoped
        .reconcile_job(&first, &publisher, options(1))
        .await
        .unwrap();
    assert_eq!(receipt.outcome, JobOutcome::Waiting {});
    assert_eq!(publisher.calls.borrow().as_slice(), &[jobs[0].id.clone()]);
    assert!(!confirmed(&service, &foreign, &other.id).await);

    // Reopening needs no executable/deployment loader, including for old pins.
    let reopened = WorkflowService::open(store, Arc::new(HostPolicies::default()))
        .await
        .unwrap();
    reopened
        .fixture_register(
            &app,
            leased_policy(
                1,
                AppPolicy {
                    admission: false,
                    dispatch: false,
                    ..AppPolicy::default()
                },
            ),
        )
        .await
        .unwrap();
    let scope = reopened.fixture_app(app.clone());
    let mut expired = first.retry();
    expired.expires = Instant::now();
    assert_eq!(
        scope
            .reconcile_job(&expired, &publisher, options(1))
            .await
            .unwrap(),
        receipt
    );
    assert_eq!(publisher.calls.borrow().len(), 1);
    let mut changed = first.retry();
    changed.delivery.job.available_at = 0.try_into().unwrap();
    assert!(matches!(
        scope.reconcile_job(&changed, &publisher, options(1)).await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    for expected in [JobOutcome::Waiting {}, JobOutcome::Waiting {}] {
        let grant = Grant::new(&jobs[0]);
        assert_eq!(
            scope
                .reconcile_job(&grant, &publisher, options(1))
                .await
                .unwrap()
                .outcome,
            expected
        );
    }
    assert_eq!(
        publisher.calls.borrow().as_slice(),
        &jobs.iter().map(|job| job.id.clone()).collect::<Vec<_>>()
    );
    assert!(scope.pending_jobs(None, 10).await.unwrap().is_empty());
    assert_explicit_empty_phase_transitions(&reopened, &scope, &jobs[0], &publisher).await;
}

async fn assert_explicit_empty_phase_transitions(
    service: &WorkflowService,
    scope: &AppWorkflows,
    seed: &JobSpec,
    publisher: &Publisher,
) {
    let state = scans(service, scope.app_id()).await;
    assert_eq!(state[0].integer("revision").unwrap(), 4);
    assert!(state[0].optional_text("after_id").unwrap().is_none());
    assert_eq!(state[0].text("phase").unwrap(), "deployment_holds");
    let empty = Grant::new(seed);
    assert_eq!(
        scope
            .reconcile_job(&empty, publisher, options(1))
            .await
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );
    assert_eq!(
        scans(service, scope.app_id()).await[0]
            .integer("revision")
            .unwrap(),
        5
    );
    assert_eq!(
        scans(service, scope.app_id()).await[0]
            .text("phase")
            .unwrap(),
        "publications"
    );
    assert_eq!(
        scope
            .reconcile_job(&Grant::new(seed), publisher, options(1))
            .await
            .unwrap()
            .outcome,
        JobOutcome::Waiting {}
    );
    assert_eq!(
        scope
            .reconcile_job(&Grant::new(seed), publisher, options(1))
            .await
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );
}

async fn timeout_progress(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let jobs = start(&scope, 2).await;
    let mut publisher = Publisher::new(&app).await;
    publisher.hang = Some(jobs[0].id.clone());
    let mut grant = Grant::new(&jobs[0]);
    grant.expires = Instant::now() + Duration::from_millis(300);
    assert!(matches!(
        scope.reconcile_job(&grant, &publisher, options(2)).await,
        Err(WorkflowServiceError::Timeout)
    ));
    assert_eq!(publisher.calls.borrow().as_slice(), &[jobs[0].id.clone()]);
    assert!(scope
        .job_receipt(&grant.delivery.job)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        scope
            .reconcile_job(&grant.retry(), &publisher, options(2))
            .await
            .unwrap()
            .outcome,
        JobOutcome::Waiting {}
    );
    assert_eq!(
        publisher.calls.borrow().as_slice(),
        &[jobs[0].id.clone(), jobs[1].id.clone()]
    );
    assert!(!confirmed(&service, &app, &jobs[0].id).await);
    assert!(confirmed(&service, &app, &jobs[1].id).await);
    publisher.hang = None;
    assert_eq!(
        scope
            .reconcile_job(&Grant::new(&jobs[0]), &publisher, options(2))
            .await
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );
    let next = Grant::new(&jobs[0]);
    scope
        .reconcile_job(&next, &publisher, options(2))
        .await
        .unwrap();
    assert!(scope.pending_jobs(None, 10).await.unwrap().is_empty());
}

async fn failures(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let jobs = start(&scope, 3).await;
    let publisher = Publisher::new(&app).await;
    publisher.lose_reply.set(true);
    let tx = service.begin().await.unwrap();
    tx.database()
        .collection("__zeroship_workflow_job_publications")
        .unwrap()
        .update(
            value!({"app_id":app.as_str(), "id":jobs[0].id.as_str()}),
            value!({"specification":"invalid"}),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    scope
        .reconcile_job(&Grant::new(&jobs[0]), &publisher, options(3))
        .await
        .unwrap();
    assert_eq!(
        publisher.calls.borrow().as_slice(),
        &[jobs[1].id.clone(), jobs[2].id.clone()]
    );
    assert!(!confirmed(&service, &app, &jobs[0].id).await);
    assert!(!confirmed(&service, &app, &jobs[1].id).await);
    assert!(confirmed(&service, &app, &jobs[2].id).await);
    let tx = service.begin().await.unwrap();
    tx.database()
        .collection("__zeroship_workflow_job_publications")
        .unwrap()
        .update(
            value!({"app_id":app.as_str(), "id":jobs[0].id.as_str()}),
            value!({"specification":serde_json::to_string(&jobs[0]).unwrap()}),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        scope
            .reconcile_job(&Grant::new(&jobs[0]), &publisher, options(3))
            .await
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );
    scope
        .reconcile_job(&Grant::new(&jobs[0]), &publisher, options(3))
        .await
        .unwrap();
    assert!(scope.pending_jobs(None, 10).await.unwrap().is_empty());
}

async fn policy(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let jobs = start(&scope, 1).await;
    let mut publisher = Publisher::new(&app).await;
    let (entered, waiting) = flume::bounded(1);
    let (resume, gate) = flume::bounded(1);
    publisher.barrier = Some((entered, gate));
    let grant = Grant::new(&jobs[0]);
    let replace = async {
        waiting.recv_async().await.unwrap();
        service
            .fixture_register(&app, leased_policy(2, AppPolicy::default()))
            .await
            .unwrap();
        resume.send_async(()).await.unwrap();
    };
    let (result, ()) = futures::join!(scope.reconcile_job(&grant, &publisher, options(1)), replace);
    assert!(result.is_err());
    assert!(!confirmed(&service, &app, &jobs[0].id).await);
    assert!(scope
        .job_receipt(&grant.delivery.job)
        .await
        .unwrap()
        .is_none());
    publisher.barrier = None;
    scope
        .reconcile_job(&grant.retry(), &publisher, options(1))
        .await
        .unwrap();
    assert_eq!(publisher.calls.borrow().len(), 1);
    assert_eq!(
        scope
            .reconcile_job(&Grant::new(&jobs[0]), &publisher, options(1))
            .await
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );
    scope
        .reconcile_job(&Grant::new(&jobs[0]), &publisher, options(1))
        .await
        .unwrap();
    assert!(confirmed(&service, &app, &jobs[0].id).await);
}

async fn concurrency(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let jobs = start(&scope, 2).await;
    let mut publisher = Publisher::new(&app).await;
    let (entered, waiting) = flume::bounded(2);
    let (resume, gate) = flume::bounded(2);
    publisher.barrier = Some((entered, gate));
    let first = Grant::new(&jobs[0]);
    let second = Grant::new(&jobs[0]);
    let competing = async {
        waiting.recv_async().await.unwrap();
        waiting.recv_async().await.unwrap();
        scope
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await
            .unwrap();
        resume.send_async(()).await.unwrap();
        resume.send_async(()).await.unwrap();
    };
    let (a, b, ()) = futures::join!(
        scope.reconcile_job(&first, &publisher, options(1)),
        scope.reconcile_job(&second, &publisher, options(1)),
        competing
    );
    assert_eq!(a.unwrap().outcome, JobOutcome::Waiting {});
    assert_eq!(b.unwrap().outcome, JobOutcome::Waiting {});
    assert_eq!(
        scans(&service, &app).await[0].integer("revision").unwrap(),
        2
    );
    publisher.barrier = None;
    assert_eq!(
        scope
            .reconcile_job(&Grant::new(&jobs[0]), &publisher, options(1))
            .await
            .unwrap()
            .outcome,
        JobOutcome::Waiting {}
    );
    assert_eq!(
        scope.pending_jobs(None, 10).await.unwrap().len(),
        1,
        "newer intent is outside the captured upper boundary"
    );
    assert_eq!(
        scope
            .reconcile_job(&Grant::new(&jobs[0]), &publisher, options(1))
            .await
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );
    scope
        .reconcile_job(&Grant::new(&jobs[0]), &publisher, options(1))
        .await
        .unwrap();
    assert!(scope.pending_jobs(None, 10).await.unwrap().is_empty());
}

enum ReceiptFault {
    Sqlite(std::path::PathBuf),
    Postgres(Box<compio_postgres::Client>),
}
impl ReceiptFault {
    async fn set(&self, enabled: bool) {
        match self {
            Self::Sqlite(path) => {
                let path = path.clone();
                let sql = if enabled {
                    "CREATE TRIGGER fail_reconciliation BEFORE UPDATE OF outcome ON __zeroship_workflow_job_receipts BEGIN SELECT RAISE(ABORT, 'injected receipt failure'); END"
                } else { "DROP TRIGGER fail_reconciliation" };
                compio::runtime::spawn_blocking(move || {
                    rusqlite::Connection::open_with_flags(
                        path,
                        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
                    )
                    .unwrap()
                    .execute_batch(sql)
                    .unwrap();
                })
                .await
                .unwrap();
            }
            Self::Postgres(client) => client.batch_execute(if enabled {
                "CREATE FUNCTION customer.fail_reconciliation() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected receipt failure'; END $$; CREATE TRIGGER fail_reconciliation BEFORE UPDATE OF outcome ON customer.__zeroship_workflow_job_receipts FOR EACH ROW EXECUTE FUNCTION customer.fail_reconciliation();"
            } else { "DROP TRIGGER fail_reconciliation ON customer.__zeroship_workflow_job_receipts; DROP FUNCTION customer.fail_reconciliation();" }).await.unwrap(),
        }
    }
}

#[compio::test]
async fn sqlite_reconciliation_receipt_failure_does_not_advance_the_scan() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("zs-workflow.sqlite");
    let store = Rc::new(sqlite_store(&path).await);
    Box::pin(receipt_rollback(store, ReceiptFault::Sqlite(path))).await;
}

#[compio::test]
async fn postgres_reconciliation_receipt_failure_does_not_advance_the_scan() {
    let fixture = PostgresFixture::start().await;
    let client = connect(&fixture.admin_url).await;
    Box::pin(receipt_rollback(
        Rc::new(fixture.store.clone()),
        ReceiptFault::Postgres(Box::new(client)),
    ))
    .await;
}

async fn receipt_rollback(store: Rc<OrmStore>, fault: ReceiptFault) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let jobs = start(&scope, 2).await;
    let publisher = Publisher::new(&app).await;
    let grant = Grant::new(&jobs[0]);
    fault.set(true).await;
    assert!(scope
        .reconcile_job(&grant, &publisher, options(1))
        .await
        .is_err());
    assert!(scope
        .job_receipt(&grant.delivery.job)
        .await
        .unwrap()
        .is_none());
    assert!(confirmed(&service, &app, &jobs[0].id).await);
    let state = scans(&service, &app).await;
    assert_eq!(state[0].integer("revision").unwrap(), 1);
    assert!(state[0].optional_text("after_id").unwrap().is_none());
    fault.set(false).await;
    let receipt = scope
        .reconcile_job(&grant.retry(), &publisher, options(1))
        .await
        .unwrap();
    assert_eq!(receipt.outcome, JobOutcome::Waiting {});
    assert_eq!(
        publisher.calls.borrow().len(),
        1,
        "retry resumes finalization without publishing again"
    );
    assert_eq!(
        scans(&service, &app).await[0].integer("revision").unwrap(),
        2
    );
    assert_eq!(
        scope.job_receipt(&grant.delivery.job).await.unwrap(),
        Some(receipt)
    );

    let last_publication = Grant::new(&jobs[0]);
    fault.set(true).await;
    assert!(scope
        .reconcile_job(&last_publication, &publisher, options(1))
        .await
        .is_err());
    let unchanged = scans(&service, &app).await;
    assert_eq!(unchanged[0].integer("revision").unwrap(), 2);
    assert_eq!(unchanged[0].text("phase").unwrap(), "publications");
    assert_eq!(
        unchanged[0].optional_text("after_id").unwrap().as_deref(),
        Some(jobs[0].id.as_str())
    );
    assert!(confirmed(&service, &app, &jobs[1].id).await);
    fault.set(false).await;
    assert_eq!(
        scope
            .reconcile_job(&last_publication.retry(), &publisher, options(1))
            .await
            .unwrap()
            .outcome,
        JobOutcome::Waiting {}
    );
    let advanced = scans(&service, &app).await;
    assert_eq!(advanced[0].integer("revision").unwrap(), 3);
    assert_eq!(advanced[0].text("phase").unwrap(), "deployment_holds");
    assert!(advanced[0].optional_text("after_id").unwrap().is_none());
    assert_eq!(
        publisher.calls.borrow().len(),
        jobs.len(),
        "phase finalization retries must not resubmit confirmed publications"
    );
}
