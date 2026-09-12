use async_trait::async_trait;
use serde_json::json;
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, Mutex},
    time::Duration,
};
use zeroship_core::{app_id::AppId, typed_id};
use zeroship_runtime::plugin::{NativePlugin, NativeRegistrar};
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

#[derive(Clone, Default)]
struct Markers(Arc<Mutex<Vec<String>>>);
impl NativePlugin for Markers {
    fn namespace(&self) -> &str {
        "probe"
    }
    fn register(&self, registrar: &mut NativeRegistrar) {
        let markers = self.clone();
        registrar.add_setup("markers", move |scope, _| {
            scope.set_slot(markers.clone());
        });
        registrar.add(
            "mark",
            |scope: &mut v8::PinScope,
             args: v8::FunctionCallbackArguments,
             _rv: v8::ReturnValue| {
                let marker = args.get(0).to_rust_string_lossy(scope);
                scope
                    .get_slot::<Markers>()
                    .unwrap()
                    .0
                    .lock()
                    .unwrap()
                    .push(marker);
            },
        );
    }
}

const BURN: &str = r"
    import { env } from 'zeroship';
    function burn() {
        env.probe.mark('entered');
        let n = 0;
        for (let i = 0; i < 300000000; i++) n = (n + 1) % 2147483647;
        env.probe.mark('escaped');
        return n;
    }
";

struct Loader {
    source: String,
    probes: RefCell<Vec<InnerProbe>>,
    cpu_limit: Option<Duration>,
    markers: Markers,
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
        let mut builder = Runtime::builder()
            .modules(vec![ModuleEntry {
                specifier: "index.js".into(),
                source: self.source.clone(),
            }])
            .plugins(vec![Arc::new(self.markers.clone())])
            .app_id(app.uuid());
        if let Some(limit) = self.cpu_limit {
            builder = builder.cpu_limit(limit);
        }
        let runtime = builder.build();
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
        Self::with_limits(
            source,
            Some(Duration::from_millis(100)),
            AppPolicy::default(),
        )
        .await
    }
    async fn with_limits(source: &str, cpu_limit: Option<Duration>, policy: AppPolicy) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow.sqlite");
        schema::initialize_sqlite(&path).unwrap();
        let service = WorkflowService::open(Arc::new(SqliteStore::new(path)))
            .await
            .unwrap();
        let app = AppId::mint();
        service.register_app(&app, &policy).await.unwrap();
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
                cpu_limit,
                markers: Markers::default(),
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

async fn assert_deadline_interrupts(source: &str, timeout: Duration, policy: AppPolicy) {
    // Disable the CPU timer: only the host's monotonic deadline may interrupt.
    let fixture = Fixture::with_limits(&format!("{BURN}\n{source}"), None, policy).await;
    let run = fixture
        .app
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let mut runner = fixture.runner(timeout);
    let result = runner.run_once().await;
    assert!(
        matches!(result, Err(WorkflowServiceError::Timeout)),
        "{result:?}"
    );
    assert_eq!(
        *fixture.loader.markers.0.lock().unwrap(),
        ["entered"],
        "the watchdog must interrupt the loop before its final native effect"
    );
    assert_ne!(
        fixture.app.status(&run.id).await.unwrap().state,
        RunState::Completed
    );
    fixture.assert_disposed().await;
}

#[compio::test]
async fn deadline_interrupts_module_initialization_without_an_async_yield() {
    assert_deadline_interrupts(
        r"
            const n = burn();
            export class Example { run() { return n; } }
        ",
        Duration::from_millis(50),
        AppPolicy::default(),
    )
    .await;
}

#[compio::test]
async fn deadline_interrupts_synchronous_workflow_code_without_a_cpu_timer() {
    assert_deadline_interrupts(
        r"
            export class Example {
                run() {
                    return burn();
                }
            }
        ",
        Duration::from_millis(50),
        AppPolicy::default(),
    )
    .await;
}

#[compio::test]
async fn deadline_interrupts_a_synchronous_timer_callback() {
    assert_deadline_interrupts(
        r"
            export class Example {
                async run(_trigger, step) {
                    return await step.run('burn', () => new Promise(resolve => setTimeout(() => {
                        resolve(burn());
                    }, 1)));
                }
            }
        ",
        Duration::from_millis(50),
        AppPolicy::default(),
    )
    .await;
}

#[compio::test]
async fn confirmed_lease_bounds_synchronous_execution_before_its_timeout() {
    assert_deadline_interrupts(
        r"
            export class Example {
                run() {
                    return burn();
                }
            }
        ",
        Duration::from_secs(10),
        AppPolicy {
            lease_ms: 100,
            ..AppPolicy::default()
        },
    )
    .await;
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
async fn control_bounded_loop_completes_when_the_deadline_allows_it() {
    let source = format!("{BURN}\nexport class Example {{ run() {{ return burn(); }} }}");
    let fixture = Fixture::with_limits(&source, None, AppPolicy::default()).await;
    fixture
        .app
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let done = advance_until_suspended(&mut fixture.runner(Duration::from_secs(30))).await;
    assert_eq!(done.state, RunState::Completed);
    assert_eq!(
        *fixture.loader.markers.0.lock().unwrap(),
        ["entered", "escaped"]
    );
    fixture.assert_disposed().await;
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
