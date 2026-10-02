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
    operations::{RunOperation, RunState, RunStatus, SignalOptions, StartOptions},
    service::{
        AppPolicy, AppWorkflows, DeployRegistration, HostPolicies, PayloadSlot,
        PolicySnapshot, RequestId, TaskAssignment, TaskToken, WorkerIdentity,
        WorkflowService,
    },
    WorkflowServiceError,
};
use zeroship_workflow_runner::{
    ObjectStepOutputs, PayloadObjects, PayloadRead, RunPayloads, TaskPayloadLimits,
    TaskPayloads, WorkerBinding, WorkerPayloads, WorkerTasks,
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
    journal: WorkflowService,
    objects: PayloadObjects,
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
                    self.app
                        .clone()
                        .into_backend(
                            &self.journal,
                            Arc::new(
                                ObjectStepOutputs::new(self.objects.clone(), 1).unwrap(),
                            ),
                            Arc::new(self.objects.clone()),
                        )
                        .unwrap(),
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
    objects: PayloadObjects,
    loader: Rc<Loader>,
    /// The hash of the deploy this fixture activated, so a case can reach the
    /// manifest an assignment names without polling for a task to read it off.
    deploy_hash: String,
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
        Self::with_workflows(source, cpu_limit, policy, &["Example"]).await
    }
    /// The same fixture with the deploy declaring `workflows`, so a parent can
    /// start the children it calls.
    async fn with_workflows(
        source: &str,
        cpu_limit: Option<Duration>,
        policy: AppPolicy,
        workflows: &[&str],
    ) -> Self {
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
        .unwrap();
        let objects = PayloadObjects::open(zeroship_storage::StorageStore::from_backend(
            Arc::new(zeroship_storage::LocalFs::new(dir.path().join("payloads"))),
        ))
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
                    workflows: workflows.iter().map(|name| (*name).to_owned()).collect(),
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
            deploy_hash: declaration.hash,
            app: api.clone(),
            service: service.clone(),
            objects: objects.clone(),
            loader: Rc::new(Loader {
                app: api,
                journal: service,
                objects,
                probes: RefCell::new(Vec::new()),
                cpu_limit,
                markers: Markers::default(),
            }),
        }
    }
    /// Start a run from `input`, staging the value first as the creator seam
    /// does: a generation row keeps no inline slot for a run.s input.
    async fn start(&self, input: serde_json::Value) -> zeroship_workflow::operations::StartedRun {
        let request = RequestId::mint();
        let options = StartOptions {
            input_ref: Some(
                zeroship_workflow::InputStager::stage_input(
                    &self.objects,
                    &self.app,
                    &request,
                    &input,
                )
                .await
                .unwrap(),
            ),
            ..StartOptions::default()
        };
        self.app.start(&request, "Example", options).await.unwrap()
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
        .payloads(&fixture.objects)
        .stage(
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

/// What a run returned, read back from the payload object it was staged into.
///
/// A run's result reaches the journal as a descriptor, so `status` names the
/// object and the bytes come from the store. Both halves are asserted here:
/// the descriptor status hands out is the one whose object holds these bytes.
async fn returned_value(fixture: &Fixture, run: &str) -> serde_json::Value {
    let described = fixture
        .app
        .status(run)
        .await
        .unwrap()
        .output
        .expect("a run that returned a value names the payload holding it");
    assert_eq!(described["kind"], "ref", "{described}");
    let bytes = fixture
        .app
        .payloads(&fixture.objects)
        .read(run, 0, PayloadSlot::Output)
        .await
        .unwrap()
        .into_bytes(4096)
        .await
        .unwrap();
    assert_eq!(described["size"], json!(bytes.len()), "{described}");
    serde_json::from_slice(&bytes).unwrap()
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
    let manager = Manager::new(&fixture).await;
    let mut consumer = manager.consumer(&fixture, 1);
    let done =
        advance_until_suspended(&fixture, &manager, &mut consumer, &run, Duration::from_secs(20))
            .await;
    assert_eq!(done.state, RunState::Completed);
    assert_eq!(
        returned_value(&fixture, &run).await,
        json!({"secret":"retained"})
    );
    fixture.assert_disposed().await;
}

#[compio::test]
async fn native_runner_hydrates_continuation_input_before_entering_v8() {
    let fixture =
        Fixture::new("export class Example { run(trigger) { return trigger.input; } }").await;
    let run = prepare_payload(&fixture, true).await;
    let manager = Manager::new(&fixture).await;
    // The staged input belongs to a continuation, so the work the manager
    // delivers is the SUCCESSOR run. Naming it is what the slot loop left
    // implicit: it polled for any runnable task and returned whichever receipt
    // came last, so the state it asserted was the successor's by accident.
    // Both links are settled by ONE consumer run. Two `run_until` calls would
    // make the second wait out the claim the first was still settling, which is
    // a flake rather than a property of the code.
    let mut consumer = manager.consumer(&fixture, 1);
    let successor = std::cell::RefCell::new(None);
    let outcome = std::cell::RefCell::new(None);
    let within = compio::time::timeout(
        Duration::from_secs(20),
        consumer.run_until(async {
            let mut watched = run.clone();
            loop {
                manager.publish(&fixture.app).await;
                let status = fixture.app.status(&watched).await.unwrap();
                match status.state {
                    RunState::Queued | RunState::Running => {}
                    RunState::ContinuedAsNew => {
                        let next = status
                            .continued_as_new_run_id
                            .clone()
                            .expect("a continued close names the run it handed its work to");
                        *successor.borrow_mut() = Some(next.clone());
                        watched = next;
                    }
                    _ => {
                        *outcome.borrow_mut() = Some(status);
                        return;
                    }
                }
                compio::time::sleep(Duration::from_millis(2)).await;
            }
        }),
    )
    .await;
    assert!(within.is_ok(), "the continuation chain never settled");
    let next = successor
        .into_inner()
        .expect("the staged input belongs to a continuation, so a successor must exist");
    assert_eq!(
        outcome.into_inner().expect("a settled run has a status").state,
        RunState::Completed
    );
    assert_eq!(
        returned_value(&fixture, &next).await,
        json!({"secret":"retained"})
    );
    fixture.assert_disposed().await;
}

#[compio::test]
async fn oversized_input_never_initializes_the_creator_module() {
    assert!(!payload_bounded_initialization(1).await);
}

#[compio::test]
async fn an_input_inside_the_payload_budget_initializes_the_creator_module() {
    // The control for the case above: the same staged input and the same drive,
    // differing only in whether it clears the budget. Without it, an empty
    // `probes` would prove the budget refused the input OR that nothing was
    // ever delivered to refuse.
    assert!(payload_bounded_initialization(1024 * 1024).await);
}

/// Drive a staged continuation input through delivery under `max_payload_bytes`
/// and answer whether the host built a creator isolate for it.
///
/// The creator module throws on evaluation, so a constructed isolate is proof
/// the input cleared the budget and nothing beyond that.
async fn payload_bounded_initialization(max_payload_bytes: usize) -> bool {
    let fixture =
        Fixture::new("throw new Error('must not evaluate'); export class Example {}").await;
    prepare_payload(&fixture, true).await;
    let manager = Manager::new(&fixture).await;
    let mut consumer = manager.consumer_with_limits(
        &fixture,
        1,
        TaskPayloadLimits {
            max_inline_bytes: max_payload_bytes,
            max_payload_bytes,
            ..TaskPayloadLimits::default()
        },
    );
    drive_for(&fixture, &manager, &mut consumer, DRIVE_WINDOW).await;
    let built = !fixture.loader.probes.borrow().is_empty();
    built
}

struct UnavailablePayloads(Rc<WorkerTasks>);
#[async_trait(?Send)]
impl zeroship_workflow_runner::TaskPayloads for UnavailablePayloads {
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
    ) -> Result<zeroship_workflow_runner::UploadReceipt, WorkflowServiceError> {
        Err(WorkflowServiceError::Unavailable(
            "fixture payload outage".into(),
        ))
    }
    async fn read(
        &self,
        _task: &str,
        _token: &zeroship_workflow::service::TaskToken,
        _reference: &zeroship_workflow::engine::WorkflowOutputRef,
    ) -> Result<PayloadRead, WorkflowServiceError> {
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
    // The outage consumes a delivery it cannot finish, so the recovery below
    // waits on the manager redelivering it.
    let manager = Manager::with_delivery_lease(&fixture, Duration::from_secs(1)).await;
    let tasks = Rc::new(fixture.app.tasks(
        WorkerIdentity::new(manager.worker.as_str().to_owned()).unwrap(),
        fixture.objects.clone(),
    ));
    let executor = Rc::new(
        V8TaskExecutor::new(
            fixture.loader.clone(),
            Rc::new(UnavailablePayloads(tasks.clone())),
            TaskPayloadLimits::default(),
        )
        .unwrap(),
    );
    let mut outage = manager.consumer_with_executor(&fixture, 1, executor);
    drive_for(&fixture, &manager, &mut outage, Duration::from_secs(2)).await;
    assert!(
        fixture.loader.markers.0.lock().unwrap().is_empty(),
        "app code ran despite a replay payload it could not read"
    );
    assert!(!fixture.app.status(&run).await.unwrap().state.is_terminal());
    fixture.assert_disposed().await;
    drop(outage);
    let mut consumer = manager.consumer(&fixture, 1);
    let done =
        advance_until_suspended(&fixture, &manager, &mut consumer, &run, Duration::from_secs(20))
            .await;
    assert_eq!(done.state, RunState::Completed);
    assert_eq!(
        returned_value(&fixture, &run).await,
        json!({"secret":"retained"})
    );
    fixture.assert_disposed().await;
}

const OUTPUT_LIMITS: TaskPayloadLimits = TaskPayloadLimits {
    max_inline_bytes: 32,
    max_payload_bytes: 256,
    max_result_bytes: 4096,
    max_replay_bytes: 256,
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
    ) -> Result<zeroship_workflow_runner::UploadReceipt, WorkflowServiceError> {
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
    let manager = Manager::new(&fixture).await;
    // The payload seam must answer for the worker that holds the claim, which on
    // this path is the one the manager placed rather than a name of the test's
    // own choosing.
    let tasks = Rc::new(fixture.app.tasks(
        WorkerIdentity::new(manager.worker.as_str().to_owned()).unwrap(),
        fixture.objects.clone(),
    ));
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
    let mut consumer = manager.consumer_with_executor(&fixture, 1, executor);
    let done =
        advance_until_suspended(&fixture, &manager, &mut consumer, &run.id, Duration::from_secs(20))
            .await;
    assert_eq!(done.state, RunState::Completed);
    assert_eq!(returned_value(&fixture, &run.id).await, json!(64));
    assert_eq!(*fixture.loader.markers.0.lock().unwrap(), ["callback"]);
    {
        let requests = upload.requests.borrow();
        // The step's upload carries one identity across the lost receipt, so
        // the retry is the same request rather than a second payload. The run's
        // own output is staged under an identity of its own, which is what
        // makes the repeated pair a retry and not just two uploads.
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0], requests[1]);
        assert_ne!(requests[1], requests[2]);
    }
    let bytes = fixture
        .app
        .payloads(&fixture.objects)
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
    let manager = Manager::new(&fixture).await;
    let tasks = Rc::new(fixture.app.tasks(
        WorkerIdentity::new(manager.worker.as_str().to_owned()).unwrap(),
        fixture.objects.clone(),
    ));
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
    let mut consumer = manager.consumer_with_executor(&fixture, 1, executor);
    // The outage leaves the frontier retryable, so there is no settled state to
    // wait for; the window is what bounds the case.
    drive_for(&fixture, &manager, &mut consumer, Duration::from_secs(3)).await;
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
    let manager = Manager::new(&fixture).await;
    let mut consumer = manager.consumer_with_limits(&fixture, 1, OUTPUT_LIMITS);
    assert_eq!(
        advance_until_suspended(&fixture, &manager, &mut consumer, &run.id, Duration::from_secs(20))
            .await
            .state,
        RunState::Completed
    );
    let bytes = fixture
        .app
        .payloads(&fixture.objects)
        .read(&run.id, 0, PayloadSlot::Output)
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
    let manager = Manager::new(&fixture).await;
    let mut consumer = manager.consumer_with_limits(&fixture, 1, OUTPUT_LIMITS);
    assert_eq!(
        advance_until_suspended(&fixture, &manager, &mut consumer, &run.id, Duration::from_secs(20))
            .await
            .state,
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
async fn caught_step_output(length: usize) -> (RunState, serde_json::Value) {
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
    let manager = Manager::new(&fixture).await;
    let mut consumer = manager.consumer_with_limits(&fixture, 1, OUTPUT_LIMITS);
    let state =
        advance_until_suspended(&fixture, &manager, &mut consumer, &run.id, Duration::from_secs(20))
            .await
            .state;
    let output = returned_value(&fixture, &run.id).await;
    fixture.assert_disposed().await;
    (state, output)
}

#[compio::test]
async fn an_oversized_step_output_reaches_the_body_as_that_step_failing() {
    // The refusal is recorded against the step, so the body resumes at that row
    // and a catch around it can take another path the run completes on.
    assert_eq!(
        caught_step_output(64).await,
        (RunState::Completed, json!("LimitExceededError"))
    );
}

#[compio::test]
async fn a_step_output_inside_the_inline_budget_never_reaches_the_catch() {
    // The control for the case above: the same body and the same catch site,
    // differing only in whether the output clears the inline budget.
    assert_eq!(
        caught_step_output(8).await,
        (RunState::Completed, json!("xxxxxxxx"))
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

/// Drive a burning workflow past `bound` and assert the host cut it short.
///
/// The watchdog's proof is the marker list, not a returned error: delivery
/// leaves a timed-out claim retryable rather than answering the caller, so what
/// a caller could observe is that the loop was ENTERED and never reached its
/// final native effect.
///
/// The drive window is fixed at just past [`DEADLINE`], not derived from
/// `bound`. That is what lets a case name a bound the interrupt must BEAT: a
/// window inside `bound` leaves the confirmed lease as the only thing that
/// could have cut the run. It also stays well inside the manager's lease, so
/// the single attempt asserted here is the only one.
async fn assert_deadline_interrupts(source: &str, bound: Duration, policy: AppPolicy) {
    // Disable the CPU timer: only the host's monotonic deadline may interrupt.
    let fixture = Fixture::with_limits(&format!("{BURN}\n{source}"), None, policy).await;
    let run = fixture
        .app
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let manager = Manager::new(&fixture).await;
    let mut consumer = manager.consumer_bounding_execution(&fixture, 1, bound);
    let window = DEADLINE + Duration::from_secs(2);
    assert!(
        drive_until_disposed(&fixture, &manager, &mut consumer, window).await,
        "the host must cut the loop and dispose its isolate within {window:?}"
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

/// Drive the app's frontier the way the worker does, and answer with what the
/// journal says.
///
/// The manager publishes the jobs the journal has pending, the consumer claims
/// and executes them, and the run's own status is the assertion's subject. This
/// is the path `zeroship-worker` takes, so it carries the delivery grant, the
/// fencing and the lease renewal a real job is executed under.
///
/// A drive that instead polled the journal itself would assert the executor's
/// behaviour under a dispatch nothing performs.
async fn advance_until_suspended(
    fixture: &Fixture,
    manager: &Rc<Manager>,
    consumer: &mut zeroship_workflow_runner::consumer::JobConsumer<Manager>,
    run: &str,
    bound: Duration,
) -> RunStatus {
    advance_until(fixture, manager, consumer, run, bound, |status| {
        !matches!(status.state, RunState::Queued | RunState::Running)
    })
    .await
}

/// The same drive, with the caller naming the state it is waiting FOR.
///
/// A run that is already suspended when the drive starts - one resumed by a
/// signal, say - is not what `advance_until_suspended` answers, because its
/// entry state already satisfies that predicate.
async fn advance_until(
    fixture: &Fixture,
    manager: &Rc<Manager>,
    consumer: &mut zeroship_workflow_runner::consumer::JobConsumer<Manager>,
    run: &str,
    bound: Duration,
    done: impl Fn(&RunStatus) -> bool,
) -> RunStatus {
    drive_until(fixture, manager, consumer, run, bound, done)
        .await
        .unwrap_or_else(|| panic!("the run stayed runnable for {bound:?}; delivery never settled it"))
}

/// The same drive, answering `None` when the state never arrived.
///
/// For a case whose claim is that a run does NOT settle, reading the status once
/// after the host cut the job proves nothing: the host commits a failure AFTER
/// disposing the isolate, so a status read at that moment is early either way.
/// Waiting the bound out and finding nothing is the assertion.
async fn drive_until(
    fixture: &Fixture,
    manager: &Rc<Manager>,
    consumer: &mut zeroship_workflow_runner::consumer::JobConsumer<Manager>,
    run: &str,
    bound: Duration,
    done: impl Fn(&RunStatus) -> bool,
) -> Option<RunStatus> {
    let settled = std::cell::RefCell::new(None);
    let _ = compio::time::timeout(
        bound,
        consumer.run_until(async {
            loop {
                manager.publish(&fixture.app).await;
                let status = fixture.app.status(run).await.unwrap();
                if done(&status) {
                    *settled.borrow_mut() = Some(status);
                    return;
                }
                compio::time::sleep(Duration::from_millis(2)).await;
            }
        }),
    )
    .await;
    settled.into_inner()
}

/// Drive delivery until every isolate the loader handed out is released, and
/// answer whether that happened inside `bound`.
///
/// For a run the host must CUT, disposal is the observable event and the time it
/// takes is the assertion. Asserting only that the isolate ended up disposed
/// proves nothing about which bound cut the run: the app's confirmed lease and
/// the manager's delivery lease would both release it eventually, and a case
/// naming a shorter bound would pass on their strength.
async fn drive_until_disposed(
    fixture: &Fixture,
    manager: &Rc<Manager>,
    consumer: &mut zeroship_workflow_runner::consumer::JobConsumer<Manager>,
    bound: Duration,
) -> bool {
    compio::time::timeout(
        bound,
        consumer.run_until(async {
            loop {
                manager.publish(&fixture.app).await;
                let released = {
                    let probes = fixture.loader.probes.borrow();
                    !probes.is_empty() && probes.iter().all(|probe| probe.strong_count() == 0)
                };
                if released {
                    return;
                }
                compio::time::sleep(Duration::from_millis(5)).await;
            }
        }),
    )
    .await
    .is_ok()
}

/// How long a case drives delivery when what it asserts is what the host did
/// while it could not finish.
///
/// Long enough for a claim to be delivered and attempted, and well inside the
/// manager's lease, so a case counting what happened once sees one attempt
/// rather than a redelivery.
const DRIVE_WINDOW: Duration = Duration::from_secs(1);

/// Drive delivery for a fixed window, for the cases whose run never settles.
///
/// A payload outage leaves the frontier retryable on purpose, so there is no
/// state to wait for: what the case asserts is what the host did while it could
/// not finish - the callback it ran once, the uploads it retried, and the isolate
/// it disposed.
async fn drive_for(
    fixture: &Fixture,
    manager: &Rc<Manager>,
    consumer: &mut zeroship_workflow_runner::consumer::JobConsumer<Manager>,
    window: Duration,
) {
    let _ = compio::time::timeout(
        window,
        consumer.run_until(async {
            loop {
                manager.publish(&fixture.app).await;
                compio::time::sleep(Duration::from_millis(5)).await;
            }
        }),
    )
    .await;
}

#[compio::test]
async fn control_bounded_loop_completes_when_the_deadline_allows_it() {
    // The control of the deadline cases above: the same loop, bounded well
    // inside the deadline, must run to completion and mark `escaped`.
    let source = format!("{BURN}\nexport class Example {{ run() {{ return burn(50); }} }}");
    let fixture = Fixture::with_limits(&source, None, AppPolicy::default()).await;
    let run = fixture
        .app
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let manager = Manager::new(&fixture).await;
    let mut consumer = manager.consumer(&fixture, 1);
    let done =
        advance_until_suspended(&fixture, &manager, &mut consumer, &run.id, Duration::from_secs(30))
            .await;
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
    let run = fixture.start(json!({"number":7})).await;
    let manager = Manager::new(&fixture).await;
    let mut consumer = manager.consumer(&fixture, 1);
    let receipt =
        advance_until_suspended(&fixture, &manager, &mut consumer, &run.id, Duration::from_secs(20))
            .await;
    assert_eq!(receipt.state, RunState::Completed);
    assert_eq!(
        returned_value(&fixture, &run.id).await,
        json!({"squared":49})
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
    // One manager for the whole case, across the host restart. Its worker
    // registration and app assignment live in the platform store, which the
    // rebuilt service reopens, so registering again would place a second worker
    // against an app already assigned to the first.
    let manager = Manager::new(&fixture).await;
    let mut consumer = manager.consumer(&fixture, 1);
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
        let run = fixture.start(json!({"wait":wait})).await;
        let status = advance_until_suspended(
            &fixture,
            &manager,
            &mut consumer,
            &run.id,
            Duration::from_secs(20),
        )
        .await;
        if wait {
            assert_eq!(status.state, RunState::Waiting);
            old_run = Some(run.id);
        } else {
            assert_eq!(status.state, RunState::Completed);
            assert_eq!(
                returned_value(&fixture, &run.id).await,
                json!({"before":version,"after":version})
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
        journal: fixture.service.clone(),
        objects: fixture.objects.clone(),
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
    // A fresh consumer on the SAME manager: the one above holds the loader the
    // restart replaced, and loading the retained dependency is the subject.
    //
    // The signal above already moved this run out of `Running`, so the state to
    // wait for has to be named: `advance_until_suspended` is satisfied by the
    // run's ENTRY state and answers `Waiting` before delivery resumes it.
    let mut consumer = manager.consumer(&fixture, 1);
    let status = advance_until(
        &fixture,
        &manager,
        &mut consumer,
        &old_run,
        Duration::from_secs(20),
        |status| status.state == RunState::Completed,
    )
    .await;
    assert_eq!(status.state, RunState::Completed);
    assert_eq!(
        returned_value(&fixture, &old_run).await,
        json!({"before":"original","after":"original"})
    );
    fixture.assert_disposed().await;
}

#[compio::test]
async fn missing_executable_never_constructs_an_app_isolate() {
    let fixture =
        Fixture::new("throw new Error('must not evaluate'); export class Example {}").await;
    let run = fixture
        .app
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    assert!(fixture
        .deployments
        .source
        .delete_manifest(fixture.app.app_id(), &fixture.deploy_hash)
        .await
        .unwrap());
    let manager = Manager::new(&fixture).await;
    let mut consumer = manager.consumer(&fixture, 1);
    drive_for(&fixture, &manager, &mut consumer, DRIVE_WINDOW).await;
    assert!(fixture.loader.probes.borrow().is_empty());
    // An executable the host cannot load is not the creator's failure, so the
    // frontier stays retryable rather than settling the run.
    assert!(!fixture
        .app
        .status(&run.id)
        .await
        .unwrap()
        .state
        .is_terminal());
}

#[compio::test]
async fn an_available_executable_constructs_an_app_isolate() {
    // The control for the case above: the same drive over the same run and the
    // same window, differing only in whether the manifest the assignment names
    // is still there. Without it, an empty `probes` would prove the host built
    // no isolate OR that nothing was ever delivered to it.
    let fixture =
        Fixture::new("throw new Error('must not evaluate'); export class Example {}").await;
    fixture
        .app
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let manager = Manager::new(&fixture).await;
    let mut consumer = manager.consumer(&fixture, 1);
    drive_for(&fixture, &manager, &mut consumer, DRIVE_WINDOW).await;
    assert!(!fixture.loader.probes.borrow().is_empty());
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
        Self::with_delivery_lease(fixture, zeroship_workflow_manager::Options::default().lease).await
    }

    /// The same manager with the delivery lease named.
    ///
    /// A delivery the host consumed but could not finish comes back when THIS
    /// lease lapses - not when the creator task lease does - so a case that waits
    /// for a redelivery has to outlast it, and the default puts it half a minute
    /// out.
    async fn with_delivery_lease(fixture: &Fixture, lease: Duration) -> Rc<Self> {
        use zeroship_core::workflow_coordination::{
            AssignedScope, RegisterWorker, WorkerId, WorkerState,
        };
        use zeroship_workflow_manager::coordinator::{Coordinator, Options, Placed};
        let queue = fixture
            .deployments
            .platform
            .queue(zeroship_workflow_manager::Options {
                lease,
                ..zeroship_workflow_manager::Options::default()
            })
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
    ) -> zeroship_workflow_runner::consumer::JobConsumer<Self> {
        self.consumer_with_limits(fixture, slots, TaskPayloadLimits::default())
    }
    /// The same consumer under explicit payload bounds, for the cases that
    /// assert what an oversized input or output does.
    fn consumer_with_limits(
        self: &Rc<Self>,
        fixture: &Fixture,
        slots: usize,
        limits: TaskPayloadLimits,
    ) -> zeroship_workflow_runner::consumer::JobConsumer<Self> {
        let tasks = Rc::new(
            fixture
                .app
                .tasks(
                    WorkerIdentity::new(self.worker.as_str().to_owned()).unwrap(),
                    fixture.objects.clone(),
                ),
        );
        let executor = Rc::new(
            V8TaskExecutor::new(fixture.loader.clone(), tasks, limits).unwrap(),
        );
        self.consumer_with_executor(fixture, slots, executor)
    }
    /// The same consumer under an explicit execution bound, for the cases that
    /// assert what the host does to a job that outlives it.
    ///
    /// Delivery bounds an execution by the EARLIER of this and the confirmed
    /// lease's deadline, so a case can name which of the two it is about.
    fn consumer_bounding_execution(
        self: &Rc<Self>,
        fixture: &Fixture,
        slots: usize,
        execution_timeout: Duration,
    ) -> zeroship_workflow_runner::consumer::JobConsumer<Self> {
        let tasks = Rc::new(
            fixture
                .app
                .tasks(
                    WorkerIdentity::new(self.worker.as_str().to_owned()).unwrap(),
                    fixture.objects.clone(),
                ),
        );
        let executor = Rc::new(
            V8TaskExecutor::new(fixture.loader.clone(), tasks, TaskPayloadLimits::default())
                .unwrap(),
        );
        self.consumer_over(fixture, slots, executor, execution_timeout)
    }
    /// The same consumer over an executor the caller built, for the cases that
    /// wrap the payload seam in a probe.
    fn consumer_with_executor(
        self: &Rc<Self>,
        fixture: &Fixture,
        slots: usize,
        executor: Rc<V8TaskExecutor>,
    ) -> zeroship_workflow_runner::consumer::JobConsumer<Self> {
        self.consumer_over(fixture, slots, executor, Duration::from_secs(10))
    }
    fn consumer_over(
        self: &Rc<Self>,
        fixture: &Fixture,
        slots: usize,
        executor: Rc<V8TaskExecutor>,
        execution_timeout: Duration,
    ) -> zeroship_workflow_runner::consumer::JobConsumer<Self> {
        use zeroship_workflow_runner::{
            consumer::{ConsumerOptions, ConsumerScope, JobConsumer},
            delivery::DeliveryOptions,
        };
        let consumer = JobConsumer::new(
            self.clone(),
            self.worker.clone(),
            ConsumerOptions {
                slots,
                max_scopes: 1,
                idle_poll: Duration::from_millis(5),
                error_backoff: Duration::from_millis(10),
                delivery: DeliveryOptions {
                    execution_timeout,
                    operation_timeout: Duration::from_secs(2),
                    retry_delay: Duration::from_millis(5),
                },
            },
        )
        .unwrap();
        consumer
            .bindings()
            .replace(vec![ConsumerScope::new(
                fixture.app.clone(),
                fixture.app.binding().clone(),
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

    /// Run every maintenance row the app's queue holds, as the workflow
    /// service's lane does, so a child's completion reaches its parent.
    async fn maintain(&self, fixture: &Fixture) {
        use zeroship_workflow::service::maintenance::{MaintenanceOptions, MaintenanceOutcome};
        let lane = zeroship_workflow_manager::maintenance::MaintenanceAuthority::new(
            fixture.app.app_id().clone(),
            zeroship_core::workflow_coordination::WorkerId::mint(),
        );
        let queue = self.coordinator.queue();
        while let Some(grant) = lane
            .claim(queue, Ok(AppPolicy::default().max_delivery_attempts))
            .await
            .unwrap()
        {
            let outcome = fixture
                .app
                .maintenance_job(
                    &grant,
                    self,
                    &fixture.objects,
                    &fixture.objects,
                    MaintenanceOptions::default(),
                )
                .await
                .unwrap();
            let MaintenanceOutcome::Settled(receipt) = outcome else {
                return;
            };
            lane.settle(queue, &receipt.settlement(&grant).unwrap())
                .await
                .unwrap();
        }
    }
}

fn manager_error(error: zeroship_workflow_manager::Error) -> WorkflowServiceError {
    WorkflowServiceError::Unavailable(error.to_string())
}

impl zeroship_workflow_runner::delivery::JobTransport for Manager {
    /// Asked of the journal this host holds, the way the crossed transport asks
    /// the service that holds it.
    async fn release(
        &self,
        journal: &Self::Journal,
        lease: &Self::Lease,
        task: &zeroship_workflow::service::delivery::DeliveredTask,
    ) -> Result<(), WorkflowServiceError> {
        journal.release_job(task, lease).await
    }
    async fn receipt(
        &self,
        journal: &Self::Journal,
        job: &zeroship_core::workflow_jobs::JobSpec,
    ) -> Result<Option<zeroship_workflow::service::delivery::JobReceipt>, WorkflowServiceError> {
        journal.job_receipt(job).await
    }
    type Lease = zeroship_workflow_manager::DeliveryGrant;
    /// This host holds the journal, so an attempt is scoped here rather than
    /// server-side.
    type Journal = zeroship_workflow::service::AppWorkflows;
    fn scope(
        &self,
        journal: &Self::Journal,
        authority: &zeroship_workflow::service::PolicyAuthority,
    ) -> Result<Self::Journal, WorkflowServiceError> {
        zeroship_workflow_runner::delivery::scope_journal(journal, authority)
    }

    async fn claim(
        &self,
        journal: &zeroship_workflow::service::AppWorkflows,
        scope: &zeroship_core::workflow_coordination::AssignedScope,
    ) -> Result<Option<zeroship_workflow_runner::delivery::Claimed<Self::Lease>>, WorkflowServiceError> {
        let granted = self
            .coordinator
            .claim_job(&self.worker, scope, Ok(AppPolicy::default().max_delivery_attempts), || async { Ok(self.worker.clone()) })
            .await
            .map_err(manager_error)?;
        let Some(lease) = granted else {
            return Ok(None);
        };
        let accepted = if lease.delivery().job.operation.accepts_execution() {
            Some(journal.accept_job(&lease).await?)
        } else {
            None
        };
        Ok(Some(zeroship_workflow_runner::delivery::Claimed { lease, accepted }))
    }

    async fn heartbeat(
        &self,
        journal: &zeroship_workflow::service::AppWorkflows,
        lease: &Self::Lease,
        task: &zeroship_workflow::service::delivery::DeliveredTask,
    ) -> Result<zeroship_workflow_runner::delivery::Renewed<Self::Lease>, WorkflowServiceError> {
        let lease = self
            .coordinator
            .heartbeat_job(&self.worker, lease.delivery(), || async {
                Ok(self.worker.clone())
            })
            .await
            .map_err(manager_error)?;
        let renewal = journal.heartbeat_job(task, &lease).await?;
        Ok(zeroship_workflow_runner::delivery::Renewed { lease, renewal })
    }

    async fn settle(
        &self,
        journal: &zeroship_workflow::service::AppWorkflows,
        lease: &Self::Lease,
    ) -> Result<zeroship_core::workflow_jobs::SettlementReceipt, WorkflowServiceError> {
        let settlement =
            zeroship_workflow_runner::delivery::committed_settlement(journal, lease).await?;
        self.coordinator
            .settle_job(&self.worker, &settlement, || async {
                Ok(self.worker.clone())
            })
            .await
            .map_err(manager_error)
    }

    async fn complete(
        &self,
        journal: &zeroship_workflow::service::AppWorkflows,
        lease: &Self::Lease,
        task: &zeroship_workflow::service::delivery::DeliveredTask,
        execution: zeroship_workflow::WorkflowExecution,
        confirmed: Vec<zeroship_workflow::service::delivery::PayloadConfirmation>,
    ) -> Result<zeroship_workflow_runner::delivery::Completed, WorkflowServiceError> {
        let receipt = journal
            .complete_reported_job(task, lease, execution, &confirmed)
            .await?;
        Ok(zeroship_workflow_runner::delivery::Completed {
            settlement: zeroship_workflow_runner::delivery::JobTransport::settle(
                self, journal, lease,
            )
            .await?,
            receipt,
        })
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
        self.coordinator
            .submit_job(
                &self.worker,
                &zeroship_core::workflow_jobs::SubmitJob {
                    scope: self.scope.clone(),
                    job: job.clone(),
                },
                || async { Ok(self.worker.clone()) },
            )
            .await
            .map_err(manager_error)
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
        returned_value(&fixture, &run.id).await,
        json!({"accepted":true})
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
    let manager = Manager::new(&fixture).await;
    let mut consumer = manager.consumer(&fixture, 1);
    let waiting = advance_until(
        &fixture,
        &manager,
        &mut consumer,
        &run.id,
        Duration::from_secs(20),
        |status| status.state == RunState::Waiting,
    )
    .await;
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
    let done = advance_until(
        &fixture,
        &manager,
        &mut consumer,
        &run.id,
        Duration::from_secs(20),
        |status| status.state == RunState::Completed,
    )
    .await;
    assert_eq!(done.state, RunState::Completed);
    assert_eq!(
        returned_value(&fixture, &run.id).await,
        json!({"prepared":{"ready":true},"payload":{"accepted":true}})
    );
    assert!(fixture.loader.probes.borrow().len() > before_signal);
    fixture.assert_disposed().await;
}

#[compio::test]
async fn an_execution_timeout_disposes_v8_before_reusing_the_slot() {
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
        .start(json!({"hang":true}))
        .await;
    // Leave room for ordinary journal I/O when reusing the slot. The blocked
    // callback remains pending until the execution budget interrupts it.
    let manager = Manager::new(&fixture).await;
    let mut consumer = manager.consumer_bounding_execution(&fixture, 1, Duration::from_secs(1));
    assert!(
        drive_until_disposed(&fixture, &manager, &mut consumer, Duration::from_secs(4)).await,
        "the execution bound must cut the pending callback and dispose its isolate"
    );
    fixture
        .app
        .transition(&RequestId::mint(), &blocked.id, RunOperation::Cancel)
        .await
        .unwrap();
    let next = fixture
        .start(json!({"hang":false}))
        .await;
    // The SAME consumer, so what is asserted is that the slot the cut job held
    // takes another one.
    let done = advance_until(
        &fixture,
        &manager,
        &mut consumer,
        &next.id,
        Duration::from_secs(10),
        |status| status.state == RunState::Completed,
    )
    .await;
    assert_eq!(done.state, RunState::Completed);
    assert_eq!(returned_value(&fixture, &next.id).await, json!("finished"));
    fixture.assert_disposed().await;
}

/// A step that never settles, under a `StepConfig.timeout` the trigger chooses.
///
/// Two bounds can end this body: the step timeout the dispatcher arms, and the
/// host's per-job execution bound, which delivery carries and which no
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
    let run = fixture.start(json!({"timeout":"50ms"})).await;
    let manager = Manager::new(&fixture).await;
    let mut consumer = manager.consumer(&fixture, 1);
    assert_eq!(
        advance_until_suspended(&fixture, &manager, &mut consumer, &run.id, Duration::from_secs(20))
            .await
            .state,
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
    let run = fixture.start(json!({"timeout":"10m"})).await;
    let manager = Manager::new(&fixture).await;
    let mut consumer = manager.consumer_bounding_execution(&fixture, 1, Duration::from_secs(1));
    // The bound cuts the job and delivery retries it, so the drive outlasts
    // several attempts. None of them may record the creator's step timeout.
    let settled = drive_until(
        &fixture,
        &manager,
        &mut consumer,
        &run.id,
        Duration::from_secs(6),
        |status| !matches!(status.state, RunState::Queued | RunState::Running),
    )
    .await;
    assert_eq!(
        settled, None,
        "the host's bound cut the job, so nothing may settle the run"
    );
}

/// A replay transport whose object reads answer after a host round-trip, each
/// after a delay of its own, so the order they complete in is the case's choice
/// rather than the scheduler's. Every other call reaches the real task host.
struct DelayedReads {
    tasks: WorkerTasks,
    objects: Vec<(Vec<u8>, Duration)>,
    reads: RefCell<Vec<String>>,
}
#[async_trait(?Send)]
impl TaskPayloads for DelayedReads {
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
    ) -> Result<zeroship_workflow_runner::UploadReceipt, WorkflowServiceError> {
        self.tasks
            .stage(task, token, request, reference, body)
            .await
    }
    async fn read(
        &self,
        task: &str,
        token: &TaskToken,
        reference: &zeroship_workflow::engine::WorkflowOutputRef,
    ) -> Result<PayloadRead, WorkflowServiceError> {
        let Some((bytes, delay)) = self
            .objects
            .iter()
            .find(|(bytes, _)| object_ref(bytes) == *reference)
        else {
            return self.tasks.read(task, token, reference).await;
        };
        self.reads
            .borrow_mut()
            .push(String::from_utf8_lossy(bytes).into_owned());
        compio::time::sleep(*delay).await;
        PayloadRead::checked(
            reference.clone(),
            Box::new(zeroship_storage::backend::OnceChunk::new(
                bytes.clone().into(),
            )),
        )
    }
}

fn object_ref(bytes: &[u8]) -> zeroship_workflow::engine::WorkflowOutputRef {
    use sha2::{Digest, Sha256};
    zeroship_workflow::engine::WorkflowOutputRef {
        hash: format!("{:x}", Sha256::digest(bytes)),
        size: i64::try_from(bytes.len()).unwrap(),
        content_type: Some("application/json".into()),
    }
}

/// A parent that calls two children at once and runs one step after each.
/// Which step it issues first is decided by which child's value it has first.
const SIBLINGS: &str = r"
    export class A { async run() { return { from: 'A' }; } }
    export class B { async run() { return { from: 'B' }; } }
    export class Example {
        async run(_trigger, step) {
            await Promise.all([
                step.call(A, {}).then((a) => step.run('x', () => a.from)),
                step.call(B, {}).then((b) => step.run('y', () => b.from)),
            ]);
            return 'done';
        }
    }
";

const CHILD_A: &[u8] = br#"{"from":"A"}"#;
const CHILD_B: &[u8] = br#"{"from":"B"}"#;
/// Longer than any scheduling jitter between the two reads, so a read that
/// waits this long completes after one that does not.
const SLOW: Duration = Duration::from_millis(200);

/// One delivery of `Example`, replaying `journal` under the host budgets
/// `limits`, with the objects it names answered by [`DelayedReads`].
struct Replayed {
    /// The frontier the host would submit, or why it refused to run the task.
    frontier: Result<Vec<serde_json::Value>, WorkflowServiceError>,
    /// The objects [`DelayedReads`] was asked for.
    reads: Vec<String>,
    /// Whether an isolate was built for the task.
    built: bool,
}

async fn replay_under(
    fixture: &Fixture,
    journal: serde_json::Value,
    objects: Vec<(Vec<u8>, Duration)>,
    limits: TaskPayloadLimits,
) -> Replayed {
    fixture
        .app
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let worker = WorkerIdentity::new("replay-order".into()).unwrap();
    let mut task = fixture
        .service
        .poll(&worker)
        .await
        .unwrap()
        .expect("a started run is deliverable");
    task.invocation.journal = serde_json::from_value(journal).unwrap();
    let transport = Rc::new(DelayedReads {
        tasks: fixture.app.tasks(worker, fixture.objects.clone()),
        objects,
        reads: RefCell::default(),
    });
    let executor = V8TaskExecutor::new(fixture.loader.clone(), transport.clone(), limits).unwrap();
    let guard = zeroship_workflow_runner::ExecutionGuard::new(Duration::from_secs(20)).unwrap();
    let mut execution =
        zeroship_workflow_runner::TaskExecutor::start(&executor, &task, guard.budget()).unwrap();
    let frontier = execution.wait().await.map(|execution| {
        execution
            .outcomes
            .iter()
            .map(|outcome| serde_json::to_value(outcome).unwrap())
            .collect()
    });
    execution.stop().await;
    let reads = transport.reads.borrow().clone();
    let built = !fixture.loader.probes.borrow().is_empty();
    Replayed {
        frontier,
        reads,
        built,
    }
}

/// [`replay_under`] the default host budgets, for the cases they admit.
async fn replay_over(
    fixture: &Fixture,
    journal: serde_json::Value,
    objects: Vec<(Vec<u8>, Duration)>,
) -> (Vec<serde_json::Value>, Vec<String>) {
    let replayed = replay_under(fixture, journal, objects, TaskPayloadLimits::default()).await;
    (replayed.frontier.unwrap(), replayed.reads)
}

fn child(ordinal: i32, name: &str, output: &[u8]) -> serde_json::Value {
    json!({"ordinal":ordinal,"name":name,"kind":"child","state":"completed",
        "output":null,"outputRef":object_ref(output),"error":null})
}

/// The frontier reduced to what identifies each outcome.
fn shape(outcomes: &[serde_json::Value]) -> Vec<serde_json::Value> {
    outcomes
        .iter()
        .map(|outcome| {
            json!([
                outcome["kind"],
                outcome["ordinal"],
                outcome["name"],
                outcome["output"]
            ])
        })
        .collect()
}

/// A replay reaches its ordinals in the order the journal holds them, however
/// the objects behind the children it resolves arrive.
///
/// The journal is what an earlier dispatch wrote: both children completed, and
/// `x`, which follows `A`, at the ordinal after them. `A`'s object is the slow
/// one, so a body that waited on the reads would have `B`'s value first, issue
/// `y` at that ordinal, and find `x` journaled there.
#[compio::test]
async fn a_replay_issues_its_steps_in_journal_order_whatever_order_child_outputs_arrive() {
    let fixture = Fixture::new(SIBLINGS).await;
    let (outcomes, reads) = replay_over(
        &fixture,
        json!([
            child(0, "A", CHILD_A),
            child(1, "B", CHILD_B),
            {"ordinal":2,"name":"x","kind":"run","state":"completed","output":"A","error":null},
        ]),
        vec![(CHILD_A.to_vec(), SLOW), (CHILD_B.to_vec(), Duration::ZERO)],
    )
    .await;
    assert_eq!(
        reads.len(),
        2,
        "both children's objects must be read: {reads:?}"
    );
    assert_eq!(
        shape(&outcomes),
        vec![json!(["StepCompleted", 3, "y", "B"])],
        "{outcomes:?}"
    );
}

/// THE CONTROL, differing in the delays alone: with `B`'s object the slow one,
/// the reads complete in journal order, so the case above fails only because
/// they did not.
#[compio::test]
async fn a_replay_whose_child_outputs_arrive_in_journal_order_issues_the_same_steps() {
    let fixture = Fixture::new(SIBLINGS).await;
    let (outcomes, _) = replay_over(
        &fixture,
        json!([
            child(0, "A", CHILD_A),
            child(1, "B", CHILD_B),
            {"ordinal":2,"name":"x","kind":"run","state":"completed","output":"A","error":null},
        ]),
        vec![(CHILD_A.to_vec(), Duration::ZERO), (CHILD_B.to_vec(), SLOW)],
    )
    .await;
    assert_eq!(
        shape(&outcomes),
        vec![json!(["StepCompleted", 3, "y", "B"])],
        "{outcomes:?}"
    );
}

/// A completed child's value is there in the round its record resolves in, so
/// the step that follows it joins the same frontier as the sibling still
/// running. A body that waited on the read would submit the frontier without
/// that step and run its effect after the frontier was already sealed, where
/// nothing records it.
#[compio::test]
async fn a_completed_childs_follow_on_step_joins_the_frontier_of_a_running_sibling() {
    let fixture = Fixture::new(SIBLINGS).await;
    let (outcomes, _) = replay_over(
        &fixture,
        json!([
            child(0, "A", CHILD_A),
            {"ordinal":1,"name":"B","kind":"child","state":"running","output":null,"error":null},
        ]),
        vec![(CHILD_A.to_vec(), SLOW)],
    )
    .await;
    assert_eq!(
        shape(&outcomes),
        vec![
            json!(["Child", 1, "B", null]),
            json!(["StepCompleted", 2, "x", "A"]),
        ],
        "{outcomes:?}"
    );
}

/// A child that returned nothing reached the journal as JSON null with no
/// object. Its parent receives null, and nothing is read for it.
#[compio::test]
async fn a_child_that_returned_nothing_resolves_to_null_without_a_read() {
    let fixture = Fixture::new(
        r"
        export class Quiet { async run() {} }
        export class Example {
            async run(_trigger, step) {
                const quiet = await step.call(Quiet, {});
                await step.run('saw', () => ({ quiet }));
                return 'done';
            }
        }
    ",
    )
    .await;
    let (outcomes, reads) = replay_over(
        &fixture,
        json!([{"ordinal":0,"name":"Quiet","kind":"child","state":"completed",
            "output":null,"error":null}]),
        Vec::new(),
    )
    .await;
    assert!(reads.is_empty(), "{reads:?}");
    assert_eq!(
        shape(&outcomes),
        vec![json!(["StepCompleted", 1, "saw", {"quiet":null}])],
        "{outcomes:?}"
    );
}

/// A parent that calls one child and records what it saw of it.
const ONE_CHILD: &str = r"
    export class A { async run() { return {}; } }
    export class Example {
        async run(_trigger, step) {
            const a = await step.call(A, {});
            await step.run('saw', () => a.from);
            return 'done';
        }
    }
";

/// A child output of exactly `size` bytes whose `from` is `A`.
fn child_output_of(size: usize) -> Vec<u8> {
    let frame = br#"{"from":"A","pad":""}"#.len();
    let output =
        serde_json::to_vec(&json!({"from": "A", "pad": "x".repeat(size - frame)})).unwrap();
    assert_eq!(output.len(), size);
    output
}

/// The largest child output the platform admits is replayed by a host on the
/// default budgets: its bytes reach the parent, which reads a field of it.
///
/// The isolate runs with no CPU limit. Parsing an envelope this large is work
/// the isolate does inside its dispatch, so under an app's CPU limit the same
/// replay can be cut short; this case is about what the host reads and splices.
#[compio::test]
async fn a_child_output_at_the_payload_ceiling_is_replayed() {
    let fixture = Fixture::with_limits(ONE_CHILD, None, AppPolicy::default()).await;
    let output = child_output_of(zeroship_core::workflow_policy::MAX_PAYLOAD_BYTES_CEILING);
    let replayed = replay_under(
        &fixture,
        json!([child(0, "A", &output)]),
        vec![(output, Duration::ZERO)],
        TaskPayloadLimits::default(),
    )
    .await;
    let frontier = replayed.frontier.unwrap();
    assert_eq!(
        shape(&frontier),
        vec![json!(["StepCompleted", 1, "saw", "A"])],
        "{frontier:?}"
    );
    assert_eq!(replayed.reads.len(), 1);
}

/// Host budgets that read one child of `CHILD_SIZE` bytes and replay
/// `max_replay_bytes` of children's outputs.
const fn replay_limits(max_replay_bytes: usize) -> TaskPayloadLimits {
    TaskPayloadLimits {
        max_inline_bytes: 1024,
        max_payload_bytes: CHILD_SIZE,
        max_result_bytes: 1024 * 1024,
        max_replay_bytes,
    }
}
const CHILD_SIZE: usize = 3000;

/// Replay two completed children of `CHILD_SIZE` bytes each under a host
/// replay budget of `max_replay_bytes`.
async fn replay_two_within(max_replay_bytes: usize) -> Replayed {
    let fixture = Fixture::new(SIBLINGS).await;
    let first = child_output_of(CHILD_SIZE);
    let second = child_output_of(CHILD_SIZE)
        .iter()
        .map(|byte| if *byte == b'A' { b'B' } else { *byte })
        .collect::<Vec<u8>>();
    replay_under(
        &fixture,
        json!([child(0, "A", &first), child(1, "B", &second)]),
        vec![(first, Duration::ZERO), (second, Duration::ZERO)],
        replay_limits(max_replay_bytes),
    )
    .await
}

/// The replay budget is a boundary assertion: a journal whose children's
/// outputs sum past it, which a correctly configured host never receives, is
/// refused before anything is read and before an isolate is built.
#[compio::test]
async fn child_outputs_past_the_replay_budget_never_read_or_build_an_isolate() {
    let replayed = replay_two_within(2 * CHILD_SIZE - 1).await;
    assert!(
        matches!(replayed.frontier, Err(WorkflowServiceError::Internal(_))),
        "{:?}",
        replayed.frontier
    );
    assert!(replayed.reads.is_empty(), "{:?}", replayed.reads);
    assert!(
        !replayed.built,
        "an isolate was built past the replay budget"
    );
}

/// THE CONTROL, differing in the budget alone: at the children's total, both
/// outputs are read and replayed.
#[compio::test]
async fn child_outputs_inside_the_replay_budget_are_read_and_replayed() {
    let replayed = replay_two_within(2 * CHILD_SIZE).await;
    assert_eq!(
        shape(&replayed.frontier.unwrap()),
        vec![
            json!(["StepCompleted", 2, "x", "A"]),
            json!(["StepCompleted", 3, "y", "B"]),
        ]
    );
    assert_eq!(replayed.reads.len(), 2);
    assert!(replayed.built);
}

/// A child output the host cannot read stops the task before an isolate is
/// built, so no creator code runs over a journal missing a value. The control
/// is the case above, where the output is readable and an isolate is built.
#[compio::test]
async fn a_child_output_the_host_cannot_read_never_builds_an_isolate() {
    let fixture = Fixture::new(ONE_CHILD).await;
    // No object is held for this descriptor, so the read reaches the task
    // host, which holds none either.
    let replayed = replay_under(
        &fixture,
        json!([child(0, "A", CHILD_A)]),
        Vec::new(),
        TaskPayloadLimits::default(),
    )
    .await;
    assert!(replayed.frontier.is_err(), "{:?}", replayed.frontier);
    assert!(
        !replayed.built,
        "an isolate was built over an unreadable child output"
    );
}

/// A replay whose children only partly read stops the task before an isolate is
/// built, so no creator code runs over a journal missing a value. One child's
/// object is held and the other names none; the read that succeeds must not
/// build an envelope with a hole where the other value belongs.
#[compio::test]
async fn a_child_output_that_partly_fails_never_builds_an_isolate() {
    let fixture = Box::pin(Fixture::new(SIBLINGS)).await;
    let replayed = replay_under(
        &fixture,
        json!([child(0, "A", CHILD_A), child(1, "B", CHILD_B)]),
        // Only `A`'s object is held, so `B`'s read reaches the task host, which
        // holds none.
        vec![(CHILD_A.to_vec(), Duration::ZERO)],
        TaskPayloadLimits::default(),
    )
    .await;
    assert!(replayed.frontier.is_err(), "{:?}", replayed.frontier);
    assert_eq!(
        replayed.reads,
        vec![String::from_utf8_lossy(CHILD_A).into_owned()],
        "the readable child's object must have been read"
    );
    assert!(
        !replayed.built,
        "an isolate was built over a journal with an unreadable child output"
    );
}

/// A parent that calls a child whose output is wider than [`NARROW`], and
/// either catches what `step.call` throws or lets it fail the run.
fn parent_of_a_wide_child(catches: bool) -> String {
    let call = if catches {
        "try { await step.call(Wide, {}); return 'attached'; } \
         catch (error) { return 'caught ' + error.name; }"
    } else {
        "return (await step.call(Wide, {})).length;"
    };
    format!(
        "export class Wide {{ async run() {{ return 'x'.repeat(4096); }} }}
        export class Example {{ async run(_trigger, step) {{ {call} }} }}"
    )
}

/// A child-output bound the child above is wider than on its own.
const NARROW: usize = 1024;

/// Drive a run of `source` under a policy bounding child outputs at
/// `max_child_output_bytes` until it settles.
async fn settle_with_child_bound(
    source: &str,
    max_child_output_bytes: usize,
) -> (Fixture, RunStatus, String) {
    let fixture = Fixture::with_workflows(
        source,
        Some(Duration::from_millis(100)),
        AppPolicy {
            max_child_output_bytes,
            ..AppPolicy::default()
        },
        &["Example", "Wide"],
    )
    .await;
    let run = fixture.start(json!({})).await;
    let manager = Manager::new(&fixture).await;
    let mut consumer = manager.consumer(&fixture, 1);
    // Terminal, not merely suspended: the parent suspends while its child
    // runs, and the child reaches it through the maintenance lane.
    let settled = RefCell::new(None);
    let _ = compio::time::timeout(
        Duration::from_mins(1),
        consumer.run_until(async {
            loop {
                manager.publish(&fixture.app).await;
                manager.maintain(&fixture).await;
                let status = fixture.app.status(&run.id).await.unwrap();
                if status.state.is_terminal() {
                    *settled.borrow_mut() = Some(status);
                    return;
                }
                compio::time::sleep(Duration::from_millis(2)).await;
            }
        }),
    )
    .await;
    let settled = settled
        .into_inner()
        .expect("the parent settles rather than wedging on its child");
    (fixture, settled, run.id)
}

/// A child whose output would carry its parent past the child-output bound
/// fails the parent's `step.call` with `LimitExceededError`, which the parent
/// catches and carries on from: the run settles rather than wedging.
#[compio::test]
async fn a_child_output_over_the_bound_is_thrown_into_the_parent() {
    let (fixture, settled, run) =
        settle_with_child_bound(&parent_of_a_wide_child(true), NARROW).await;
    assert_eq!(settled.state, RunState::Completed, "{settled:?}");
    assert_eq!(
        returned_value(&fixture, &run).await,
        json!("caught LimitExceededError")
    );
}

/// Uncaught, the same failure settles the parent as failed with that class.
#[compio::test]
async fn an_uncaught_child_output_over_the_bound_fails_the_parent() {
    let (_fixture, settled, _run) =
        settle_with_child_bound(&parent_of_a_wide_child(false), NARROW).await;
    assert_eq!(settled.state, RunState::Failed, "{settled:?}");
    assert_eq!(
        settled.error.as_ref().map(|error| error["type"].clone()),
        Some(json!("LimitExceededError")),
        "{settled:?}"
    );
}

/// THE CONTROL, differing in the bound alone: under the default bound the same
/// child's output is attached and `step.call` resolves.
#[compio::test]
async fn a_child_output_inside_the_bound_is_attached() {
    let (fixture, settled, run) = settle_with_child_bound(
        &parent_of_a_wide_child(true),
        AppPolicy::default().max_child_output_bytes,
    )
    .await;
    assert_eq!(settled.state, RunState::Completed, "{settled:?}");
    assert_eq!(returned_value(&fixture, &run).await, json!("attached"));
}

#[path = "support/orm.rs"]
mod orm_fixture;

#[path = "../../../tests/fixtures/workflow_deployments.rs"]
mod deployment_fixture;
