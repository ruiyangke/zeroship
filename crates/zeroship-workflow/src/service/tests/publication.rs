#![expect(
    clippy::future_not_send,
    reason = "native publication tests use compio"
)]

use super::*;
pub(in crate::service) use super::manager_queue::Manager;
use crate::{
    operations::{RestartOptions, RunOperation},
    service::{publication::JobPublisher, AppWorkflows, WorkerIdentity},
    WorkflowExecution,
};
use std::{cell::Cell, time::Duration};
use zeroship_core::{
    workflow_coordination::Revision,
    workflow_jobs::{BroadcastId, DeploymentId, JobOperation, JobSpec, PropagationId},
};
use zeroship_workflow_manager::Queue;

enum FaultDb {
    Sqlite(rusqlite::Connection),
    Postgres(compio_postgres::Client),
}
impl FaultDb {
    async fn inject(&self, event: &str) {
        match self {
            Self::Sqlite(connection) => sqlite_ddl(connection, format!(
                "CREATE TRIGGER fail_publication BEFORE {event} ON __zeroship_workflow_job_publications \
                 BEGIN SELECT RAISE(ABORT, 'injected publication failure'); END;"
            )).await,
            Self::Postgres(connection) => connection.batch_execute(&format!(
                "CREATE FUNCTION customer.fail_publication() RETURNS trigger LANGUAGE plpgsql AS \
                 $$ BEGIN RAISE EXCEPTION 'injected publication failure'; END $$; \
                 CREATE TRIGGER fail_publication BEFORE {event} ON customer.__zeroship_workflow_job_publications \
                 FOR EACH ROW EXECUTE FUNCTION customer.fail_publication();"
            )).await.unwrap(),
        }
    }
    async fn clear(&self) {
        match self {
            Self::Sqlite(connection) => sqlite_ddl(connection, "DROP TRIGGER fail_publication".into()).await,
            Self::Postgres(connection) => connection.batch_execute(
                "DROP TRIGGER fail_publication ON customer.__zeroship_workflow_job_publications; \
                 DROP FUNCTION customer.fail_publication();"
            ).await.unwrap(),
        }
    }
}

async fn sqlite_ddl(connection: &rusqlite::Connection, sql: String) {
    let path = connection.path().unwrap().to_owned();
    // Allow the owning runtime to finish rollback while schema DDL waits.
    compio::runtime::spawn_blocking(move || rusqlite::Connection::open(path)?.execute_batch(&sql))
        .await
        .unwrap()
        .unwrap();
}

macro_rules! case {
    ($sqlite:ident, $postgres:ident, $contract:ident) => {
        #[compio::test]
        async fn $sqlite() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("zs-workflow.sqlite");
            let store = sqlite_store(&path).await;
            $contract(
                Rc::new(store),
                &FaultDb::Sqlite(rusqlite::Connection::open(path).unwrap()),
            )
            .await;
        }
        #[compio::test]
        async fn $postgres() {
            let fixture = PostgresFixture::start().await;
            let admin = FaultDb::Postgres(connect(&fixture.admin_url).await);
            $contract(Rc::new(fixture.store.clone()), &admin).await;
        }
    };
}

case!(
    sqlite_publication_recovers_lost_replies,
    postgres_publication_recovers_lost_replies,
    recovery
);
case!(
    sqlite_publication_rolls_back_with_acceptance,
    postgres_publication_rolls_back_with_acceptance,
    rollback
);
case!(
    sqlite_publication_tracks_frontier_and_generation,
    postgres_publication_tracks_frontier_and_generation,
    frontiers
);
case!(
    sqlite_publication_survives_history_removal_and_checks_identity,
    postgres_publication_survives_history_removal_and_checks_identity,
    retention
);
case!(
    sqlite_publication_rejects_changed_executable_prerequisite,
    postgres_publication_rejects_changed_executable_prerequisite,
    executable_identity
);
case!(
    sqlite_publication_keys_are_derived_from_their_specification,
    postgres_publication_keys_are_derived_from_their_specification,
    derived_keys
);
case!(
    sqlite_publication_deduplicates_equal_work_across_transactions,
    postgres_publication_deduplicates_equal_work_across_transactions,
    dedup
);

