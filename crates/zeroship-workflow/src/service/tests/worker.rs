#![expect(
    clippy::future_not_send,
    reason = "worker fixtures exercise storage and executors on their compio thread"
)]

use super::*;
use crate::{
    operations::RunState,
    service::{
        runner::{ExecutionBudget, TaskExecution, TaskExecutor, WorkerOptions, WorkflowWorker},
        AppWorkflows, TaskAssignment, WorkerIdentity,
    },
    WorkflowExecution,
};
use async_trait::async_trait;
use futures::{channel::oneshot, FutureExt};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

mod deployment_holds;

#[derive(Default)]
struct Probe {
    active: Cell<usize>,
    maximum: Cell<usize>,
    started: RefCell<Vec<String>>,
    fail_first: Cell<bool>,
    pending: Cell<bool>,
    gate_stop: Cell<bool>,
    stops: RefCell<Vec<oneshot::Sender<()>>>,
    gate_execution: Cell<bool>,
    releases: RefCell<Vec<oneshot::Sender<()>>>,
}
struct Executor(Rc<Probe>);
impl TaskExecutor for Executor {
    fn start(
        &self,
        task: &TaskAssignment,
        _: ExecutionBudget,
    ) -> Result<Box<dyn TaskExecution>, WorkflowServiceError> {
        if self.0.fail_first.replace(false) {
            return Err(WorkflowServiceError::Unavailable(
                "injected executor loading failure".into(),
            ));
        }
        self.0
            .started
            .borrow_mut()
            .push(task.invocation.run_id.clone());
        self.0.active.set(self.0.active.get() + 1);
        self.0
            .maximum
            .set(self.0.maximum.get().max(self.0.active.get()));
        let release = self.0.gate_execution.get().then(|| {
            let (send, receive) = oneshot::channel();
            self.0.releases.borrow_mut().push(send);
            receive
        });
        Ok(Box::new(Execution {
            probe: self.0.clone(),
            stopped: false,
            stop: None,
            release,
        }))
    }
}
struct Execution {
    probe: Rc<Probe>,
    stopped: bool,
    stop: Option<oneshot::Receiver<()>>,
    release: Option<oneshot::Receiver<()>>,
}
#[async_trait(?Send)]
impl TaskExecution for Execution {
    async fn wait(&mut self) -> Result<WorkflowExecution, WorkflowServiceError> {
        if let Some(release) = self.release.take() {
            release.await.unwrap();
        }
        if self.probe.pending.get() {
            std::future::pending::<()>().await;
        }
        compio::time::sleep(Duration::from_millis(50)).await;
        WorkflowExecution::from_runtime_value(json!({"outcomes":[{"kind":"RunCompleted"}]}))
    }
    fn cancel(&mut self) {}
    async fn stop(&mut self) {
        if self.stopped {
            return;
        }
        if self.probe.gate_stop.get() && self.stop.is_none() {
            let (send, receive) = oneshot::channel();
            self.probe.stops.borrow_mut().push(send);
            self.stop = Some(receive);
        }
        if let Some(wait) = self.stop.as_mut() {
            wait.await.unwrap();
        }
        self.stopped = true;
        self.probe.active.set(self.probe.active.get() - 1);
    }
}

fn options() -> WorkerOptions {
    WorkerOptions {
        task_slots: 2,
        idle_poll_ms: 5,
        error_backoff_ms: 5,
        maintenance_interval_ms: 10,
        ..WorkerOptions::default()
    }
}
async fn host(
    store: Rc<OrmStore>,
    dir: &Path,
) -> (
    WorkflowService,
    AppWorkflows,
    Rc<Probe>,
    WorkflowWorker,
    Deployments,
) {
    let (service, app, _, deployments) = registered_service(store).await;
    let service = service
        .with_payload_storage(zeroship_storage::StorageStore::from_backend(Arc::new(
            zeroship_storage::LocalFs::new(dir),
        )))
        .unwrap();
    let probe = Rc::new(Probe::default());
    let worker = WorkflowWorker::new(
        Rc::new(service.tasks(WorkerIdentity::new("loop-worker".into()).unwrap())),
        Rc::new(Executor(probe.clone())),
        options(),
    )
    .unwrap();
    let app = service.for_app(app);
    (service, app, probe, worker, deployments)
}
async fn wait_for(mut condition: impl FnMut() -> bool) {
    compio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            compio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("workflow host did not reach its expected state");
}

