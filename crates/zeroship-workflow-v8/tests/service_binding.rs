#![allow(
    clippy::future_not_send,
    reason = "compio and V8 execute on their owning thread"
)]

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{rc::Rc, sync::Arc, time::Duration};
use zeroship_core::{app_id::AppId, typed_id, workflow_coordination::AssignedScope};
use zeroship_runtime::{
    CancelFlag, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, Runtime, SettledFetch,
};
use zeroship_storage::backend::OnceChunk;
use zeroship_workflow::{
    engine::WorkflowOutputRef,
    operations::{RunState, StartOptions},
    service::{
        AppPolicy, AppWorkflows, DeployRegistration, HostPolicies, PolicyBinding, PolicySnapshot,
        RequestId, WorkerIdentity, WorkflowService,
    },
    WorkflowExecution,
};
use zeroship_workflow_runner::{
    ready::ReadyApps, remote::RemoteBackend, ObjectStepOutputs, PayloadObjects, WorkerPayloads,
};
use zeroship_workflow_v8::WorkflowBinding;

struct Fixture {
    _directory: tempfile::TempDir,
    objects: PayloadObjects,
    service: WorkflowService,
    app: AppWorkflows,
    other: AppWorkflows,
    /// Held so both apps keep the policy generation they were opened under.
    _bindings: [PolicyBinding; 2],
}
impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let policies = Arc::new(HostPolicies::default());
        let service = WorkflowService::open(
            std::rc::Rc::new(orm_fixture::store(directory.path()).await),
            policies.clone(),
        )
        .await
        .unwrap();
        let deployments = deployment_fixture::Deployments::new().await;
        let app = AppId::mint();
        let other = AppId::mint();
        let service = service.with_deployments(deployments.binding(&[&app, &other]));
        let bindings = [policies.bind(app).unwrap(), policies.bind(other).unwrap()];
        let mut apps = Vec::new();
        for binding in &bindings {
            binding
                .begin_refresh()
                .unwrap()
                .install(
                    PolicySnapshot::configuration(1.try_into().unwrap(), AppPolicy::default())
                        .unwrap()
                        .with_ingress_epoch(Some(1.try_into().unwrap())),
                )
                .unwrap();
            let app = service.register_app(binding).await.unwrap();
            deployments
                .activate(
                    &service,
                    app.app_id(),
                    &DeployRegistration {
                        id: typed_id::generate("dep"),
                        hash: "a".repeat(64),
                        workflows: ["Example".into()].into(),
                        schedules: Vec::new(),
                    },
                )
                .await
                .unwrap();
            apps.push(app);
        }
        let [app, other]: [AppWorkflows; 2] = apps.try_into().unwrap();
        Self {
            objects: PayloadObjects::open(zeroship_storage::StorageStore::from_backend(
                Arc::new(zeroship_storage::LocalFs::new(directory.path().join("payloads"))),
            ))
            .unwrap(),
            _directory: directory,
            app,
            other,
            service,
            _bindings: bindings,
        }
    }

    /// The placement a published backend is installed and retired by.
    fn placement(&self, app: &AppId, revision: i64) -> AssignedScope {
        AssignedScope {
            app_id: app.clone(),
            assignment_revision: revision.try_into().unwrap(),
        }
    }

    /// A backend for `app` that reaches the workflow service at `origin`.
    fn remote(&self, app: &AppId, revision: i64, origin: &str) -> RemoteBackend {
        let issuer = zeroship_core::service_assertion::ServiceIssuer::parse(&format!(
            "spiffe://zeroship.ai/svc/worker/{}",
            zeroship_core::workflow_coordination::WorkerId::mint().as_str()
        ))
        .unwrap();
        let auth = Arc::new(zeroship_core::service_peers::ServiceAuth::new(
            zeroship_core::service_peers::ServiceKeyring::from_parts(
                issuer,
                zeroship_core::service_assertion::ServiceSigningKey::generate(),
                zeroship_core::service_assertion::ServiceTrustBundle::new(),
            )
            .unwrap(),
            Arc::new(
                zeroship_core::service_assertion::TransportAssertionVerifier::new(
                    zeroship_core::service_assertion::ServiceTrustBundle::new(),
                ),
            ),
        ));
        RemoteBackend::new(
            zeroship_workflow_client::WorkerCoordinator::new(
                origin,
                auth,
                zeroship_workflow_client::Options {
                    timeout: Duration::from_millis(250),
                    ..zeroship_workflow_client::Options::default()
                },
            )
            .unwrap(),
            self.placement(app, revision),
            self.objects.clone(),
            64 * 1024,
        )
        .unwrap()
    }

    fn step_outputs(&self) -> zeroship_workflow::SharedStepOutputs {
        Arc::new(ObjectStepOutputs::new(self.objects.clone(), 1024).unwrap())
    }

    /// The store a backend stages the values its callers start runs from into.
    fn input_stager(&self) -> zeroship_workflow::SharedInputStager {
        Arc::new(self.objects.clone())
    }

    /// A request isolate whose `env.workflows` resolves published backends.
    fn ready_runtime(&self, apps: &ReadyApps, identity: &AppId, source: &str) -> Runtime {
        zeroship_runtime::init_v8();
        Runtime::builder()
            .modules(vec![ModuleEntry {
                specifier: "index.js".into(),
                source: source.into(),
            }])
            .plugins(vec![Arc::new(WorkflowBinding::ready(apps.clone()))])
            // Mutable input naming the other app must not select its backend.
            .env_vars([("APP_ID".into(), self.other.app_id().as_str().to_owned())].into())
            .app_id(identity.clone())
            .build()
    }

    fn runtime(&self, app: &AppWorkflows, identity: Option<AppId>, source: &str) -> Runtime {
        zeroship_runtime::init_v8();
        let mut builder = Runtime::builder()
            .modules(vec![ModuleEntry {
                specifier: "index.js".into(),
                source: source.into(),
            }])
            .plugins(vec![Arc::new(WorkflowBinding::service(
                app.clone()
                    .into_backend(&self.service, self.step_outputs(), self.input_stager())
                    .unwrap(),
            ))])
            // Deliberately conflicting and mutable input must not grant authority.
            .env_vars([("APP_ID".into(), self.other.app_id().as_str().to_owned())].into());
        if let Some(identity) = identity {
            builder = builder.app_id(identity);
        }
        builder.build()
    }
}

