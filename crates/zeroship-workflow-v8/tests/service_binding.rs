#![allow(
    clippy::future_not_send,
    reason = "compio and V8 execute on their owning thread"
)]

use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use zeroship_core::{app_id::AppId, typed_id};
use zeroship_runtime::{
    CancelFlag, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, Runtime, SettledFetch,
};
use zeroship_workflow::{
    operations::{RunState, StartOptions},
    service::{
        AppPolicy, AppWorkflows, DeployRegistration, HostPolicies, PolicySnapshot, RequestId,
        WorkerIdentity, WorkflowService,
    },
    WorkflowExecution,
};
use zeroship_workflow_v8::WorkflowBinding;

struct Fixture {
    _directory: tempfile::TempDir,
    service: WorkflowService,
    app: AppWorkflows,
    other: AppWorkflows,
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
            _directory: directory,
            app,
            other,
            service,
        }
    }

    fn runtime(&self, app: &AppWorkflows, identity: Option<AppId>, source: &str) -> Runtime {
        zeroship_runtime::init_v8();
        let mut builder = Runtime::builder()
            .modules(vec![ModuleEntry {
                specifier: "index.js".into(),
                source: source.into(),
            }])
            .plugins(vec![Arc::new(WorkflowBinding::service(
                app.clone().into_backend(1024).unwrap(),
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
                {"kind":"RunCompleted","output":"done"}
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

#[path = "support/orm.rs"]
mod orm_fixture;

#[path = "../../../tests/fixtures/workflow_deployments.rs"]
mod deployment_fixture;