#[compio::test]
async fn sqlite_worker_runs_bounded_slots_and_retries_without_request_isolates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    capacity_contract(Rc::new(sqlite_store(&path).await), dir.path()).await;
}
#[compio::test]
async fn postgres_worker_runs_bounded_slots_and_retries_without_request_isolates() {
    let fixture = PostgresFixture::start().await;
    let dir = tempfile::tempdir().unwrap();
    capacity_contract(Rc::new(fixture.store.clone()), dir.path()).await;
}
async fn capacity_contract(store: Rc<OrmStore>, dir: &Path) {
    let (service, app, probe, mut worker, deployments) = host(store, dir).await;
    let mut runs = Vec::new();
    for _ in 0..6 {
        runs.push(
            app.start(&RequestId::mint(), "Example", StartOptions::default())
                .await
                .unwrap()
                .id,
        );
    }
    probe.fail_first.set(true);
    probe.gate_execution.set(true);
    compio::time::timeout(
        Duration::from_secs(5),
        worker.run_until(async {
            // Hold executions until the worker fills its capacity; database
            // latency must not decide whether their lifetimes overlap.
            wait_for(|| probe.active.get() == options().task_slots).await;
            probe.gate_execution.set(false);
            for release in probe.releases.borrow_mut().drain(..) {
                release.send(()).unwrap();
            }
            loop {
                let mut complete = true;
                for run in &runs {
                    complete &= app.status(run).await.unwrap().state == RunState::Completed;
                }
                if complete {
                    break;
                }
                compio::time::sleep(Duration::from_millis(5)).await;
            }
        }),
    )
    .await
    .expect("worker did not finish accepted runs");
    assert_eq!(probe.maximum.get(), options().task_slots);
    assert_eq!(probe.active.get(), 0);
    assert_eq!(probe.started.borrow().len(), runs.len());
    maintenance_while_busy(&deployments, &service, &app, &probe, &mut worker).await;
}

#[expect(
    clippy::too_many_lines,
    reason = "the contract follows staging and scheduling through concurrent execution"
)]
async fn maintenance_while_busy(
    deployments: &Deployments,
    service: &WorkflowService,
    app: &AppWorkflows,
    probe: &Probe,
    worker: &mut WorkflowWorker,
) {
    use crate::engine::WorkflowOutputRef;
    use crate::service::{
        IntervalAnchor, ScheduleCatchUp, ScheduleOverlap, ScheduleRegistration, ScheduleTiming,
        WorkerIdentity,
    };
    deployments
        .activate(
            service,
            app.app_id(),
            &DeployRegistration {
                id: typed_id::generate("dep"),
                hash: "b".repeat(64),
                workflows: ["Example".into()].into(),
                schedules: vec![ScheduleRegistration {
                    name: "periodic".into(),
                    workflow_name: "Example".into(),
                    schedule: ScheduleTiming::Interval {
                        interval_ms: 1_000,
                        anchor: IntervalAnchor::Deploy,
                    },
                    input: json!(null),
                    overlap: ScheduleOverlap::default(),
                    catch_up: ScheduleCatchUp::default(),
                }],
            },
        )
        .await
        .unwrap();
    app.start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let writer = WorkerIdentity::new("staging-writer".into()).unwrap();
    let task = service.poll(&writer).await.unwrap().unwrap();
    let body = b"abandoned";
    service
        .stage_payload(
            &writer,
            &task.id,
            &task.token,
            &RequestId::mint(),
            WorkflowOutputRef {
                hash: crate::service::types::hash(body),
                size: i64::try_from(body.len()).unwrap(),
                content_type: None,
            },
            Box::new(zeroship_storage::backend::OnceChunk::new(
                bytes::Bytes::from_static(body),
            )),
        )
        .await
        .unwrap();
    service
        .release(&writer, &task.id, &task.token)
        .await
        .unwrap();
    for _ in 0..options().task_slots {
        app.start(&RequestId::mint(), "Example", StartOptions::default())
            .await
            .unwrap();
    }
    let mut tx = service.begin().await.unwrap();
    let due = tx.now().await.unwrap() - 1;
    tx.execute(
        &format!(
            "UPDATE {} SET next_at=$2 WHERE app_id=$1",
            tx.table("schedules")
        ),
        &[app.app_id().as_str().into(), due.into()],
    )
    .await
    .unwrap();
    tx.execute(
        &format!(
            "UPDATE {} SET expires_at=$2 WHERE app_id=$1",
            tx.table("payloads")
        ),
        &[app.app_id().as_str().into(), due.into()],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    probe.pending.set(true);
    compio::time::timeout(
        Duration::from_secs(5),
        worker.run_until(async {
            loop {
                let mut tx = service.begin().await.unwrap();
                let schedules = tx
                    .query(
                        &format!(
                            "SELECT COUNT(*) AS total FROM {} WHERE app_id=$1",
                            tx.table("occurrences")
                        ),
                        &[app.app_id().as_str().into()],
                    )
                    .await
                    .unwrap();
                let payloads = tx
                    .query(
                        &format!(
                            "SELECT COUNT(*) AS total FROM {} WHERE app_id=$1 AND state='deleted'",
                            tx.table("payloads")
                        ),
                        &[app.app_id().as_str().into()],
                    )
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
                if probe.active.get() == options().task_slots
                    && schedules[0].integer("total").unwrap() > 0
                    && payloads[0].integer("total").unwrap() > 0
                {
                    break;
                }
                compio::time::sleep(Duration::from_millis(5)).await;
            }
        }),
    )
    .await
    .expect("busy execution slots blocked maintenance");
    assert_eq!(probe.active.get(), 0);
}

#[compio::test]
async fn worker_shutdown_joins_executions_before_releasing_claims() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    let (service, app, probe, mut worker, _deployments) =
        host(Rc::new(sqlite_store(&path).await), dir.path()).await;
    for _ in 0..3 {
        app.start(&RequestId::mint(), "Example", StartOptions::default())
            .await
            .unwrap();
    }
    probe.pending.set(true);
    probe.gate_stop.set(true);
    let (send, stop) = oneshot::channel();
    let run = worker
        .run_until(async {
            stop.await.unwrap();
        })
        .boxed_local();
    let check = async {
        wait_for(|| probe.active.get() == options().task_slots).await;
        send.send(()).unwrap();
        wait_for(|| probe.stops.borrow().len() == options().task_slots).await;
        let mut tx = service.begin().await.unwrap();
        let rows = tx
            .query(
                &format!(
                    "SELECT COUNT(*) AS total FROM {} WHERE state='leased'",
                    tx.table("tasks")
                ),
                &[],
            )
            .await
            .unwrap();
        assert_eq!(
            rows[0].integer("total").unwrap(),
            i64::try_from(options().task_slots).unwrap()
        );
        tx.commit().await.unwrap();
        assert_eq!(probe.started.borrow().len(), options().task_slots);
    }
    .boxed_local();
    let remaining = match futures::future::select(run, check).await {
        futures::future::Either::Left(_) => panic!("worker returned before executions stopped"),
        futures::future::Either::Right(((), work)) => work,
    };
    for stop in probe.stops.borrow_mut().drain(..) {
        stop.send(()).unwrap();
    }
    compio::time::timeout(Duration::from_secs(5), remaining)
        .await
        .unwrap();
    assert_eq!(probe.active.get(), 0);
    let mut tx = service.begin().await.unwrap();
    let rows = tx
        .query(
            &format!(
                "SELECT COUNT(*) AS total FROM {} WHERE state='leased'",
                tx.table("tasks")
            ),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows[0].integer("total").unwrap(), 0);
    tx.commit().await.unwrap();
}

