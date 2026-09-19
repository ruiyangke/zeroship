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

/// Synchronous work that outlasts any machine it runs on.
///
/// The loop spins on the clock rather than a fixed iteration count: these tests
/// assert that the watchdog interrupts this loop, and a count fast enough to
/// finish on a quiet machine proves nothing. `escaped` is therefore reachable
/// only if no watchdog ever fired. The control case passes a short bound the
/// deadline is meant to allow, so the same function proves both directions.
const BURN: &str = r"
    import { env } from 'zeroship';
    function burn(ms = 60000) {
        env.probe.mark('entered');
        const until = Date.now() + ms;
        let n = 0;
        while (Date.now() < until) n = (n + 1) % 2147483647;
        env.probe.mark('escaped');
        return n;
    }
";

struct Loader {
    app: AppWorkflows,
    probes: RefCell<Vec<InnerProbe>>,
    cpu_limit: Option<Duration>,
    markers: Markers,
}
impl WorkflowRuntimeLoader for Loader {
    fn load(
        &self,
        assignment: &TaskAssignment,
        executable: &LoadedWorker,
    ) -> Result<LoadedWorkflow, WorkflowServiceError> {
        assert!(!serde_json::to_string(&assignment.invocation)
            .unwrap()
            .contains(assignment.token.as_str()));
        let app = AppId::parse(&assignment.invocation.app_id).unwrap();
        if self.app.app_id() != &app {
            return Err(WorkflowServiceError::PermissionDenied);
        }
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
                    self.app.clone().into_backend(1).unwrap(),
                )),
            ])
            .app_id(app);
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
        let app = AppId::mint();
        let policies = Arc::new(HostPolicies::default());
        let binding = policies.bind(app.clone()).unwrap();
        binding
            .begin_refresh()
            .unwrap()
            // The host holds the manager's first ingress epoch; the journal closed none.
            .install(
                PolicySnapshot::configuration(1.try_into().unwrap(), policy)
                    .unwrap()
                    .with_ingress_epoch(Some(1.try_into().unwrap())),
            )
            .unwrap();
        let service = WorkflowService::open(
            std::rc::Rc::new(orm_fixture::store(dir.path()).await),
            policies,
        )
        .await
        .unwrap()
        .with_payload_storage(zeroship_storage::StorageStore::from_backend(Arc::new(
            zeroship_storage::LocalFs::new(dir.path().join("payloads")),
        )))
        .unwrap();
        let deployments = deployment_fixture::Deployments::new().await;
        let service = service.with_deployments(deployments.binding(&[&app]));
        let api = service.register_app(&binding).await.unwrap();
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
            app: api.clone(),
            service: service.clone(),
            loader: Rc::new(Loader {
                app: api,
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
    assert_eq!(
        error["message"],
        "workflow step output exceeds the configured payload limits"
    );
    fixture.assert_disposed().await;
}

/// Drive one body that catches whatever its own step raises, over an output of
/// the given length, and report where the run rested and what it returned.
async fn caught_step_output(length: usize) -> (RunState, Option<serde_json::Value>) {
    let fixture = Fixture::new(&format!(
        r"
        export class Example {{
            async run(_trigger,step) {{
                try {{
                    return await step.run('large',{{output:'inline'}},()=>'x'.repeat({length}));
                }} catch (e) {{
                    return e.name;
                }}
            }}
        }}
    "
    ))
    .await;
    let run = fixture
        .app
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let mut runner = fixture.runner_with_output_limits(Duration::from_secs(5), OUTPUT_LIMITS);
    let state = advance_until_suspended(&mut runner).await.state;
    let output = fixture.app.status(&run.id).await.unwrap().output;
    fixture.assert_disposed().await;
    (state, output)
}

#[compio::test]
async fn an_oversized_step_output_reaches_the_body_as_that_step_failing() {
    // The refusal is recorded against the step, so the body resumes at that row
    // and a catch around it can take another path the run completes on.
    assert_eq!(
        caught_step_output(64).await,
        (RunState::Completed, Some(json!("LimitExceededError")))
    );
}

#[compio::test]
async fn a_step_output_inside_the_inline_budget_never_reaches_the_catch() {
    // The control for the case above: the same body and the same catch site,
    // differing only in whether the output clears the inline budget.
    assert_eq!(
        caught_step_output(8).await,
        (RunState::Completed, Some(json!("xxxxxxxx")))
    );
}

/// How long a deadline test gives the host before its watchdog must fire.
///
/// The bound races ISOLATE STARTUP, not the burn loop: if it expires before the
/// loop is entered, the run still times out but `entered` is never marked and
/// the assertion below reports a watchdog failure that did not happen. Measured
/// startup here is milliseconds; this leaves orders of magnitude of headroom so
/// a loaded machine cannot turn a passing guarantee into a red test.
const DEADLINE: Duration = Duration::from_secs(3);
const DEADLINE_MS: i64 = 3_000;

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
        DEADLINE,
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
        DEADLINE,
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
        DEADLINE,
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
            lease_ms: DEADLINE_MS,
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
    // The control of the deadline cases above: the same loop, bounded well
    // inside the deadline, must run to completion and mark `escaped`.
    let source = format!("{BURN}\nexport class Example {{ run() {{ return burn(50); }} }}");
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
async fn native_runner_awaits_creator_startup_before_committing_the_frontier() {
    let fixture = Fixture::new(
        r#"
        const exponent = await new Promise(resolve => setTimeout(() => resolve(2), 0));
        export class Example {
            async run(trigger, step) {
                const squared = await step.run('square', () => trigger.input.number ** exponent);
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
    let policies = Arc::new(HostPolicies::default());
    let binding = policies.bind(app.clone()).unwrap();
    binding
        .begin_refresh()
        .unwrap()
        .install(
            PolicySnapshot::configuration(1.try_into().unwrap(), AppPolicy::default())
                .unwrap()
                .with_ingress_epoch(Some(1.try_into().unwrap())),
        )
        .unwrap();
    let service = WorkflowService::open(
        std::rc::Rc::new(orm_fixture::store(fixture.directory.path()).await),
        policies,
    )
    .await
    .unwrap()
    .with_deployments(fixture.deployments.binding(&[&app]));
    fixture.app = service.register_app(&binding).await.unwrap();
    fixture.service = service;
    fixture.loader = Rc::new(Loader {
        app: fixture.app.clone(),
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
        .delete_manifest(fixture.app.app_id(), &task.invocation.deploy_hash)
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

/// A native manager queue delivering to one trusted worker, as the CLI host
/// composes it over the fixture's local platform file.
struct Manager {
    coordinator: zeroship_workflow_manager::coordinator::Coordinator,
    worker: zeroship_core::workflow_coordination::WorkerId,
    scope: zeroship_core::workflow_coordination::AssignedScope,
}

impl Manager {
    async fn new(fixture: &Fixture) -> Rc<Self> {
        use zeroship_core::workflow_coordination::{
            AssignedScope, RegisterWorker, WorkerId, WorkerState,
        };
        use zeroship_workflow_manager::coordinator::{Coordinator, Options, Placed};
        let queue = fixture
            .deployments
            .platform
            .queue(zeroship_workflow_manager::Options::default())
            .await
            .unwrap();
        let coordinator = Coordinator::new(
            queue,
            Options::default(),
            std::rc::Rc::new(zeroship_workflow_manager::eligibility::LocalEligibility::new(
                zeroship_workflow_manager::eligibility::ZoneId::default_zone(),
            )),
        )
        .unwrap();
        let worker = WorkerId::mint();
        coordinator
            .register(
                &worker,
                &RegisterWorker {
                    capacity: 1.try_into().unwrap(),
                    state: WorkerState::Ready,
                },
            )
            .await
            .unwrap();
        let Placed::Assigned(assignment) = coordinator.place(fixture.app.app_id()).await.unwrap()
        else {
            panic!("the one registered worker is eligible and idle");
        };
        Rc::new(Self {
            coordinator,
            worker,
            scope: AssignedScope {
                app_id: assignment.app_id,
                assignment_revision: assignment.revision,
            },
        })
    }

    fn consumer(
        self: &Rc<Self>,
        fixture: &Fixture,
        slots: usize,
    ) -> zeroship_workflow::service::runner::consumer::JobConsumer<Self> {
        use zeroship_workflow::service::{
            collection::CollectionOptions,
            fanout::FanoutOptions,
            propagation::PropagationOptions,
            reconciliation::ReconciliationOptions,
            runner::{
                consumer::{ConsumerOptions, ConsumerScope, JobConsumer},
                delivery::DeliveryOptions,
            },
        };
        let tasks = Rc::new(
            fixture
                .app
                .tasks(WorkerIdentity::new(self.worker.as_str().to_owned()).unwrap()),
        );
        let executor = Rc::new(
            V8TaskExecutor::new(fixture.loader.clone(), tasks, TaskPayloadLimits::default())
                .unwrap(),
        );
        let consumer = JobConsumer::new(
            self.clone(),
            self.worker.clone(),
            ConsumerOptions {
                slots,
                max_scopes: 1,
                idle_poll: Duration::from_millis(5),
                error_backoff: Duration::from_millis(10),
                delivery: DeliveryOptions {
                    execution_timeout: Duration::from_secs(10),
                    operation_timeout: Duration::from_secs(2),
                    retry_delay: Duration::from_millis(5),
                    reconciliation: ReconciliationOptions::default(),
                    collection: CollectionOptions::default(),
                    fanout: FanoutOptions::default(),
                    propagation: PropagationOptions::default(),
                },
            },
        )
        .unwrap();
        consumer
            .bindings()
            .replace(vec![ConsumerScope::new(
                fixture.app.clone(),
                self.scope.clone(),
                executor,
            )
            .unwrap()])
            .unwrap();
        consumer
    }

    /// Stand in for the host's immediate publication after creator commits.
    async fn publish(&self, app: &AppWorkflows) {
        for job in app.pending_jobs(None, 64).await.unwrap() {
            app.publish_job(&job.id, self).await.unwrap();
        }
    }
}

fn manager_error(error: zeroship_workflow_manager::Error) -> WorkflowServiceError {
    WorkflowServiceError::Unavailable(error.to_string())
}

impl zeroship_workflow::service::runner::delivery::JobTransport for Manager {
    type Lease = zeroship_workflow_manager::DeliveryGrant;

    async fn claim(
        &self,
        scope: &zeroship_core::workflow_coordination::AssignedScope,
    ) -> Result<Option<Self::Lease>, WorkflowServiceError> {
        self.coordinator
            .claim_job(&self.worker, scope, AppPolicy::default().max_delivery_attempts, || async { Ok(self.worker.clone()) })
            .await
            .map_err(manager_error)
    }

    async fn submit(
        &self,
        scope: &zeroship_core::workflow_coordination::AssignedScope,
        job: &zeroship_core::workflow_jobs::JobSpec,
    ) -> Result<zeroship_core::workflow_jobs::JobSpec, WorkflowServiceError> {
        self.coordinator
            .submit_job(
                &self.worker,
                &zeroship_core::workflow_jobs::SubmitJob {
                    scope: scope.clone(),
                    job: job.clone(),
                },
                || async { Ok(self.worker.clone()) },
            )
            .await
            .map_err(manager_error)
    }

    async fn heartbeat(&self, lease: &Self::Lease) -> Result<Self::Lease, WorkflowServiceError> {
        self.coordinator
            .heartbeat_job(&self.worker, lease.delivery(), || async {
                Ok(self.worker.clone())
            })
            .await
            .map_err(manager_error)
    }

    async fn settle(
        &self,
        settlement: &zeroship_core::workflow_jobs::Settlement,
    ) -> Result<zeroship_core::workflow_jobs::SettlementReceipt, WorkflowServiceError> {
        self.coordinator
            .settle_job(&self.worker, settlement, || async {
                Ok(self.worker.clone())
            })
            .await
            .map_err(manager_error)
    }
}

impl zeroship_workflow::service::publication::JobPublisher for Manager {
    fn app_id(&self) -> &AppId {
        &self.scope.app_id
    }

    async fn submit(
        &self,
        job: &zeroship_core::workflow_jobs::JobSpec,
    ) -> Result<zeroship_core::workflow_jobs::JobSpec, WorkflowServiceError> {
        zeroship_workflow::service::runner::delivery::JobTransport::submit(self, &self.scope, job)
            .await
    }
}

#[compio::test]
async fn delivered_jobs_resume_v8_without_a_request_isolate() {
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
    let manager = Manager::new(&fixture).await;
    let mut consumer = manager.consumer(&fixture, 1);
    compio::time::timeout(
        Duration::from_secs(10),
        consumer.run_until(async {
            while fixture.app.status(&run.id).await.unwrap().state != RunState::Waiting {
                manager.publish(&fixture.app).await;
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
                manager.publish(&fixture.app).await;
                compio::time::sleep(Duration::from_millis(5)).await;
            }
        }),
    )
    .await
    .expect("delivered jobs did not resume the durable signal wait");
    assert_eq!(
        fixture.app.status(&run.id).await.unwrap().output,
        Some(json!({"accepted":true}))
    );
    fixture.assert_disposed().await;
}

#[compio::test]
async fn consumer_shutdown_disposes_every_concurrent_v8_isolate() {
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
    let manager = Manager::new(&fixture).await;
    manager.publish(&fixture.app).await;
    let mut consumer = manager.consumer(&fixture, runs.len());
    compio::time::timeout(
        Duration::from_secs(10),
        consumer.run_until(async {
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
    // Leave room for ordinary journal I/O when reusing the slot. The blocked
    // callback remains pending until the execution budget interrupts it.
    let mut runner = fixture.runner(Duration::from_secs(1));
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

/// A step that never settles, under a `StepConfig.timeout` the trigger chooses.
///
/// Two bounds can end this body: the step timeout the dispatcher arms, and the
/// host's per-job execution timeout, which `RunnerSlot` carries and which no
/// creator can see. The pair below differs only in which of the two is smaller.
const HANGING_STEP: &str = r"
    export class Example {
        async run(trigger, step) {
            return await step.run('work', {timeout: trigger.input.timeout}, () => new Promise(() => {}));
        }
    }
    export default { workflows: { Example } };
";

#[compio::test]
async fn a_step_timeout_under_the_execution_bound_commits_a_step_failure() {
    let fixture = Fixture::new(HANGING_STEP).await;
    let run = fixture
        .app
        .start(
            &RequestId::mint(),
            "Example",
            StartOptions {
                input: json!({"timeout":"50ms"}),
                ..StartOptions::default()
            },
        )
        .await
        .unwrap();
    let mut runner = fixture.runner(Duration::from_secs(10));
    assert_eq!(
        advance_until_suspended(&mut runner).await.state,
        RunState::Failed
    );
    let error = fixture.app.status(&run.id).await.unwrap().error.unwrap();
    assert_eq!(error["type"], "StepTimeoutError");
    assert_eq!(error["retryable"], true);
    assert_eq!(error["message"], "workflow step timed out after 50ms");
    fixture.assert_disposed().await;
}

#[compio::test]
async fn a_step_timeout_over_the_execution_bound_never_fires() {
    // The control: same body, same host, and only the step timeout moved past
    // the execution bound. The host's deadline reaches the job first, so the
    // step timeout the creator configured is never the reason recorded.
    let fixture = Fixture::new(HANGING_STEP).await;
    fixture
        .app
        .start(
            &RequestId::mint(),
            "Example",
            StartOptions {
                input: json!({"timeout":"10m"}),
                ..StartOptions::default()
            },
        )
        .await
        .unwrap();
    let mut runner = fixture.runner(Duration::from_secs(1));
    assert!(matches!(
        runner.run_once().await,
        Err(WorkflowServiceError::Timeout)
    ));
    fixture.assert_disposed().await;
}

#[path = "support/orm.rs"]
mod orm_fixture;

#[path = "../../../tests/fixtures/workflow_deployments.rs"]
mod deployment_fixture;