async fn fetch(runtime: Runtime) -> Value {
    runtime.start_pump();
    let outcome = runtime.call_fetch_handler(
        "GET",
        "http://localhost/",
        &[],
        "",
        &EnvSnapshot::empty(),
        RequestCtx::new(CancelFlag::new()),
    );
    runtime.exit_isolate();
    let (status, body) = match outcome {
        FetchOutcome::Response { status, body, .. } => (status, body),
        FetchOutcome::Pending { rx, .. } => {
            match compio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                SettledFetch::Response { status, body, .. } => (status, body),
                _ => panic!("expected a buffered response"),
            }
        }
        _ => panic!("expected a buffered response"),
    };
    runtime.shutdown().await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    serde_json::from_slice(&body).unwrap()
}

#[compio::test]
async fn binding_uses_host_identity_for_lifecycle_operations() {
    let fixture = Fixture::new().await;
    let result = fetch(fixture.runtime(
        &fixture.app,
        Some(fixture.app.app_id().clone()),
        r"
        export default { async fetch(_request, env) {
            const run = await env.workflows.Example.start({input:{secret:'app-a'}});
            const before = await run.status();
            const signal = await run.signal({type:'ready',payload:true});
            const paused = await run.pause();
            const resumed = await run.resume();
            const cancelled = await run.cancel();
            const restarted = await run.restart();
            return Response.json({id:run.id, before, signal, paused, resumed, cancelled,
                restarted:await restarted.status()});
        }};
    ",
    ))
    .await;
    let run = result["id"].as_str().unwrap();
    assert_eq!(result["before"]["state"], "queued");
    assert_eq!(result["paused"]["state"], "paused");
    assert_eq!(result["resumed"]["state"], "queued");
    // Cancellation records intent; maintenance resolves the terminal state.
    assert_eq!(result["cancelled"]["state"], "queued");
    assert_eq!(result["restarted"]["state"], "queued");
    assert!(result["signal"]["id"].as_str().is_some());
    assert_eq!(
        fixture.app.status(run).await.unwrap().state,
        RunState::Queued
    );
    assert!(fixture.other.status(run).await.is_err());
}

