use async_trait::async_trait;
use serde_json::json;
use std::{cell::RefCell, rc::Rc, sync::Arc, time::Duration};
use zeroship_core::{app_id::AppId, typed_id};
use zeroship_runtime::{runtime::InnerProbe, EnvSnapshot, ModuleEntry, Runtime};
use zeroship_workflow::{
    operations::{RunOperation, RunState, SignalOptions, StartOptions},
    service::{
        runner::{RunnerOutcome, RunnerSlot},
        schema,
        store::SqliteStore,
        AppPolicy, AppWorkflows, CompletionReceipt, DeployRegistration, RequestId, TaskAssignment,
        WorkerIdentity, WorkflowService,
    },
    WorkflowServiceError,
};
use zeroship_workflow_v8::{LoadedWorkflow, V8TaskExecutor, WorkflowRuntimeLoader};

struct Loader {
    source: String,
    probes: RefCell<Vec<InnerProbe>>,
}
#[async_trait(?Send)]
impl WorkflowRuntimeLoader for Loader {
    async fn load(
        &self,
        assignment: &TaskAssignment,
    ) -> Result<LoadedWorkflow, WorkflowServiceError> {
        assert_eq!(assignment.invocation.deploy_hash, "a".repeat(64));
        assert!(!serde_json::to_string(&assignment.invocation)
            .unwrap()
            .contains(assignment.token.as_str()));
        let app = AppId::parse(&assignment.invocation.app_id).unwrap();
        zeroship_runtime::init_v8();
        let runtime = Runtime::builder()
            .modules(vec![ModuleEntry {
                specifier: "index.js".into(),
                source: self.source.clone(),
            }])
            .app_id(app.uuid())
            .cpu_limit(Duration::from_millis(100))
            .build();
        self.probes
            .borrow_mut()
            .push(runtime.clone().into_inner_probe_for_test());
        Ok(LoadedWorkflow::new(runtime, EnvSnapshot::empty()))
    }
}
struct Fixture {
    _dir: tempfile::TempDir,
    service: WorkflowService,
    app: AppWorkflows,
    loader: Rc<Loader>,
}
impl Fixture {
    async fn new(source: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow.sqlite");
        schema::initialize_sqlite(&path).unwrap();
        let service = WorkflowService::open(Arc::new(SqliteStore::new(path)))
            .await
            .unwrap();
        let app = AppId::mint();
        service
            .register_app(&app, &AppPolicy::default())
            .await
            .unwrap();
        service
            .activate_deploy(
                &app,
                &DeployRegistration {
                    id: typed_id::generate("dep"),
                    hash: "a".repeat(64),
                    workflows: ["Example".into()].into(),
                    schedules: Vec::new(),
                },
            )
            .await
            .unwrap();
        Self {
            _dir: dir,
            app: service.for_app(app),
            service,
            loader: Rc::new(Loader {
                source: source.into(),
                probes: RefCell::new(Vec::new()),
            }),
        }
    }
    fn runner(&self, timeout: Duration) -> RunnerSlot {
        RunnerSlot::new(
            Rc::new(
                self.service
                    .tasks(WorkerIdentity::new("local-v8-worker".into()).unwrap()),
            ),
            Rc::new(V8TaskExecutor::new(self.loader.clone())),
            timeout,
        )
        .unwrap()
    }
    async fn assert_disposed(&self) {
        compio::time::timeout(Duration::from_secs(5), async {
            while self
                .loader
                .probes
                .borrow()
                .iter()
                .any(|probe| probe.strong_count() != 0)
            {
                compio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("finished workflow isolates must be disposed");
        assert!(!self.loader.probes.borrow().is_empty());
    }
}

async fn advance_until_suspended(runner: &mut RunnerSlot) -> CompletionReceipt {
    for _ in 0..16 {
        let RunnerOutcome::Completed(receipt) = runner.run_once().await.unwrap() else {
            panic!("an accepted workflow must have a runnable frontier");
        };
        if !matches!(receipt.state, RunState::Queued | RunState::Running) {
            return receipt;
        }
    }
    panic!("workflow did not reach a wait or terminal state");
}

#[compio::test]
async fn native_runner_executes_v8_and_commits_the_workflow_frontier() {
    let fixture = Fixture::new(
        r#"
        export class Example {
            async run(trigger, step) {
                const squared = await step.run('square', () => trigger.input.number ** 2);
                return { squared };
            }
        }
        export default { workflows: { Example } };
    "#,
    )
    .await;
    let run = fixture
        .app
        .start(
            &RequestId::mint(),
            "Example",
            StartOptions {
                input: json!({"number":7}),
                ..StartOptions::default()
            },
        )
        .await
        .unwrap();
    let mut runner = fixture.runner(Duration::from_secs(5));
    let receipt = advance_until_suspended(&mut runner).await;
    assert_eq!(receipt.run_id, run.id);
    assert_eq!(receipt.state, RunState::Completed);
    assert_eq!(
        fixture.app.status(&run.id).await.unwrap().output,
        Some(json!({"squared":49}))
    );
    fixture.assert_disposed().await;
}

#[compio::test]
async fn native_runner_reloads_v8_to_resume_a_durable_signal_wait() {
    let fixture = Fixture::new(
        r#"
        export class Example {
            async run(_trigger, step) {
                const prepared = await step.run('prepare', () => ({ready:true}));
                const signal = await step.waitForSignal('approved', { timeout: '1h' });
                return { prepared, payload: signal.payload };
            }
        }
        export default { workflows: { Example } };
    "#,
    )
    .await;
    let run = fixture
        .app
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let mut runner = fixture.runner(Duration::from_secs(5));
    let waiting = advance_until_suspended(&mut runner).await;
    assert_eq!(waiting.state, RunState::Waiting);
    fixture.assert_disposed().await;
    let before_signal = fixture.loader.probes.borrow().len();
    fixture
        .app
        .signal(
            &RequestId::mint(),
            &run.id,
            SignalOptions {
                signal_type: "approved".into(),
                payload: json!({"accepted":true}),
            },
        )
        .await
        .unwrap();
    let done = advance_until_suspended(&mut runner).await;
    assert_eq!(done.state, RunState::Completed);
    assert_eq!(
        fixture.app.status(&run.id).await.unwrap().output,
        Some(json!({"prepared":{"ready":true},"payload":{"accepted":true}}))
    );
    assert!(fixture.loader.probes.borrow().len() > before_signal);
    fixture.assert_disposed().await;
}

#[compio::test]
async fn native_runner_timeout_disposes_v8_before_reusing_its_slot() {
    let fixture = Fixture::new(
        r#"
        export class Example {
            async run(trigger, step) {
                return await step.run('work', () => trigger.input.hang
                    ? new Promise(resolve => setTimeout(() => resolve('late'), 60000))
                    : 'finished');
            }
        }
        export default { workflows: { Example } };
    "#,
    )
    .await;
    let blocked = fixture
        .app
        .start(
            &RequestId::mint(),
            "Example",
            StartOptions {
                input: json!({"hang":true}),
                ..StartOptions::default()
            },
        )
        .await
        .unwrap();
    let mut runner = fixture.runner(Duration::from_millis(30));
    assert!(matches!(
        runner.run_once().await,
        Err(WorkflowServiceError::Timeout)
    ));
    fixture.assert_disposed().await;
    fixture
        .app
        .transition(&RequestId::mint(), &blocked.id, RunOperation::Cancel)
        .await
        .unwrap();
    let next = fixture
        .app
        .start(
            &RequestId::mint(),
            "Example",
            StartOptions {
                input: json!({"hang":false}),
                ..StartOptions::default()
            },
        )
        .await
        .unwrap();
    let done = advance_until_suspended(&mut runner).await;
    assert_eq!(done.run_id, next.id);
    assert_eq!(
        fixture.app.status(&next.id).await.unwrap().output,
        Some(json!("finished"))
    );
    fixture.assert_disposed().await;
}
