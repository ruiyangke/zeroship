use super::*;
use crate::{
    operations::{RunOperation, RunState},
    service::{
        runner::{
            ExecutionBudget, RunnerOutcome, RunnerSlot, TaskExecution, TaskExecutor, TaskTransport,
            WorkerTasks,
        },
        CompletionReceipt, ControlIntent, Heartbeat, TaskAssignment, TaskToken, WorkerIdentity,
    },
    WorkflowExecution,
};
use async_trait::async_trait;
use futures::{channel::oneshot, future::Either, FutureExt};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

#[derive(Clone, Copy)]
enum ExecutionMode {
    Complete,
    Delayed,
    Pending,
}
struct Probe {
    events: RefCell<Vec<&'static str>>,
    mode: Cell<ExecutionMode>,
    stopped: Cell<bool>,
    gate: RefCell<Option<oneshot::Receiver<()>>>,
}
impl Probe {
    fn record(&self, event: &'static str) {
        self.events.borrow_mut().push(event);
    }
    fn count(&self, event: &str) -> usize {
        self.events
            .borrow()
            .iter()
            .filter(|seen| **seen == event)
            .count()
    }
    async fn observed(&self, event: &str) {
        compio::time::timeout(Duration::from_secs(5), async {
            while self.count(event) == 0 {
                compio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("did not observe {event}"));
    }
    fn block_stop(&self) -> oneshot::Sender<()> {
        let (sender, receiver) = oneshot::channel();
        *self.gate.borrow_mut() = Some(receiver);
        sender
    }
}
struct Executor(Rc<Probe>);
impl TaskExecutor for Executor {
    fn start(
        &self,
        _: &TaskAssignment,
        _: ExecutionBudget,
    ) -> Result<Box<dyn TaskExecution>, WorkflowServiceError> {
        self.0.record("start");
        self.0.stopped.set(false);
        Ok(Box::new(Execution {
            probe: self.0.clone(),
            mode: self.0.mode.get(),
            gate: self.0.gate.borrow_mut().take(),
        }))
    }
}
struct Execution {
    probe: Rc<Probe>,
    mode: ExecutionMode,
    gate: Option<oneshot::Receiver<()>>,
}
#[async_trait(?Send)]
impl TaskExecution for Execution {
    async fn wait(&mut self) -> Result<WorkflowExecution, WorkflowServiceError> {
        self.probe.record("execute");
        match self.mode {
            ExecutionMode::Complete => {}
            ExecutionMode::Delayed => compio::time::sleep(Duration::from_millis(900)).await,
            ExecutionMode::Pending => std::future::pending().await,
        }
        WorkflowExecution::from_runtime_value(
            json!({"outcomes":[{"kind":"RunCompleted","output":{"done":true}}]}),
        )
    }
    fn cancel(&mut self) {
        self.probe.record("cancel");
    }
    async fn stop(&mut self) {
        if self.probe.stopped.get() {
            return;
        }
        self.probe.record("stopping");
        if let Some(gate) = self.gate.as_mut() {
            gate.await.unwrap();
        }
        self.gate = None;
        self.probe.stopped.set(true);
        self.probe.record("stopped");
    }
}
struct ObservedTasks {
    inner: WorkerTasks,
    app: crate::service::AppWorkflows,
    probe: Rc<Probe>,
    control: Cell<Option<RunOperation>>,
    lose_authority: Cell<bool>,
    stall_heartbeat: Cell<bool>,
    lose_completion: Cell<bool>,
    fail_completion: Cell<bool>,
    poll_delay: Cell<Duration>,
    completions: RefCell<Vec<String>>,
    run: RefCell<Option<String>>,
}
#[async_trait(?Send)]
impl TaskTransport for ObservedTasks {
    async fn poll(&self) -> Result<Option<TaskAssignment>, WorkflowServiceError> {
        self.probe.record("poll");
        let mut task = self.inner.poll().await?;
        if !self.poll_delay.get().is_zero() {
            compio::time::sleep(self.poll_delay.get()).await;
        }
        if let Some(task) = &mut task {
            *self.run.borrow_mut() = Some(task.invocation.run_id.clone());
            assert!(task.lease_ms > 0);
            // Simulate unrelated wall-clock epochs. Only the granted duration
            // and transport elapsed time can authorize local execution time.
            task.deadline = 0;
        }
        Ok(task)
    }
    async fn heartbeat(
        &self,
        task: &str,
        token: &TaskToken,
    ) -> Result<Heartbeat, WorkflowServiceError> {
        self.probe.record("heartbeat");
        if self.stall_heartbeat.get() {
            return std::future::pending().await;
        }
        if self.lose_authority.get() {
            return Err(WorkflowServiceError::Unauthenticated);
        }
        if let Some(operation) = self.control.take() {
            let run = self.run.borrow().clone().unwrap();
            self.app
                .transition(&RequestId::mint(), &run, operation)
                .await?;
        }
        self.inner.heartbeat(task, token).await
    }
    async fn complete(
        &self,
        task: &str,
        token: &TaskToken,
        execution: WorkflowExecution,
    ) -> Result<CompletionReceipt, WorkflowServiceError> {
        assert!(
            self.probe.stopped.get(),
            "publishing requires stopped execution"
        );
        self.probe.record("complete");
        self.completions
            .borrow_mut()
            .push(serde_json::to_string(&execution).unwrap());
        if self.fail_completion.get() {
            return Err(WorkflowServiceError::Unavailable(
                "completion unavailable".into(),
            ));
        }
        let receipt = self.inner.complete(task, token, execution).await?;
        if self.lose_completion.replace(false) {
            return Err(WorkflowServiceError::Unavailable(
                "lost committed completion response".into(),
            ));
        }
        Ok(receipt)
    }
    async fn release(&self, task: &str, token: &TaskToken) -> Result<(), WorkflowServiceError> {
        assert!(
            self.probe.stopped.get(),
            "release requires stopped execution"
        );
        self.probe.record("release");
        self.inner.release(task, token).await
    }
}
struct Harness {
    tasks: Rc<ObservedTasks>,
    probe: Rc<Probe>,
    run: String,
}
impl Harness {
    async fn new(store: Arc<dyn WorkflowStore>) -> Self {
        let (service, app, _) = registered_service(store).await;
        service
            .register_app(
                &app,
                super::configured_policy(
                    2,
                    AppPolicy {
                        lease_ms: 600,
                        ..AppPolicy::default()
                    },
                ),
            )
            .await
            .unwrap();
        let app = service.for_app(app);
        let run = app
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await
            .unwrap()
            .id;
        let probe = Rc::new(Probe {
            events: RefCell::new(Vec::new()),
            mode: Cell::new(ExecutionMode::Complete),
            stopped: Cell::new(true),
            gate: RefCell::new(None),
        });
        let tasks = Rc::new(ObservedTasks {
            inner: service.tasks(WorkerIdentity::new("local-test-worker".into()).unwrap()),
            app,
            probe: probe.clone(),
            control: Cell::new(None),
            lose_authority: Cell::new(false),
            stall_heartbeat: Cell::new(false),
            lose_completion: Cell::new(false),
            fail_completion: Cell::new(false),
            poll_delay: Cell::new(Duration::ZERO),
            completions: RefCell::new(Vec::new()),
            run: RefCell::new(None),
        });
        Self { tasks, probe, run }
    }
    fn slot(&self, timeout: Duration) -> RunnerSlot {
        RunnerSlot::new(
            self.tasks.clone(),
            Rc::new(Executor(self.probe.clone())),
            timeout,
        )
        .unwrap()
    }
}
async fn sqlite() -> (tempfile::TempDir, Harness) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    let harness = Harness::new(Arc::new(SqliteStore::new(path))).await;
    (dir, harness)
}

async fn completion_contract(harness: Harness) {
    harness.probe.mode.set(ExecutionMode::Delayed);
    harness.tasks.lose_completion.set(true);
    let mut slot = harness.slot(Duration::from_secs(5));
    let RunnerOutcome::Completed(receipt) = slot.run_once().await.unwrap() else {
        panic!("task must complete");
    };
    assert_eq!(receipt.run_id, harness.run);
    assert_eq!(harness.probe.count("execute"), 1);
    assert!(harness.probe.count("heartbeat") > 0);
    {
        let completions = harness.tasks.completions.borrow();
        assert_eq!(completions.len(), 2);
        assert_eq!(
            completions[0], completions[1],
            "lost receipt retries identical outcomes"
        );
    }
    assert_eq!(
        harness.tasks.app.status(&harness.run).await.unwrap().state,
        RunState::Completed
    );
    assert!(matches!(
        slot.run_once().await.unwrap(),
        RunnerOutcome::Idle
    ));
}

#[compio::test]
async fn sqlite_runner_renews_and_recovers_completion_without_reexecution() {
    let (_dir, harness) = sqlite().await;
    completion_contract(harness).await;
}
#[compio::test]
async fn postgres_runner_renews_and_recovers_completion_without_reexecution() {
    let fixture = PostgresFixture::start().await;
    completion_contract(Harness::new(Arc::new(fixture.store.clone())).await).await;
}

#[compio::test]
async fn runner_control_waits_for_stopped_execution_before_releasing() {
    let (_dir, harness) = sqlite().await;
    harness.probe.mode.set(ExecutionMode::Pending);
    harness.tasks.control.set(Some(RunOperation::Pause));
    let gate = harness.probe.block_stop();
    let mut slot = harness.slot(Duration::from_secs(5));
    let observer = async {
        harness.probe.observed("stopping").await;
        assert!(harness.probe.count("cancel") > 0);
        assert_eq!(harness.probe.count("release"), 0);
        assert_eq!(harness.probe.count("complete"), 0);
        gate.send(()).unwrap();
    };
    let (result, ()) = futures::join!(slot.run_once(), observer);
    assert!(matches!(
        result.unwrap(),
        RunnerOutcome::Interrupted(ControlIntent::Pause)
    ));
    assert_eq!(harness.probe.count("release"), 1);
    assert!(matches!(
        slot.run_once().await.unwrap(),
        RunnerOutcome::Idle
    ));
    assert_eq!(
        harness.tasks.app.status(&harness.run).await.unwrap().state,
        RunState::Paused
    );
}

#[compio::test]
async fn runner_timeout_waits_for_stopped_execution_before_releasing() {
    let (_dir, harness) = sqlite().await;
    harness.probe.mode.set(ExecutionMode::Pending);
    let gate = harness.probe.block_stop();
    let mut slot = harness.slot(Duration::from_millis(20));
    let observer = async {
        harness.probe.observed("stopping").await;
        assert_eq!(harness.probe.count("release"), 0);
        assert_eq!(harness.probe.count("complete"), 0);
        gate.send(()).unwrap();
    };
    let (result, ()) = futures::join!(slot.run_once(), observer);
    assert!(matches!(result, Err(WorkflowServiceError::Timeout)));
    assert_eq!(harness.probe.count("release"), 1);
}

#[compio::test]
async fn runner_lost_authority_stops_without_publishing_or_releasing() {
    let (_dir, harness) = sqlite().await;
    harness.probe.mode.set(ExecutionMode::Pending);
    harness.tasks.lose_authority.set(true);
    let mut slot = harness.slot(Duration::from_secs(5));
    assert!(matches!(
        slot.run_once().await,
        Err(WorkflowServiceError::Unauthenticated)
    ));
    assert!(harness.probe.stopped.get());
    assert!(harness.probe.count("cancel") > 0);
    assert_eq!(harness.probe.count("complete"), 0);
    assert_eq!(harness.probe.count("release"), 0);
}

#[compio::test]
async fn runner_stalled_heartbeat_stops_at_the_granted_lease_budget() {
    let (_dir, harness) = sqlite().await;
    harness.probe.mode.set(ExecutionMode::Pending);
    harness.tasks.stall_heartbeat.set(true);
    let mut slot = harness.slot(Duration::from_secs(5));
    let result = compio::time::timeout(Duration::from_secs(2), slot.run_once())
        .await
        .expect("a stalled heartbeat must not run until the execution timeout");
    assert!(matches!(result, Err(WorkflowServiceError::Timeout)));
    assert!(harness.probe.stopped.get());
    assert!(harness.probe.count("cancel") > 0);
    assert_eq!(harness.probe.count("complete"), 0);
    assert_eq!(harness.probe.count("release"), 0);
}

#[compio::test]
async fn runner_discards_assignments_whose_transport_consumed_the_lease() {
    let (_dir, harness) = sqlite().await;
    harness.tasks.poll_delay.set(Duration::from_millis(700));
    let mut slot = harness.slot(Duration::from_secs(5));
    assert!(matches!(
        slot.run_once().await,
        Err(WorkflowServiceError::Timeout)
    ));
    assert_eq!(harness.probe.count("start"), 0);
    assert_eq!(harness.probe.count("execute"), 0);
    assert_eq!(harness.probe.count("complete"), 0);
}

#[compio::test]
async fn runner_completion_retries_remain_bounded_while_heartbeats_succeed() {
    let (_dir, harness) = sqlite().await;
    harness.tasks.fail_completion.set(true);
    let mut slot = harness.slot(Duration::from_millis(400));
    let result = compio::time::timeout(Duration::from_secs(2), slot.run_once())
        .await
        .expect("renewal must not make completion retries unbounded");
    assert!(matches!(result, Err(WorkflowServiceError::Timeout)));
    assert_eq!(harness.probe.count("execute"), 1);
    assert!(harness.probe.count("complete") > 1);
    assert!(harness.probe.count("heartbeat") > 0);
    assert!(harness.probe.stopped.get());
}

#[compio::test]
async fn cancelled_runner_drains_its_old_execution_before_polling_again() {
    let (_dir, harness) = sqlite().await;
    harness.probe.mode.set(ExecutionMode::Pending);
    let gate = harness.probe.block_stop();
    let mut slot = harness.slot(Duration::from_secs(5));
    match futures::future::select(
        slot.run_once().boxed_local(),
        harness.probe.observed("execute").boxed_local(),
    )
    .await
    {
        Either::Left(_) => panic!("execution must still be pending"),
        Either::Right(((), running)) => drop(running),
    }
    assert!(
        harness.probe.count("cancel") > 0,
        "dropping the caller signals cancellation"
    );
    assert!(!harness.probe.stopped.get());
    harness.probe.mode.set(ExecutionMode::Complete);
    let observer = async {
        harness.probe.observed("stopping").await;
        assert_eq!(harness.probe.count("poll"), 1);
        assert_eq!(harness.probe.count("start"), 1);
        assert_eq!(harness.probe.count("release"), 0);
        gate.send(()).unwrap();
    };
    let (result, ()) = futures::join!(slot.run_once(), observer);
    assert!(matches!(result.unwrap(), RunnerOutcome::Completed(_)));
    assert_eq!(harness.probe.count("execute"), 2);
    assert_eq!(harness.probe.count("release"), 1);
}