#[compio::test]
async fn binding_reads_outputs_and_denies_foreign_run_handles() {
    let fixture = Fixture::new().await;
    let run = fixture
        .app
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let worker = WorkerIdentity::new("binding-test".into()).unwrap();
    let task = fixture.service.poll(&worker).await.unwrap().unwrap();
    fixture
        .service
        .complete(
            &worker,
            &task.id,
            &task.token,
            WorkflowExecution::from_runtime_value(json!({"outcomes":[
                {"kind":"StepCompleted","ordinal":0,"name":"value","output":{"secret":"app-a"}},
                {"kind":"RunCompleted"}
            ]}))
            .unwrap(),
        )
        .await
        .unwrap();
    let source = format!(
        r"
        export default {{ async fetch(_request, env) {{
            const run = env.workflows.Example.get({});
            try {{
                const output = await run.readStepOutput('value',0);
                return Response.json({{output:JSON.parse(new TextDecoder().decode(output))}});
            }} catch (error) {{ return Response.json({{code:error.code}}); }}
        }} }};
    ",
        serde_json::to_string(&run.id).unwrap()
    );
    let owner =
        fetch(fixture.runtime(&fixture.app, Some(fixture.app.app_id().clone()), &source)).await;
    assert_eq!(owner, json!({"output":{"secret":"app-a"}}));
    let stranger = fetch(fixture.runtime(
        &fixture.other,
        Some(fixture.other.app_id().clone()),
        &source,
    ))
    .await;
    assert_eq!(stranger, json!({"code":"workflow_not_found"}));
}

/// A creator holding only a run id reads that run's final output. The three
/// fetches below share one handler source and one call, and differ in exactly
/// one variable each: who is asking, and whether the run returned anything for
/// its payload to hold.
#[compio::test]
async fn binding_reads_a_staged_run_output_and_denies_foreign_run_handles() {
    let fixture = Fixture::new().await;
    let worker = WorkerIdentity::new("output-binding".into()).unwrap();
    let bytes = br#"{"secret":"app-a"}"#;
    let reference = WorkflowOutputRef {
        hash: format!("{:x}", Sha256::digest(bytes)),
        size: i64::try_from(bytes.len()).unwrap(),
        content_type: Some("application/json".into()),
    };
    let staged = fixture
        .app
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let task = fixture.service.poll(&worker).await.unwrap().unwrap();
    fixture
        .service
        .payloads(&fixture.objects)
        .stage(
            &worker,
            &task.id,
            &task.token,
            &RequestId::mint(),
            reference.clone(),
            Box::new(OnceChunk::new(bytes.to_vec().into())),
        )
        .await
        .unwrap();
    fixture
        .service
        .complete(
            &worker,
            &task.id,
            &task.token,
            WorkflowExecution::from_runtime_value(json!({"outcomes":[
                {"kind":"RunCompleted","outputRef":reference}
            ]}))
            .unwrap(),
        )
        .await
        .unwrap();
    // The premise this capability exists for: status hands out a descriptor,
    // so the bytes are unreachable without a read of their own.
    let described = fixture
        .app
        .status(&staged.id)
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(described["kind"], "ref", "{described}");
    assert_eq!(described["hash"], reference.hash);

    // The control: same app, same call, a run that returned nothing, so no
    // object was ever written for it and status names none.
    let empty = fixture
        .app
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let task = fixture.service.poll(&worker).await.unwrap().unwrap();
    fixture
        .service
        .complete(
            &worker,
            &task.id,
            &task.token,
            WorkflowExecution::from_runtime_value(json!({"outcomes":[
                {"kind":"RunCompleted"}
            ]}))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        fixture.app.status(&empty.id).await.unwrap().output,
        None,
        "a run that returned nothing must name no payload"
    );

    let source = |run: &str| {
        format!(
            r"
        export default {{ async fetch(_request, env) {{
            const run = env.workflows.Example.get({});
            try {{
                const output = await run.readOutput();
                return Response.json({{output:JSON.parse(new TextDecoder().decode(output))}});
            }} catch (error) {{ return Response.json({{code:error.code}}); }}
        }} }};
    ",
            serde_json::to_string(run).unwrap()
        )
    };
    let owner = fetch(fixture.runtime(
        &fixture.app,
        Some(fixture.app.app_id().clone()),
        &source(&staged.id),
    ))
    .await;
    assert_eq!(owner, json!({"output":{"secret":"app-a"}}));
    let stranger = fetch(fixture.runtime(
        &fixture.other,
        Some(fixture.other.app_id().clone()),
        &source(&staged.id),
    ))
    .await;
    assert_eq!(stranger, json!({"code":"workflow_not_found"}));
    // A run that returned nothing owns no object, so the app that owns the run
    // finds nothing to open on it either. Without this arm the owner's read
    // above could be reporting bytes the call never had to find.
    let empty_read = fetch(fixture.runtime(
        &fixture.app,
        Some(fixture.app.app_id().clone()),
        &source(&empty.id),
    ))
    .await;
    assert_eq!(empty_read, json!({"code":"workflow_not_found"}));
}