/// A publishable job's id is the one its own content derives, so an intent
/// whose specification moved off that key is refused, and an intent wearing an
/// operation no creator journal publishes derives no key at all.
async fn derived_keys(store: Rc<OrmStore>, _: &FaultDb) {
    let (service, app, _, _platform) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let advance = scope.pending_jobs(None, 1).await.unwrap().remove(0);
    let manager = Manager::new(&app).await;
    let publisher = Publisher::new(&app, manager.queue.clone());

    // The key the journal holds this intent under is the one its content
    // derives; another app's, another due time's and an unpublishable
    // operation's are not.
    assert_eq!(advance.publication_id(), Some(advance.id.clone()));
    let mut elsewhere = advance.clone();
    elsewhere.app_id = AppId::mint();
    assert_ne!(elsewhere.publication_id(), Some(advance.id.clone()));
    let mut rescheduled = advance.clone();
    rescheduled.available_at = (advance.available_at.get() + 1).try_into().unwrap();
    assert_ne!(rescheduled.publication_id(), Some(advance.id.clone()));
    let mut unpublishable = advance.clone();
    unpublishable.operation = JobOperation::Reconcile {};
    assert_eq!(unpublishable.publication_id(), None);

    // Each of those, written under the key the intact intent owns, is a
    // damaged journal rather than a job.
    for changed in [elsewhere, rescheduled, unpublishable] {
        let tx = service.begin().await.unwrap();
        journal_update(
            &tx,
            "job_publications",
            json!({"id":advance.id.as_str()}),
            json!({"specification":serde_json::to_string(&changed).unwrap()}),
        )
        .await;
        tx.commit().await.unwrap();
        assert!(
            matches!(
                scope.pending_jobs(None, 1).await,
                Err(WorkflowServiceError::Internal(_))
            ),
            "{changed:?}"
        );
        assert!(
            matches!(
                scope.publish_job(&advance.id, &publisher).await,
                Err(WorkflowServiceError::Internal(_))
            ),
            "{changed:?}"
        );
        assert_eq!(publisher.calls.get(), 0);
        assert_eq!(manager.count(), 0);
    }

    // The control: the specification that does derive this key publishes.
    let tx = service.begin().await.unwrap();
    journal_update(
        &tx,
        "job_publications",
        json!({"id":advance.id.as_str()}),
        json!({"specification":serde_json::to_string(&advance).unwrap()}),
    )
    .await;
    tx.commit().await.unwrap();
    assert_eq!(
        scope.pending_jobs(None, 1).await.unwrap(),
        std::slice::from_ref(&advance)
    );
    assert_eq!(
        scope.publish_job(&advance.id, &publisher).await.unwrap(),
        advance
    );
    assert_eq!(manager.count(), 1);
}

/// Recording the same work twice, in two separate committed transactions,
/// leaves ONE intent under ONE id.
///
/// This is what the derived key buys: the recorder does not search for an
/// earlier row by a tuple of business columns, it computes the key and finds
/// the row already there. Each of the three publishable kinds is exercised,
/// because each derives from a different tuple, and each is PAIRED WITH A
/// CONTROL that moves one component of that tuple and must therefore produce a
/// SECOND intent - without it a derivation that ignored its inputs entirely
/// would print exactly what this prints.
async fn dedup(store: Rc<OrmStore>, _: &FaultDb) {
    let (service, app, _, _platform) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let broadcast = BroadcastId::mint();
    let obligation = PropagationId::mint();

    let first = record_each(&service, &app, &run.id, &broadcast, &obligation, 1).await;
    let again = record_each(&service, &app, &run.id, &broadcast, &obligation, 1).await;
    assert_eq!(again, first, "equal work must resolve to the same jobs");
    let moved = record_each(&service, &app, &run.id, &broadcast, &obligation, 2).await;
    assert_eq!(moved.len(), first.len());
    for (repeated, control) in first.iter().zip(&moved) {
        assert_ne!(
            repeated.id, control.id,
            "a moved revision must be a different job"
        );
    }

    // One intent per distinct identity, and every one of them still keyed by
    // what it derives.
    let tx = service.begin().await.unwrap();
    let intents = publication_intents(&tx, &app).await;
    tx.commit().await.unwrap();
    assert!(!intents.is_empty());
    let mut keys: Vec<&str> = intents.iter().map(|job| job.id.as_str()).collect();
    let total = keys.len();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), total, "every intent must own a distinct key");
    for job in &intents {
        assert_eq!(job.publication_id(), Some(job.id.clone()));
    }
    for recorded in first.iter().chain(&moved) {
        assert_eq!(
            intents.iter().filter(|job| job.id == recorded.id).count(),
            1,
            "{:?} must have exactly one intent",
            recorded.operation
        );
    }
}

