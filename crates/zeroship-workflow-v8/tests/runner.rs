use async_trait::async_trait;
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{Arc, Mutex},
    time::Duration,
};
use zeroship_bundle::LoadedWorker;
use zeroship_core::{app_id::AppId, typed_id};
use zeroship_runtime::plugin::{NativePlugin, NativeRegistrar};
use zeroship_runtime::{runtime::InnerProbe, EnvSnapshot, ModuleEntry, Runtime};
use zeroship_workflow::{
    operations::{RunOperation, RunState, SignalOptions, StartOptions},
    service::{
        runner::{RunnerOutcome, RunnerSlot, TaskPayloadLimits, TaskPayloads, WorkerTasks},
        AppPolicy, AppWorkflows, CompletionReceipt, DeployRegistration, HostPolicies, PayloadRead,
        PayloadSlot, PolicySnapshot, RequestId, StagedPayload, TaskAssignment, TaskToken,
        WorkerIdentity, WorkflowService,
    },
    WorkflowServiceError,
};
use zeroship_workflow_v8::{
    LoadedWorkflow, V8TaskExecutor, WorkflowBinding, WorkflowRuntimeLoader,
};

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
    service: WorkflowService,
    probes: RefCell<Vec<InnerProbe>>,
    cpu_limit: Option<Duration>,
    markers: Markers,
}
#[async_trait(?Send)]
impl WorkflowRuntimeLoader for Loader {
    async fn load(
        &self,
        assignment: &TaskAssignment,
        executable: &LoadedWorker,
    ) -> Result<LoadedWorkflow, WorkflowServiceError> {
        assert!(!serde_json::to_string(&assignment.invocation)
            .unwrap()
            .contains(assignment.token.as_str()));
        let app = AppId::parse(&assignment.invocation.app_id).unwrap();
        zeroship_runtime::init_v8();
        let mut builder = Runtime::builder()
            .modules(
                std::iter::once(executable.entry())
                    .chain(
                        executable
                            .modules()
                            .keys()
                            .map(String::as_str)
                            .filter(|name| *name != executable.entry()),
                    )
                    .map(|specifier| ModuleEntry {
                        specifier: specifier.into(),
                        source: executable.modules()[specifier].clone(),
                    })
                    .collect(),
            )
            .plugins(vec![
                Arc::new(self.markers.clone()),
                Arc::new(WorkflowBinding::service(
                    // Replay reads must use task authority and its own read budget.
                    self.service.for_app(app.clone()).into_backend(1).unwrap(),
                )),
            ])
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
    directory: tempfile::TempDir,
    deployments: deployment_fixture::Deployments,
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
        let service = WorkflowService::open(
            std::rc::Rc::new(orm_fixture::store(dir.path()).await),
            Arc::new(HostPolicies::default()),
        )
        .await
        .unwrap()
        .with_payload_storage(zeroship_storage::StorageStore::from_backend(Arc::new(
            zeroship_storage::LocalFs::new(dir.path().join("payloads")),
        )))
        .unwrap();
        let deployments = deployment_fixture::Deployments::new().await;
        let app = AppId::mint();
        let service = service.with_deployments(deployments.binding(&[&app]));
        service
            .register_app(
                &app,
                PolicySnapshot::configuration(1.try_into().unwrap(), policy).unwrap(),
            )
            .await
            .unwrap();
        let declaration = deployments
            .publish(
                &app,
                &DeployRegistration {
                    id: typed_id::generate("dep"),
                    hash: "a".repeat(64),
                    workflows: ["Example".into()].into(),
                    schedules: Vec::new(),
                },
                &deployment_fixture::Sources::single(source),
            )
            .await
            .unwrap();
        service.activate_deploy(&app, &declaration).await.unwrap();
        Self {
            directory: dir,
            deployments,
            app: service.for_app(app),
            service: service.clone(),
            loader: Rc::new(Loader {
                service,
                probes: RefCell::new(Vec::new()),
                cpu_limit,
                markers: Markers::default(),
            }),
        }
    }
    fn runner(&self, timeout: Duration) -> RunnerSlot {
        self.runner_with_payload_limit(timeout, 1024 * 1024)
    }
    fn runner_with_payload_limit(&self, timeout: Duration, max_payload_bytes: usize) -> RunnerSlot {
        self.runner_with_output_limits(
            timeout,
            TaskPayloadLimits {
                max_inline_bytes: max_payload_bytes,
                max_payload_bytes,
                ..TaskPayloadLimits::default()
            },
        )
    }
    fn runner_with_output_limits(
        &self,
        timeout: Duration,
        limits: TaskPayloadLimits,
    ) -> RunnerSlot {
        let tasks = Rc::new(
            self.service
                .tasks(WorkerIdentity::new("local-v8-worker".into()).unwrap()),
        );
        RunnerSlot::new(
            tasks.clone(),
            Rc::new(V8TaskExecutor::new(self.loader.clone(), tasks, limits).unwrap()),
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

async fn prepare_payload(fixture: &Fixture, continuation: bool) -> String {
    use sha2::{Digest, Sha256};
    use zeroship_workflow::{engine::WorkflowOutputRef, WorkflowExecution};
    fixture
        .app
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let worker = WorkerIdentity::new("fixture-writer".into()).unwrap();
    let task = fixture.service.poll(&worker).await.unwrap().unwrap();
    let bytes = br#"{"secret":"retained"}"#;
    let reference = WorkflowOutputRef {
        hash: format!("{:x}", Sha256::digest(bytes)),
        size: bytes.len() as i64,
        content_type: Some("application/json".into()),
    };
    fixture
        .service
        .stage_payload(
            &worker,
            &task.id,
            &task.token,
            &RequestId::mint(),
            reference.clone(),
            Box::new(zeroship_storage::backend::OnceChunk::new(
                bytes.to_vec().into(),
            )),
        )
        .await
        .unwrap();
    let outcome = if continuation {
        json!({"kind":"ContinueAsNew","inputRef":reference})
    } else {
        json!({"kind":"StepCompleted","ordinal":0,"name":"stored","outputRef":reference})
    };
    let receipt = fixture
        .service
        .complete(
            &worker,
            &task.id,
            &task.token,
            WorkflowExecution::from_runtime_value(json!({"outcomes":[outcome]})).unwrap(),
        )
        .await
        .unwrap();
    receipt.run_id
}

#[compio::test]
async fn native_runner_reads_replay_payloads_through_its_task_authority() {
    let fixture = Fixture::new(
        r"
        export class Example {
            async run(_trigger, step) {
                const saved = await step.run('stored', () => { throw new Error('must replay'); });
                return await saved.json();
            }
        }
    ",
    )
    .await;
    let run = prepare_payload(&fixture, false).await;
    let done = advance_until_suspended(&mut fixture.runner(Duration::from_secs(5))).await;
    assert_eq!(done.run_id, run);
    assert_eq!(done.state, RunState::Completed);
    assert_eq!(
        fixture.app.status(&run).await.unwrap().output,
        Some(json!({"secret":"retained"}))
    );
    fixture.assert_disposed().await;
}

#[compio::test]
async fn native_runner_hydrates_continuation_input_before_entering_v8() {
    let fixture =
        Fixture::new("export class Example { run(trigger) { return trigger.input; } }").await;
    prepare_payload(&fixture, true).await;
    let done = advance_until_suspended(&mut fixture.runner(Duration::from_secs(5))).await;
    assert_eq!(done.state, RunState::Completed);
    assert_eq!(
        fixture.app.status(&done.run_id).await.unwrap().output,
        Some(json!({"secret":"retained"}))
    );
    fixture.assert_disposed().await;
}

#[compio::test]
async fn oversized_input_never_initializes_the_creator_module() {
    let fixture =
        Fixture::new("throw new Error('must not evaluate'); export class Example {}").await;
    prepare_payload(&fixture, true).await;
    let result = fixture
        .runner_with_payload_limit(Duration::from_secs(5), 1)
        .run_once()
        .await;
    assert!(
        matches!(result, Err(WorkflowServiceError::PayloadTooLarge)),
        "{result:?}"
    );
    assert!(fixture.loader.probes.borrow().is_empty());
}

struct UnavailablePayloads(Rc<WorkerTasks>);
#[async_trait(?Send)]
impl zeroship_workflow::service::runner::TaskPayloads for UnavailablePayloads {
    async fn executable(
        &self,
        task: &str,
        token: &TaskToken,
    ) -> Result<LoadedWorker, WorkflowServiceError> {
        TaskPayloads::executable(self.0.as_ref(), task, token).await
    }
    async fn stage(
        &self,
        _task: &str,
        _token: &zeroship_workflow::service::TaskToken,
        _request: &RequestId,
        _reference: zeroship_workflow::engine::WorkflowOutputRef,
        _body: zeroship_storage::backend::BoxChunkSource,
    ) -> Result<zeroship_workflow::service::StagedPayload, WorkflowServiceError> {
        Err(WorkflowServiceError::Unavailable(
            "fixture payload outage".into(),
        ))
    }
    async fn read(
        &self,
        _task: &str,
        _token: &zeroship_workflow::service::TaskToken,
        _reference: &zeroship_workflow::engine::WorkflowOutputRef,
    ) -> Result<zeroship_workflow::service::PayloadRead, WorkflowServiceError> {
        Err(WorkflowServiceError::Unavailable(
            "fixture payload outage".into(),
        ))
    }
}

#[compio::test]
async fn payload_outage_interrupts_app_code_and_leaves_the_frontier_retryable() {
    let fixture = Fixture::new(
        r"
        import { env } from 'zeroship';
        export class Example {
            async run(_trigger, step) {
                const saved = await step.run('stored', () => { throw new Error('must replay'); });
                try { return await saved.json(); }
                catch {
                    env.probe.mark('escaped');
                    return 'caught infrastructure failure';
                }
            }
        }
    ",
    )
    .await;
    let run = prepare_payload(&fixture, false).await;
    let tasks = Rc::new(
        fixture
            .service
            .tasks(WorkerIdentity::new("local-v8-worker".into()).unwrap()),
    );
    let executor = Rc::new(
        V8TaskExecutor::new(
            fixture.loader.clone(),
            Rc::new(UnavailablePayloads(tasks.clone())),
            TaskPayloadLimits::default(),
        )
        .unwrap(),
    );
    let mut runner = RunnerSlot::new(tasks, executor, Duration::from_secs(5)).unwrap();
    let result = runner.run_once().await;
    assert_eq!(
        result.unwrap_err(),
        WorkflowServiceError::Unavailable("fixture payload outage".into())
    );
    assert!(fixture.loader.markers.0.lock().unwrap().is_empty());
    assert!(!fixture.app.status(&run).await.unwrap().state.is_terminal());
    fixture.assert_disposed().await;
    let done = advance_until_suspended(&mut fixture.runner(Duration::from_secs(5))).await;
    assert_eq!(done.state, RunState::Completed);
    assert_eq!(
        fixture.app.status(&run).await.unwrap().output,
        Some(json!({"secret":"retained"}))
    );
    fixture.assert_disposed().await;
}

const OUTPUT_LIMITS: TaskPayloadLimits = TaskPayloadLimits {
    max_inline_bytes: 32,
    max_payload_bytes: 256,
    max_result_bytes: 4096,
};

struct UploadProbe {
    tasks: WorkerTasks,
    loader: Rc<Loader>,
    requests: RefCell<Vec<String>>,
    lose_receipt: Cell<bool>,
    unavailable: bool,
}
#[async_trait(?Send)]
impl TaskPayloads for UploadProbe {
    async fn executable(
        &self,
        task: &str,
        token: &TaskToken,
    ) -> Result<LoadedWorker, WorkflowServiceError> {
        TaskPayloads::executable(&self.tasks, task, token).await
    }
    async fn stage(
        &self,
        task: &str,
        token: &TaskToken,
        request: &RequestId,
        reference: zeroship_workflow::engine::WorkflowOutputRef,
        body: zeroship_storage::backend::BoxChunkSource,
    ) -> Result<StagedPayload, WorkflowServiceError> {
        assert!(!self.loader.probes.borrow().is_empty());
        assert!(
            self.loader
                .probes
                .borrow()
                .iter()
                .all(|probe| probe.strong_count() == 0),
            "app isolates must be disposed before uploading results"
        );
        self.requests.borrow_mut().push(request.as_str().to_owned());
        compio::time::sleep(Duration::from_millis(50)).await;
        if self.unavailable {
            return Err(WorkflowServiceError::Unavailable(
                "fixture upload outage".into(),
            ));
        }
        let receipt = self
            .tasks
            .stage(task, token, request, reference, body)
            .await?;
        if self.lose_receipt.replace(false) {
            Err(WorkflowServiceError::Unavailable(
                "lost upload response".into(),
            ))
        } else {
            Ok(receipt)
        }
    }
    async fn read(
        &self,
        task: &str,
        token: &TaskToken,
        reference: &zeroship_workflow::engine::WorkflowOutputRef,
    ) -> Result<PayloadRead, WorkflowServiceError> {
        self.tasks.read(task, token, reference).await
    }
}

#[compio::test]
async fn output_upload_retry_preserves_the_callback_and_stops_background_app_work() {
    let fixture = Fixture::new(r"
        import { env } from 'zeroship';
        export class Example {
            async run(_trigger, step) {
                const saved = await step.run('saved', {output:{as:'blob', contentType:'application/json'}}, () => {
                    env.probe.mark('callback');
                    setTimeout(() => env.probe.mark('escaped'), 20);
                    return {value:'x'.repeat(64)};
                });
                return (await saved.json()).value.length;
            }
        }
    ").await;
    let run = fixture
        .app
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let tasks = Rc::new(
        fixture
            .service
            .tasks(WorkerIdentity::new("upload-worker".into()).unwrap()),
    );
    let upload = Rc::new(UploadProbe {
        tasks: tasks.as_ref().clone(),
        loader: fixture.loader.clone(),
        requests: RefCell::default(),
        lose_receipt: Cell::new(true),
        unavailable: false,
    });
    let executor = Rc::new(
        V8TaskExecutor::new(fixture.loader.clone(), upload.clone(), OUTPUT_LIMITS).unwrap(),
    );
    let mut runner = RunnerSlot::new(tasks, executor, Duration::from_secs(5)).unwrap();
    let done = advance_until_suspended(&mut runner).await;
    assert_eq!(done.state, RunState::Completed);
    assert_eq!(
        fixture.app.status(&run.id).await.unwrap().output,
        Some(json!(64))
    );
    assert_eq!(*fixture.loader.markers.0.lock().unwrap(), ["callback"]);
    {
        let requests = upload.requests.borrow();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0], requests[1]);
    }
    let bytes = fixture
        .app
        .read_step_output(&run.id, "saved", 0)
        .await
        .unwrap()
        .into_bytes(256)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
        json!({"value":"x".repeat(64)})
    );
    fixture.assert_disposed().await;
}

#[compio::test]
async fn upload_outage_is_bounded_after_disposing_the_app() {
    let fixture = Fixture::new(
        r"
        import { env } from 'zeroship';
        export class Example {
            run() { env.probe.mark('callback'); return 'x'.repeat(64); }
        }
    ",
    )
    .await;
    let run = fixture
        .app
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let tasks = Rc::new(
        fixture
            .service
            .tasks(WorkerIdentity::new("upload-worker".into()).unwrap()),
    );
    let upload = Rc::new(UploadProbe {
        tasks: tasks.as_ref().clone(),
        loader: fixture.loader.clone(),
        requests: RefCell::default(),
        lose_receipt: Cell::new(false),
        unavailable: true,
    });
    let executor = Rc::new(
        V8TaskExecutor::new(fixture.loader.clone(), upload.clone(), OUTPUT_LIMITS).unwrap(),
    );
    let mut runner = RunnerSlot::new(tasks, executor, Duration::from_millis(500)).unwrap();
    assert!(matches!(
        runner.run_once().await,
        Err(WorkflowServiceError::Timeout)
    ));
    assert!(!fixture
        .app
        .status(&run.id)
        .await
        .unwrap()
        .state
        .is_terminal());
    assert_eq!(*fixture.loader.markers.0.lock().unwrap(), ["callback"]);
    assert!(upload.requests.borrow().len() > 1);
    assert!(upload
        .requests
        .borrow()
        .windows(2)
        .all(|pair| pair[0] == pair[1]));
    fixture.assert_disposed().await;
}

#[compio::test]
async fn native_runner_stores_large_root_results_in_service_owned_payloads() {
    let fixture = Fixture::new("export class Example { run() { return 'x'.repeat(64); } }").await;
    let run = fixture
        .app
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let mut runner = fixture.runner_with_output_limits(Duration::from_secs(5), OUTPUT_LIMITS);
    assert_eq!(
        advance_until_suspended(&mut runner).await.state,
        RunState::Completed
    );
    let bytes = fixture
        .app
        .read_payload(&run.id, 0, PayloadSlot::Output)
        .await
        .unwrap()
        .into_bytes(256)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<String>(&bytes).unwrap(),
        "x".repeat(64)
    );
    fixture.assert_disposed().await;
}