#[compio::test]
async fn mismatched_or_missing_host_identity_rejects_before_creator_evaluation() {
    let fixture = Fixture::new().await;
    for identity in [Some(fixture.app.app_id().clone()), None] {
        // APP_ID agrees with this binding; only the immutable host slot differs.
        let runtime = fixture.runtime(
            &fixture.other,
            identity,
            r"
            globalThis.creatorEvaluated = true;
            export default { fetch() { return new Response('evaluated'); } };
        ",
        );
        let error = runtime.initialize(&EnvSnapshot::empty()).await.unwrap_err();
        assert!(
            error.contains("workflow binding does not match runtime app identity"),
            "{error}"
        );
        assert!(!runtime.with_scope(|scope| {
            let global = scope.get_current_context().global(scope);
            let key = v8::String::new(scope, "creatorEvaluated").unwrap();
            global.get(scope, key.into()).unwrap().is_true()
        }));
        runtime.exit_isolate();
        runtime.shutdown().await;
    }
}

/// A published backend serves only the runtime identity it names.
///
/// The registry holds backends that reach the workflow service, so a resolved
/// backend and an unresolved one BOTH surface as `workflow_unavailable` to
/// creator code -- one because nothing is published, the other because the
/// service could not be reached. The code alone therefore cannot tell them
/// apart, and what does is whether the call left this process at all: each
/// installed backend is pointed at its own listener, and the connection count is
/// the evidence.
///
/// The value-staging half of the creator start now happens at the far end, and is
/// bound in `zeroship-workflow-runner`'s
/// `a_remote_start_refuses_a_creator_supplied_payload_descriptor`.
#[compio::test]
async fn ready_binding_reaches_only_the_published_backend_of_its_runtime_identity() {
    let fixture = Fixture::new().await;
    let apps = ReadyApps::default();
    let identity = fixture.app.app_id().clone();
    let other = fixture.other.app_id().clone();
    let mine = Dialled::new().await;
    let theirs = Dialled::new().await;
    let source = r"
        export default { async fetch(_request, env) {
            try {
                const run = await env.workflows.Example.start({input:{secret:'app-a'}});
                return Response.json({id:run.id});
            } catch (error) { return Response.json({code:error.code}); }
        }};
    ";
    let refused = json!({"code":"workflow_unavailable"});
    // Nothing published: a retryable refusal, and no call leaves.
    assert_eq!(
        fetch(fixture.ready_runtime(&apps, &identity, source)).await,
        refused
    );
    assert_eq!((mine.dialled(), theirs.dialled()), (0, 0));
    // Another app's published backend never serves this identity, whatever the
    // mutable environment names.
    apps.install(fixture.remote(&other, 1, &theirs.url));
    assert_eq!(
        fetch(fixture.ready_runtime(&apps, &identity, source)).await,
        refused
    );
    assert_eq!(
        (mine.dialled(), theirs.dialled()),
        (0, 0),
        "another app's backend must not be reached for this identity"
    );
    // This identity's own backend is resolved, and the call leaves for the
    // service it names.
    apps.install(fixture.remote(&identity, 1, &mine.url));
    assert_eq!(
        fetch(fixture.ready_runtime(&apps, &identity, source)).await,
        refused
    );
    assert_eq!((mine.dialled(), theirs.dialled()), (1, 0));
    // Retiring the other app's placement leaves this one published.
    apps.retire(&fixture.placement(&other, 1));
    assert_eq!(
        fetch(fixture.ready_runtime(&apps, &identity, source)).await,
        refused
    );
    assert_eq!((mine.dialled(), theirs.dialled()), (2, 0));
    apps.retire(&fixture.placement(&identity, 1));
    assert_eq!(
        fetch(fixture.ready_runtime(&apps, &identity, source)).await,
        refused
    );
    assert_eq!(
        (mine.dialled(), theirs.dialled()),
        (2, 0),
        "a retired placement must stop being reached"
    );
}

