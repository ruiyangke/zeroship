#![expect(
    clippy::future_not_send,
    reason = "native publication tests use compio"
)]

use super::*;
use crate::{
    operations::{RestartOptions, RunOperation},
    service::{publication::JobPublisher, AppWorkflows, WorkerIdentity},
    WorkflowExecution,
};
use std::{cell::Cell, time::Duration};
use zeroship_core::{
    schema_name::SchemaName,
    workflow_deployments::{HoldGeneration, HoldReceipt, HoldScope, HoldState},
    workflow_jobs::{DeploymentId, JobOperation, JobSpec},
};
use zeroship_data_orm::binding::DbBinding;
use zeroship_workflow_manager::{
    retention::{HoldClient, HoldFuture},
    Options, Queue,
};

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

/// Publication tests isolate the journal outbox from artifact retention. The
/// manager's retention suite supplies a real catalog for deployment safety.
#[derive(Debug)]
struct PublicationHolds;

impl PublicationHolds {
    fn receipt(
        app: &AppId,
        deployment: &DeploymentId,
        generation: HoldGeneration,
        state: HoldState,
    ) -> HoldReceipt {
        HoldReceipt {
            app_id: app.clone(),
            deploy_id: deployment.as_str().into(),
            deploy_hash: zeroship_bundle::sha256_hex(deployment.as_str().as_bytes()),
            holder_id: HoldScope::for_queue(app.clone()).holder().into(),
            generation,
            state,
        }
    }
}

impl HoldClient for PublicationHolds {
    fn acquire<'a>(
        &'a self,
        app: &'a AppId,
        deployment: &'a DeploymentId,
        generation: HoldGeneration,
    ) -> HoldFuture<'a> {
        Box::pin(async move { Ok(Self::receipt(app, deployment, generation, HoldState::Held)) })
    }

    fn release<'a>(
        &'a self,
        app: &'a AppId,
        deployment: &'a DeploymentId,
        generation: HoldGeneration,
    ) -> HoldFuture<'a> {
        Box::pin(async move {
            Ok(Self::receipt(
                app,
                deployment,
                generation,
                HoldState::Released,
            ))
        })
    }
}

pub(in crate::service) struct Manager {
    _directory: tempfile::TempDir,
    path: std::path::PathBuf,
    pub(in crate::service) queue: Queue,
}
impl Manager {
    pub(in crate::service) async fn new(app: &AppId) -> Self {
        Self::with_options(app, Options::default()).await
    }
    pub(super) async fn with_options(app: &AppId, options: Options) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("manager.sqlite");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;")
            .unwrap();
        connection
            .execute_batch(include_str!(
                "../../../../zeroship-workflow-manager/schema/sqlite.sql"
            ))
            .unwrap();
        let queue = Self::open_with_options(&path, options).await;
        queue.register_scope(app).await.unwrap();
        Self {
            _directory: directory,
            path,
            queue,
        }
    }
    async fn open(path: &Path) -> Queue {
        Self::open_with_options(path, Options::default()).await
    }
    async fn open_with_options(path: &Path, options: Options) -> Queue {
        Queue::connect(
            DbBinding::new(
                "workflow_manager",
                "publication-test",
                SchemaName::new("main").unwrap(),
            ),
            &format!("sqlite:{}", path.display()),
            options,
            Rc::new(PublicationHolds),
        )
        .await
        .unwrap()
    }
    fn count(&self) -> i64 {
        rusqlite::Connection::open(&self.path)
            .unwrap()
            .query_row("SELECT count(*) FROM jobs", [], |row| row.get(0))
            .unwrap()
    }
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
    let options = StartOptions {
        input: json!({"secret":"creator-private-input"}),
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
    for table in ["tasks", "generations", "runs"] {
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