#[compio::test]
async fn inline_output_limit_commits_a_terminal_workflow_failure() {
    let fixture = Fixture::new(
        r"
        export class Example {
            async run(_trigger,step) {
                return await step.run('large',{output:'inline'},()=>'x'.repeat(64));
            }
        }
    ",
    )
    .await;
    let run = fixture
        .app
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let mut runner = fixture.runner_with_output_limits(Duration::from_secs(5), OUTPUT_LIMITS);
    assert_eq!(
        advance_until_suspended(&mut runner).await.state,
        RunState::Failed
    );
    let error = fixture.app.status(&run.id).await.unwrap().error.unwrap();
    assert_eq!(error["type"], "LimitExceededError");
    assert_eq!(error["retryable"], false);
    fixture.assert_disposed().await;
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
async fn replay_loads_retained_dependencies_after_redeploy_and_host_restart() {
    let mut fixture = Fixture::new("export default {};").await;
    let app = fixture.app.app_id().clone();
    let graph = |value: &str| {
        deployment_fixture::Sources {
        entry: "entry.js".into(),
        modules: [
            (
                "dependency.js".into(),
                format!("export default {};", serde_json::to_string(value).unwrap()),
            ),
            (
                "entry.js".into(),
                r"
                    import version from './dependency.js';
                    export class Example {
                        async run(trigger, step) {
                            const before = await step.run('version', () => version);
                            if (trigger.input.wait) await step.waitForSignal('resume', { timeout: '1h' });
                            return { before, after: version };
                        }
                    }
                ".into(),
            ),
        ].into(),
        descriptor: None,
    }
    };
    let mut old_run = None;
    for (version, hash, wait) in [("original", 'b', true), ("replacement", 'c', false)] {
        let declaration = fixture
            .deployments
            .publish(
                &app,
                &DeployRegistration {
                    id: typed_id::generate("dep"),
                    hash: hash.to_string().repeat(64),
                    workflows: ["Example".into()].into(),
                    schedules: vec![],
                },
                &graph(version),
            )
            .await
            .unwrap();
        fixture
            .service
            .activate_deploy(&app, &declaration)
            .await
            .unwrap();
        let run = fixture
            .app
            .start(
                &RequestId::mint(),
                "Example",
                StartOptions {
                    input: json!({"wait":wait}),
                    ..StartOptions::default()
                },
            )
            .await
            .unwrap();
        let receipt = advance_until_suspended(&mut fixture.runner(Duration::from_secs(5))).await;
        assert_eq!(receipt.run_id, run.id);
        if wait {
            assert_eq!(receipt.state, RunState::Waiting);
            old_run = Some(run.id);
        } else {
            assert_eq!(receipt.state, RunState::Completed);
            assert_eq!(
                fixture.app.status(&run.id).await.unwrap().output,
                Some(json!({"before":version,"after":version}))
            );
        }
    }
    fixture.assert_disposed().await;
    let old_run = old_run.unwrap();
    let service = WorkflowService::open(
        std::rc::Rc::new(orm_fixture::store(fixture.directory.path()).await),
        Arc::new(HostPolicies::default()),
    )
    .await
    .unwrap()
    .with_deployments(fixture.deployments.binding(&[&app]));
    service
        .register_app(
            &app,
            PolicySnapshot::configuration(1.try_into().unwrap(), AppPolicy::default()).unwrap(),
        )
        .await
        .unwrap();
    fixture.app = service.for_app(app);
    fixture.service = service.clone();
    fixture.loader = Rc::new(Loader {
        service,
        probes: RefCell::new(Vec::new()),
        cpu_limit: Some(Duration::from_millis(100)),
        markers: Markers::default(),
    });
    fixture
        .app
        .signal(
            &RequestId::mint(),
            &old_run,
            SignalOptions {
                signal_type: "resume".into(),
                payload: json!(null),
            },
        )
        .await
        .unwrap();
    let receipt = advance_until_suspended(&mut fixture.runner(Duration::from_secs(5))).await;
    assert_eq!(receipt.run_id, old_run);
    assert_eq!(receipt.state, RunState::Completed);
    assert_eq!(
        fixture.app.status(&old_run).await.unwrap().output,
        Some(json!({"before":"original","after":"original"}))
    );
    fixture.assert_disposed().await;
}

#[compio::test]
async fn missing_executable_never_constructs_an_app_isolate() {
    use zeroship_workflow::service::runner::TaskTransport;
    let fixture =
        Fixture::new("throw new Error('must not evaluate'); export class Example {}").await;
    let run = fixture
        .app
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let tasks = fixture
        .service
        .tasks(WorkerIdentity::new("inspect-executable".into()).unwrap());
    let task = tasks.poll().await.unwrap().unwrap();
    assert!(fixture
        .deployments
        .source
        .delete_manifest(&fixture.app.app_id().uuid(), &task.invocation.deploy_hash)
        .await
        .unwrap());
    tasks.release(&task.id, &task.token).await.unwrap();
    assert!(matches!(
        fixture.runner(Duration::from_secs(5)).run_once().await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert!(fixture.loader.probes.borrow().is_empty());
    assert!(!fixture
        .app
        .status(&run.id)
        .await
        .unwrap()
        .state
        .is_terminal());
    assert!(tasks.poll().await.unwrap().is_none());
}

#[compio::test]
async fn persistent_worker_resumes_v8_without_a_request_isolate() {
    use zeroship_workflow::service::runner::{WorkerOptions, WorkflowWorker};
    let fixture = Fixture::new(
        r"
        export class Example {
            async run(_trigger, step) {
                await step.run('prepare', () => 'ready');
                const signal = await step.waitForSignal('resume', { timeout: '1h' });
                return signal.payload;
            }
        }
    ",
    )
    .await;
    let run = fixture
        .app
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let tasks = Rc::new(
        fixture
            .service
            .tasks(WorkerIdentity::new("background-worker".into()).unwrap()),
    );
    let executor = Rc::new(
        V8TaskExecutor::new(
            fixture.loader.clone(),
            tasks.clone(),
            TaskPayloadLimits::default(),
        )
        .unwrap(),
    );
    let mut worker = WorkflowWorker::new(
        tasks,
        executor,
        WorkerOptions {
            idle_poll_ms: 5,
            ..WorkerOptions::default()
        },
    )
    .unwrap();
    compio::time::timeout(
        Duration::from_secs(5),
        worker.run_until(async {
            while fixture.app.status(&run.id).await.unwrap().state != RunState::Waiting {
                compio::time::sleep(Duration::from_millis(5)).await;
            }
            fixture.assert_disposed().await;
            fixture
                .app
                .signal(
                    &RequestId::mint(),
                    &run.id,
                    SignalOptions {
                        signal_type: "resume".into(),
                        payload: json!({"accepted":true}),
                    },
                )
                .await
                .unwrap();
            while fixture.app.status(&run.id).await.unwrap().state != RunState::Completed {
                compio::time::sleep(Duration::from_millis(5)).await;
            }
        }),
    )
    .await
    .expect("background worker did not resume its durable signal wait");
    assert_eq!(
        fixture.app.status(&run.id).await.unwrap().output,
        Some(json!({"accepted":true}))
    );
    fixture.assert_disposed().await;
}

#[compio::test]
async fn concurrent_worker_shutdown_disposes_every_v8_isolate() {
    use zeroship_workflow::service::runner::{WorkerOptions, WorkflowWorker};
    let fixture = Fixture::new(
        r"
        import { env } from 'zeroship';
        export class Example {
            async run(_trigger, step) {
                return await step.run('hold', async () => {
                    env.probe.mark('entered');
                    await new Promise(() => {});
                });
            }
        }
    ",
    )
    .await;
    let mut runs = Vec::new();
    for _ in 0..2 {
        runs.push(
            fixture
                .app
                .start(&RequestId::mint(), "Example", StartOptions::default())
                .await
                .unwrap()
                .id,
        );
    }
    let tasks = Rc::new(
        fixture
            .service
            .tasks(WorkerIdentity::new("concurrent-worker".into()).unwrap()),
    );
    let executor = Rc::new(
        V8TaskExecutor::new(
            fixture.loader.clone(),
            tasks.clone(),
            TaskPayloadLimits::default(),
        )
        .unwrap(),
    );
    let mut worker = WorkflowWorker::new(
        tasks,
        executor,
        WorkerOptions {
            task_slots: runs.len(),
            idle_poll_ms: 5,
            ..WorkerOptions::default()
        },
    )
    .unwrap();
    compio::time::timeout(
        Duration::from_secs(5),
        worker.run_until(async {
            while fixture.loader.markers.0.lock().unwrap().len() < runs.len() {
                compio::time::sleep(Duration::from_millis(5)).await;
            }
            assert_eq!(
                fixture
                    .loader
                    .probes
                    .borrow()
                    .iter()
                    .filter(|probe| probe.strong_count() > 0)
                    .count(),
                runs.len()
            );
        }),
    )
    .await
    .expect("concurrent V8 executions did not stop");
    fixture.assert_disposed().await;
    for run in runs {
        assert!(!fixture.app.status(&run).await.unwrap().state.is_terminal());
    }
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

#[path = "support/orm.rs"]
mod orm_fixture;

#[path = "../../../tests/fixtures/workflow_deployments.rs"]
mod deployment_fixture;