/// A listener that counts the connections a backend opens to it and closes each.
///
/// No protocol: what is being asserted is that the call LEFT, which a creator
/// error code cannot show because a transport failure and an unpublished app
/// answer alike.
struct Dialled {
    url: String,
    dialled: Rc<std::cell::Cell<usize>>,
    _handle: compio::runtime::JoinHandle<()>,
}

impl Dialled {
    async fn new() -> Self {
        let listener = compio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let dialled = Rc::new(std::cell::Cell::new(0usize));
        let counted = dialled.clone();
        let handle = compio::runtime::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                counted.set(counted.get() + 1);
                drop(stream);
            }
        });
        Self {
            url,
            dialled,
            _handle: handle,
        }
    }

    fn dialled(&self) -> usize {
        self.dialled.get()
    }
}

#[compio::test]
async fn engine_refusals_carry_a_code_and_shape_faults_stay_type_errors() {
    let fixture = Fixture::new().await;
    // Both calls are `restart` on the same live run, so the operation, the
    // handle and the run state are held fixed and only the `from` argument
    // differs. One names a step this journal does not hold, which the engine
    // refuses; the other is not a target object at all, which the binding
    // refuses before the engine is reached.
    let observed = fetch(fixture.runtime(
        &fixture.app,
        Some(fixture.app.app_id().clone()),
        r"
        const caught = async (call) => {
            try { await call(); return {outcome:'resolved'}; }
            catch (error) {
                return {outcome:'threw', name:error.name, code:error.code ?? null,
                    message:error.message, isTypeError:error instanceof TypeError};
            }
        };
        export default { async fetch(_request, env) {
            const run = await env.workflows.Example.start({});
            return Response.json({
                unresolvable: await caught(() => run.restart({from:{name:'absent'}})),
                malformed: await caught(() => run.restart({from:5})),
            });
        }};
    ",
    ))
    .await;
    // The engine refused a well-formed argument, so it is catchable by code
    // like every other refusal a control operation can produce.
    assert_eq!(
        observed["unresolvable"],
        json!({"outcome":"threw", "name":"Error", "code":"workflow_invalid_request",
            "message":"restart target is missing or ambiguous", "isTypeError":false}),
    );
    // The binding refused a wrongly typed argument, which is what
    // `instanceof TypeError` is for, and it carries no code.
    assert_eq!(observed["malformed"]["outcome"], "threw");
    assert_eq!(observed["malformed"]["name"], "TypeError");
    assert_eq!(observed["malformed"]["code"], Value::Null);
    assert_eq!(observed["malformed"]["isTypeError"], true);
}

#[path = "support/orm.rs"]
mod orm_fixture;

#[path = "../../../tests/fixtures/workflow_deployments.rs"]
mod deployment_fixture;