#[compio::test]
async fn stopped_worker_does_not_claim_new_work() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    let (_, app, probe, mut worker, _deployments) =
        host(Rc::new(sqlite_store(&path).await), dir.path()).await;
    let run = app
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    worker.run_until(async {}).await;
    assert!(probe.started.borrow().is_empty());
    assert_eq!(app.status(&run.id).await.unwrap().state, RunState::Queued);
}

#[compio::test]
async fn worker_refuses_missing_storage_and_invalid_capacity_before_claiming() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    let (service, app, _, deployments) =
        registered_service(Rc::new(sqlite_store(&path).await)).await;
    let identity = WorkerIdentity::new("invalid-host".into()).unwrap();
    let executor = Rc::new(Executor(Rc::new(Probe::default())));
    assert!(WorkflowWorker::new(
        Rc::new(service.tasks(identity.clone())),
        executor.clone(),
        options()
    )
    .is_err());
    let service = service
        .with_payload_storage(zeroship_storage::StorageStore::from_backend(Arc::new(
            zeroship_storage::LocalFs::new(dir.path()),
        )))
        .unwrap();
    let incomplete = service
        .clone()
        .with_deployments(deployments.binding(&[&app]));
    assert!(matches!(
        WorkflowWorker::new(
            Rc::new(incomplete.tasks(identity.clone())),
            executor.clone(),
            options(),
        ),
        Err(WorkflowServiceError::PermissionDenied)
    ));
    let tasks = Rc::new(service.tasks(identity));
    for options in [
        WorkerOptions {
            task_slots: 0,
            ..options()
        },
        WorkerOptions {
            payload_collection_batch: usize::MAX,
            ..options()
        },
    ] {
        assert!(WorkflowWorker::new(tasks.clone(), executor.clone(), options).is_err());
    }
    let run = service
        .for_app(app.clone())
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    assert_eq!(
        service.for_app(app).status(&run.id).await.unwrap().state,
        RunState::Queued
    );
}