/// Record one Advance, one Fanout and one Propagate page, each in its own
/// committed transaction, at the given frontier and page revision.
async fn record_each(
    service: &WorkflowService,
    app: &AppId,
    run: &str,
    broadcast: &BroadcastId,
    obligation: &PropagationId,
    revision: i64,
) -> Vec<JobSpec> {
    use crate::service::publication;
    let page = Revision::try_from(revision).unwrap();
    let mut recorded = Vec::new();

    // The run's frontier is re-observed, not advanced: `record_job` reads the
    // revision the row carries, so setting it is how this names one identity
    // twice and a second one once.
    let mut tx = service.begin().await.unwrap();
    let now = tx.now().await.unwrap();
    journal_update(
        &tx,
        "runs",
        json!({"app_id":app.as_str(), "id":run}),
        json!({"frontier_revision":revision}),
    )
    .await;
    recorded.push(
        publication::record_job(&tx, app, run, now)
            .await
            .unwrap()
            .expect("a runnable frontier publishes an Advance"),
    );
    tx.commit().await.unwrap();

    let mut tx = service.begin().await.unwrap();
    let now = tx.now().await.unwrap();
    recorded.push(
        publication::fanout(&tx, app, broadcast, page, now)
            .await
            .unwrap(),
    );
    tx.commit().await.unwrap();

    let mut tx = service.begin().await.unwrap();
    let now = tx.now().await.unwrap();
    recorded.push(
        publication::propagate(&tx, app, obligation, page, now)
            .await
            .unwrap(),
    );
    tx.commit().await.unwrap();
    recorded
}

async fn executable_identity(store: Rc<OrmStore>, _: &FaultDb) {
    let (service, app, _, _platform) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let original = scope.pending_jobs(None, 1).await.unwrap().remove(0);
    let manager = Manager::new(&app).await;
    let publisher = Publisher::new(&app, manager.queue.clone());
    let mut substituted = original.clone();
    let JobOperation::Advance { deployment_id, .. } = &mut substituted.operation else {
        panic!("publication must require executable code")
    };
    *deployment_id = DeploymentId::mint();
    let mut erased = original.clone();
    erased.operation = JobOperation::Reconcile {};
    for changed in [substituted, erased] {
        let tx = service.begin().await.unwrap();
        journal_update(
            &tx,
            "job_publications",
            json!({"id":original.id.as_str()}),
            json!({"specification":serde_json::to_string(&changed).unwrap()}),
        )
        .await;
        tx.commit().await.unwrap();
        assert!(matches!(
            scope.pending_jobs(None, 1).await,
            Err(WorkflowServiceError::Internal(_))
        ));
        assert!(matches!(
            scope.publish_job(&original.id, &publisher).await,
            Err(WorkflowServiceError::Internal(_))
        ));
        assert_eq!(publisher.calls.get(), 0);
        assert_eq!(manager.count(), 0);
    }
    let tx = service.begin().await.unwrap();
    journal_update(
        &tx,
        "job_publications",
        json!({"id":original.id.as_str()}),
        json!({"specification":serde_json::to_string(&original).unwrap()}),
    )
    .await;
    tx.commit().await.unwrap();
    assert_eq!(
        scope.publish_job(&original.id, &publisher).await.unwrap(),
        original
    );
    assert_eq!(manager.count(), 1);
}

#[derive(Clone, Copy)]
enum Reply {
    Exact,
    Lost,
    Changed,
}
pub(super) struct Publisher {
    app: AppId,
    queue: Queue,
    reply: Cell<Reply>,
    calls: Cell<usize>,
    gate: Option<(flume::Sender<()>, flume::Receiver<()>)>,
}
impl Publisher {
    pub(super) fn new(app: &AppId, queue: Queue) -> Self {
        Self {
            app: app.clone(),
            queue,
            reply: Cell::new(Reply::Exact),
            calls: Cell::new(0),
            gate: None,
        }
    }
}
impl JobPublisher for Publisher {
    fn app_id(&self) -> &AppId {
        &self.app
    }
    async fn submit(&self, job: &JobSpec) -> Result<JobSpec, WorkflowServiceError> {
        self.calls.set(self.calls.get() + 1);
        let mut receipt = self
            .queue
            .submit(job)
            .await
            .map_err(|_| WorkflowServiceError::Unavailable("test manager refused".into()))?;
        if let Some((ready, resume)) = &self.gate {
            ready.send_async(()).await.unwrap();
            resume.recv_async().await.unwrap();
        }
        match self.reply.get() {
            Reply::Lost => return Err(WorkflowServiceError::Timeout),
            Reply::Changed => {
                receipt.available_at = (receipt.available_at.get() + 1).try_into().unwrap();
            }
            Reply::Exact => {}
        }
        Ok(receipt)
    }
}

async fn reopen(scope: &AppWorkflows) -> AppWorkflows {
    WorkflowService::open(scope.service.store.clone(), scope.service.policies.clone())
        .await
        .unwrap()
        .with_deployments(scope.service.deployments.clone().unwrap())
        .fixture_app(scope.app.clone())
}

async fn recovery(store: Rc<OrmStore>, faults: &FaultDb) {
    let (service, app, other, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let request = RequestId::mint();
    let objects = objects::Objects::new();
    let options = StartOptions {
        input_ref: objects
            .start_input(&scope, json!({"secret":"creator-private-input"}))
            .await,
        ..Default::default()
    };
    let run = scope
        .start(&request, "Example", options.clone())
        .await
        .unwrap();
    let jobs = scope.pending_jobs(None, 10).await.unwrap();
    assert_eq!(jobs.len(), 1);
    let job = jobs[0].clone();
    assert!(
        matches!(&job.operation, JobOperation::Advance { run_id, generation:0, revision, .. } if run_id.as_str() == run.id && revision.get() == 1)
    );
    assert!(!serde_json::to_string(&job)
        .unwrap()
        .contains("creator-private-input"));
    assert_eq!(
        scope.start(&request, "Example", options).await.unwrap(),
        run
    );
    assert_eq!(scope.pending_jobs(None, 10).await.unwrap(), jobs);
    assert!(service
        .fixture_app(other.clone())
        .pending_jobs(None, 10)
        .await
        .unwrap()
        .is_empty());
    assert!(scope.pending_jobs(None, 0).await.is_err());
    assert!(scope.pending_jobs(None, u32::MAX).await.is_err());
    assert!(scope
        .pending_jobs(Some(&job.id), 1)
        .await
        .unwrap()
        .is_empty());

    let manager = Manager::new(&app).await;
    let publisher = Publisher::new(&app, manager.queue.clone());
    let foreign = Publisher::new(&other, manager.queue.clone());
    assert_eq!(
        scope.publish_job(&job.id, &foreign).await,
        Err(WorkflowServiceError::PermissionDenied)
    );
    assert!(matches!(
        service
            .fixture_app(other)
            .publish_job(&job.id, &foreign)
            .await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    assert_eq!(foreign.calls.get(), 0);
    publisher.reply.set(Reply::Lost);
    assert_eq!(
        scope.publish_job(&job.id, &publisher).await,
        Err(WorkflowServiceError::Timeout)
    );
    assert_eq!(manager.count(), 1);
    assert_eq!(scope.pending_jobs(None, 10).await.unwrap(), jobs);
    let reopened = reopen(&scope).await;
    let publisher = Publisher::new(&app, Manager::open(&manager.path).await);
    publisher.reply.set(Reply::Changed);
    assert!(matches!(
        reopened.publish_job(&job.id, &publisher).await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    assert_eq!(reopened.pending_jobs(None, 10).await.unwrap(), jobs);

    publisher.reply.set(Reply::Exact);
    faults.inject("UPDATE").await;
    assert!(reopened.publish_job(&job.id, &publisher).await.is_err());
    faults.clear().await;
    assert_eq!(reopened.pending_jobs(None, 10).await.unwrap(), jobs);

    race_confirmation(&reopened, &run.id, &job, publisher).await;
    assert_eq!(manager.count(), 1);
}

async fn race_confirmation(scope: &AppWorkflows, run: &str, job: &JobSpec, publisher: Publisher) {
    // Race exact acknowledgements while proving no creator transaction spans I/O.
    let (ready, reached) = flume::bounded(2);
    let (resume, resumed) = flume::bounded(2);
    let publisher = Rc::new(Publisher {
        gate: Some((ready, resumed)),
        ..publisher
    });
    let attempts = (0..2)
        .map(|_| {
            let scope = scope.clone();
            let publisher = publisher.clone();
            let id = job.id.clone();
            compio::runtime::spawn(async move { scope.publish_job(&id, publisher.as_ref()).await })
        })
        .collect::<Vec<_>>();
    for _ in 0..2 {
        compio::time::timeout(Duration::from_secs(10), reached.recv_async())
            .await
            .unwrap()
            .unwrap();
    }
    compio::time::timeout(
        Duration::from_secs(10),
        scope.transition(&RequestId::mint(), run, RunOperation::Pause),
    )
    .await
    .unwrap()
    .unwrap();
    for _ in 0..2 {
        resume.send_async(()).await.unwrap();
    }
    for attempt in attempts {
        assert_eq!(&attempt.await.unwrap().unwrap(), job);
    }
    assert!(scope.pending_jobs(None, 10).await.unwrap().is_empty());
    let calls = publisher.calls.get();
    assert_eq!(
        &scope
            .publish_job(&job.id, publisher.as_ref())
            .await
            .unwrap(),
        job
    );
    assert_eq!(publisher.calls.get(), calls);
}

async fn rollback(store: Rc<OrmStore>, faults: &FaultDb) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let request = RequestId::mint();
    faults.inject("INSERT").await;
    assert!(scope
        .start(&request, "Example", StartOptions::default())
        .await
        .is_err());
    faults.clear().await;
    let tx = service.begin().await.unwrap();
    for table in [
        "runs",
        "generations",
        "requests",
        "outbox",
        "job_publications",
    ] {
        assert_eq!(
            journal_count(&tx, table, json!({"app_id":app.as_str()})).await,
            0,
            "{table}"
        );
    }
    tx.commit().await.unwrap();
    scope
        .start(&request, "Example", StartOptions::default())
        .await
        .unwrap();
    let original = scope.pending_jobs(None, 10).await.unwrap();
    assert_eq!(original.len(), 1);
    let worker = WorkerIdentity::new("publication-worker".into()).unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    let execution = WorkflowExecution::from_runtime_value(json!({"outcomes":[{
        "kind":"Sleep", "ordinal":0, "name":"delay", "nameOccurrence":0,
        "wakeAt":chrono::Utc::now() + chrono::Duration::hours(1)
    }]}))
    .unwrap();
    faults.inject("INSERT").await;
    assert!(service
        .complete(&worker, &task.id, &task.token, execution.clone())
        .await
        .is_err());
    faults.clear().await;
    let tx = service.begin().await.unwrap();
    assert_eq!(
        journal_count(&tx, "steps", json!({"app_id":app.as_str()})).await,
        0
    );
    let tasks = journal_rows(&tx, "tasks", json!({"id":task.id})).await;
    assert_eq!(tasks[0].text("state").unwrap(), "leased");
    tx.commit().await.unwrap();
    assert_eq!(scope.pending_jobs(None, 10).await.unwrap(), original);
    service
        .complete(&worker, &task.id, &task.token, execution)
        .await
        .unwrap();
    assert_eq!(scope.pending_jobs(None, 10).await.unwrap().len(), 2);
}

async fn frontiers(store: Rc<OrmStore>, _faults: &FaultDb) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let initial = scope.pending_jobs(None, 1).await.unwrap().remove(0);
    let worker = WorkerIdentity::new("frontier-worker".into()).unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    service
        .heartbeat(&worker, &task.id, &task.token)
        .await
        .unwrap();
    assert_eq!(
        scope.pending_jobs(None, 10).await.unwrap(),
        std::slice::from_ref(&initial)
    );
    let execution = WorkflowExecution::from_runtime_value(json!({"outcomes":[{
        "kind":"Wait", "ordinal":0, "name":"approval", "nameOccurrence":0,
        "signalType":"approved",
        "wakeAt":chrono::Utc::now() + chrono::Duration::hours(1)
    }]}))
    .unwrap();
    service
        .complete(&worker, &task.id, &task.token, execution.clone())
        .await
        .unwrap();
    let sleeping = scope.pending_jobs(None, 10).await.unwrap();
    assert_eq!(sleeping.len(), 2);
    let timer = sleeping.iter().find(|job| job.id != initial.id).unwrap();
    assert!(
        matches!(timer.operation, JobOperation::Advance {generation:0,revision,..} if revision.get() == 2)
    );
    assert!(timer.available_at.get() > initial.available_at.get());
    service
        .complete(&worker, &task.id, &task.token, execution)
        .await
        .unwrap();
    assert_eq!(scope.pending_jobs(None, 10).await.unwrap(), sleeping);
    scope
        .signal(
            &RequestId::mint(),
            &run.id,
            SignalOptions {
                signal_type: "approved".into(),
                payload: json!({"private":true}),
            },
        )
        .await
        .unwrap();
    let awake = scope.pending_jobs(None, 10).await.unwrap();
    assert_eq!(awake.len(), 3);
    let wake = awake.iter().find(|job| matches!(job.operation, JobOperation::Advance {revision,..} if revision.get() == 3)).unwrap();
    assert!(wake.available_at.get() < timer.available_at.get());
    scope
        .transition(&RequestId::mint(), &run.id, RunOperation::Pause)
        .await
        .unwrap();
    assert_eq!(scope.pending_jobs(None, 10).await.unwrap(), awake);
    scope
        .restart(&RequestId::mint(), &run.id, RestartOptions::default())
        .await
        .unwrap();
    let all = scope.pending_jobs(None, 10).await.unwrap();
    assert_eq!(all.len(), 4);
    assert!(all.iter().any(|job| matches!(job.operation, JobOperation::Advance {generation:1,revision,..} if revision.get() == 1)));
    let mut paged = Vec::new();
    loop {
        let page = scope
            .pending_jobs(paged.last().map(|job: &JobSpec| &job.id), 1)
            .await
            .unwrap();
        if page.is_empty() {
            break;
        }
        paged.extend(page);
    }
    assert_eq!(paged, all);
}

async fn retention(store: Rc<OrmStore>, _faults: &FaultDb) {
    let (service, app, other, platform) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let job = scope.pending_jobs(None, 1).await.unwrap().remove(0);
    let worker = WorkerIdentity::new("retention-worker".into()).unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            WorkflowExecution::from_runtime_value(json!({"outcomes":[{"kind":"RunCompleted"}]}))
                .unwrap(),
        )
        .await
        .unwrap();
    let replacement = platform.deploy(&app).await;
    service.activate_deploy(&app, &replacement).await.unwrap();

    // Model later history collection without erasing publication responsibility.
    let mut tx = service.begin().await.unwrap();
    super::super::app::lock_app(&mut tx, &app).await.unwrap();
    for table in [
        "tasks",
        "continuation_members",
        "continuation_heads",
        "generations",
        "runs",
    ] {
        tx.database()
            .collection(&format!("__zeroship_workflow_{table}"))
            .unwrap()
            .execute(zeroship_data_orm::orm::Operation::Purge {
                filter: zeroship_data_orm::value!({"app_id":app.as_str()}),
                many: true,
            })
            .await
            .unwrap();
    }
    journal_update(
        &tx,
        "job_publications",
        json!({"app_id":app.as_str()}),
        json!({"created_at":0}),
    )
    .await;
    tx.commit().await.unwrap();
    assert_eq!(
        scope.pending_jobs(None, 10).await.unwrap(),
        std::slice::from_ref(&job)
    );
    let client = platform.client(&app);
    assert_eq!(
        service
            .release_deployment_hold(&app, job.deployment_id().unwrap().as_str(), &client)
            .await,
        Err(WorkflowServiceError::Conflict(
            "deployment retains unpublished workflow jobs".into()
        ))
    );

    let manager = Manager::new(&app).await;
    let publisher = Publisher::new(&app, manager.queue.clone());
    let mut changed = job.clone();
    changed.app_id = other;
    let tx = service.begin().await.unwrap();
    journal_update(
        &tx,
        "job_publications",
        json!({"id":job.id.as_str()}),
        json!({"specification":serde_json::to_string(&changed).unwrap()}),
    )
    .await;
    tx.commit().await.unwrap();
    assert!(scope.pending_jobs(None, 10).await.is_err());
    assert!(scope.publish_job(&job.id, &publisher).await.is_err());
    assert_eq!(publisher.calls.get(), 0);
    let tx = service.begin().await.unwrap();
    journal_update(
        &tx,
        "job_publications",
        json!({"id":job.id.as_str()}),
        json!({"specification":serde_json::to_string(&job).unwrap()}),
    )
    .await;
    tx.commit().await.unwrap();
    scope.publish_job(&job.id, &publisher).await.unwrap();
    service
        .release_deployment_hold(&app, job.deployment_id().unwrap().as_str(), &client)
        .await
        .unwrap();
    assert!(scope.pending_jobs(None, 10).await.unwrap().is_empty());
    let tx = service.begin().await.unwrap();
    assert_eq!(
        journal_count(&tx, "job_publications", json!({"id":job.id.as_str()})).await,
        1
    );
    tx.commit().await.unwrap();
}
