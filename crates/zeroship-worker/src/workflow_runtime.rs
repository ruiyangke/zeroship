//! Exclusive workflow isolates built from retained code and trusted app context.

use std::{collections::HashMap, rc::Rc, sync::Arc};
use zeroship_bundle::LoadedWorker;
use zeroship_core::{app_derivation, app_id::AppId, schema_name::SchemaName};
use zeroship_metering::Meter;
use zeroship_runtime::{
    transport::net_policy::NetPolicy, EnvSnapshot, ModuleEntry, NativePlugin, Runtime,
    RuntimeLimits,
};
use zeroship_workflow::{
    service::{AppBackend, TaskAssignment},
    WorkflowServiceError,
};
use zeroship_workflow_v8::{LoadedWorkflow, WorkflowBinding, WorkflowRuntimeLoader};

/// A snapshot already authorized by the worker's app metadata provider.
///
/// `env_vars` is visible to app JavaScript. Database credentials and schema
/// metadata belong in the native host; they must never be placed in that map.
/// Peers contain the ordinary native primitives, excluding `workflows`.
#[derive(Clone)]
pub struct WorkflowAppContext {
    pub app: AppId,
    pub schema: SchemaName,
    pub backend: AppBackend,
    pub env_vars: HashMap<String, String>,
    pub env: EnvSnapshot,
    pub limits: RuntimeLimits,
    pub net_policy: NetPolicy,
    pub peers: Vec<Arc<dyn NativePlugin>>,
    pub meter: Option<Arc<Meter>>,
}

impl std::fmt::Debug for WorkflowAppContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkflowAppContext")
            .field("app", &self.app)
            .finish_non_exhaustive()
    }
}

/// Runtime-local access to current, trusted metadata for an assigned app.
///
/// Implementations must refuse unknown apps and stale execution authority.
/// Resolution happens for each task; retaining an old snapshot in this loader
/// would hide env rotation and network-policy revocation from later executions.
pub trait WorkflowContextProvider {
    /// Resolve an authorized app snapshot without evaluating creator code.
    ///
    /// # Errors
    /// Refuse unknown apps, expired authority and unavailable required metadata.
    fn resolve(&self, app: &AppId) -> Result<WorkflowAppContext, WorkflowServiceError>;
}

/// Creates a fresh isolate for each execution; never shares an HTTP runtime.
/// The owning host initializes V8 and drives the executor on its compio thread.
pub struct WorkerWorkflowRuntimeLoader {
    contexts: Rc<dyn WorkflowContextProvider>,
}

impl std::fmt::Debug for WorkerWorkflowRuntimeLoader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkerWorkflowRuntimeLoader")
            .finish_non_exhaustive()
    }
}

impl WorkerWorkflowRuntimeLoader {
    #[must_use]
    pub fn new(contexts: Rc<dyn WorkflowContextProvider>) -> Self {
        Self { contexts }
    }

    fn build(
        &self,
        assignment: &TaskAssignment,
        executable: &LoadedWorker,
    ) -> Result<(Runtime, EnvSnapshot), WorkflowServiceError> {
        let app = AppId::parse(&assignment.invocation.app_id)
            .map_err(|_| invalid("invalid workflow runtime app identity"))?;
        if !zeroship_bundle::validate_hash_format(&assignment.invocation.deploy_hash) {
            return Err(invalid("invalid workflow runtime deployment hash"));
        }
        let mut context = self.contexts.resolve(&app)?;
        if context.app != app || context.backend.app_id() != &app {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        validate_peers(&context)?;
        if matches!(context.net_policy, NetPolicy::Trusted { .. }) {
            return Err(invalid(
                "creator workflow cannot use trusted network policy",
            ));
        }

        // Native primitives and the runtime share the authorized app identity.
        context
            .env_vars
            .insert("APP_ID".into(), app.as_str().to_owned());
        context.env_vars.insert(
            "ZEROSHIP_DEPLOY_ID".into(),
            assignment.invocation.deploy_hash.clone(),
        );
        context
            .peers
            .push(Arc::new(WorkflowBinding::service(context.backend)));
        let modules = std::iter::once(executable.entry())
            .chain(
                executable
                    .modules()
                    .keys()
                    .map(String::as_str)
                    .filter(|name| *name != executable.entry()),
            )
            .map(|name| ModuleEntry {
                specifier: name.to_owned(),
                source: executable.modules()[name].clone(),
            })
            .collect();
        let descriptor = executable
            .runtime_descriptor()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| invalid("invalid workflow runtime descriptor"))?;
        let mut builder = Runtime::builder()
            .app_id(app)
            .modules(modules)
            .runtime_descriptor(descriptor)
            .env_vars(context.env_vars)
            .plugins(context.peers)
            .limits(context.limits)
            .net_policy(context.net_policy);
        if let Some(meter) = context.meter {
            builder = builder.meter(meter);
        }
        // The executor installs its budget before initializing app modules.
        // Building must neither evaluate app code nor start the runtime pump.
        Ok((builder.build(), context.env))
    }
}

fn validate_peers(context: &WorkflowAppContext) -> Result<(), WorkflowServiceError> {
    let mut namespaces = std::collections::HashSet::new();
    for peer in &context.peers {
        let namespace = peer.namespace();
        if namespace == "workflows" || !namespaces.insert(namespace) {
            return Err(invalid("conflicting workflow runtime native peers"));
        }
        if namespace == "db" {
            // The V8 data adapter currently derives its physical schema from
            // APP_ID; it exposes no public explicit DbBinding injection hook.
            // Refuse a different host schema instead of opening the wrong one.
            let expected = app_derivation::schema_name(&context.app);
            if context.schema.as_str() != expected {
                return Err(invalid(
                    "workflow runtime database schema binding is unsupported",
                ));
            }
        }
    }
    Ok(())
}

fn invalid(message: &str) -> WorkflowServiceError {
    WorkflowServiceError::InvalidRequest(message.to_owned())
}

impl WorkflowRuntimeLoader for WorkerWorkflowRuntimeLoader {
    fn load(
        &self,
        assignment: &TaskAssignment,
        executable: &LoadedWorker,
    ) -> Result<LoadedWorkflow, WorkflowServiceError> {
        let (runtime, env) = self.build(assignment, executable)?;
        Ok(LoadedWorkflow::new(runtime, env))
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::future_not_send,
        reason = "workflow execution remains on its owning compio and V8 thread"
    )]

    use super::*;
    use serde_json::{json, Value};
    use std::{cell::RefCell, collections::BTreeMap, time::Duration};
    use zeroship_bundle::{
        BlobStore, LocalDiskBlobStore, Manifest, RuntimeDescriptorEntry, WorkerCode,
    };
    use zeroship_core::typed_id;
    use zeroship_data_orm::{
        binding::DbBinding, connection::ConnectionFactory, encryption::ProjectKeySource,
    };
    use zeroship_runtime::{
        plugin::NativeRegistrar, CancelFlag, FetchOutcome, RequestCtx, SettledFetch,
    };
    use zeroship_workflow::service::{schema, store::OrmStore, HostPolicies, WorkflowService};

    struct Contexts(RefCell<WorkflowAppContext>);
    impl WorkflowContextProvider for Contexts {
        fn resolve(&self, app: &AppId) -> Result<WorkflowAppContext, WorkflowServiceError> {
            let context = self.0.borrow();
            if &context.app != app {
                return Err(WorkflowServiceError::PermissionDenied);
            }
            Ok(context.clone())
        }
    }

    struct ForeignContext(WorkflowAppContext);
    impl WorkflowContextProvider for ForeignContext {
        fn resolve(&self, _app: &AppId) -> Result<WorkflowAppContext, WorkflowServiceError> {
            Ok(self.0.clone())
        }
    }

    struct Peer(&'static str);
    impl NativePlugin for Peer {
        fn namespace(&self) -> &str {
            self.0
        }
        fn register(&self, _registrar: &mut NativeRegistrar) {}
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        contexts: Rc<Contexts>,
        service: WorkflowService,
        blobs: LocalDiskBlobStore,
    }

    impl Fixture {
        async fn new() -> Self {
            zeroship_runtime::init_v8();
            let directory = tempfile::tempdir().unwrap();
            let app = AppId::mint();
            let tenant = app_derivation::schema_name(&app);
            let store = OrmStore::connect(
                DbBinding::new(&tenant, "fixture", SchemaName::new(&tenant).unwrap()),
                &ConnectionFactory::for_url(&format!(
                    "sqlite:{}",
                    directory.path().join("app.sqlite").display()
                ))
                .unwrap(),
                ProjectKeySource::unavailable(),
            )
            .await
            .unwrap();
            schema::initialize_local(&store).await.unwrap();
            let service = WorkflowService::open(Rc::new(store), Arc::new(HostPolicies::default()))
                .await
                .unwrap();
            let context = WorkflowAppContext {
                schema: SchemaName::new(&tenant).unwrap(),
                backend: service.for_app(app.clone()).into_backend(1024).unwrap(),
                app,
                env_vars: HashMap::from([
                    ("APP_ID".into(), "forged-app".into()),
                    ("ZEROSHIP_DEPLOY_ID".into(), "live-deployment".into()),
                ]),
                env: EnvSnapshot::new(
                    BTreeMap::from([("COLOR".into(), "blue".into())]),
                    BTreeMap::from([("SECRET".into(), "private-value".into())]),
                    Vec::new(),
                ),
                limits: RuntimeLimits {
                    cpu_limit: Some(Duration::from_secs(1)),
                    wall_timeout: Some(Duration::from_secs(2)),
                    heap_limit_bytes: Some(64 * 1024 * 1024),
                },
                net_policy: NetPolicy::Denied,
                peers: vec![Arc::new(Peer("auth"))],
                meter: Some(Arc::new(Meter::new())),
            };
            Self {
                contexts: Rc::new(Contexts(RefCell::new(context))),
                service,
                blobs: LocalDiskBlobStore::new(directory.path().join("bundles")).unwrap(),
                _directory: directory,
            }
        }

        fn loader(&self) -> WorkerWorkflowRuntimeLoader {
            WorkerWorkflowRuntimeLoader::new(self.contexts.clone())
        }

        fn assignment(&self) -> TaskAssignment {
            let run = typed_id::generate("wfr");
            serde_json::from_value(json!({
                "id":typed_id::generate("wft"), "token":"a".repeat(64),
                "generation":1, "epoch":1, "deadline":10000, "leaseMs":1000,
                "invocation": {
                    "appId":self.contexts.0.borrow().app.as_str(),
                    "deployId":typed_id::generate("dep"), "deployHash":"b".repeat(64),
                    "runId":run, "workflowName":"Example", "phase":"forward",
                    "trigger": {"input":null, "startedAt":"2026-01-01T00:00:00Z",
                        "runId":run, "workflowName":"Example"},
                    "journal":[]
                }
            }))
            .unwrap()
        }

        async fn executable(&self, source: &str, descriptor: Option<Value>) -> LoadedWorker {
            let source_hash = zeroship_bundle::sha256_hex(source.as_bytes());
            self.blobs
                .put_blob(&source_hash, source.as_bytes())
                .await
                .unwrap();
            let mut manifest = Manifest {
                worker: Some(WorkerCode {
                    entry: "index.js".into(),
                    modules: [("index.js".into(), source_hash)].into(),
                }),
                ..Manifest::default()
            };
            if let Some(descriptor) = descriptor {
                let bytes = serde_json::to_vec(&descriptor).unwrap();
                let hash = zeroship_bundle::sha256_hex(&bytes);
                self.blobs.put_blob(&hash, &bytes).await.unwrap();
                manifest.runtime_descriptor = Some(RuntimeDescriptorEntry { hash });
            }
            LoadedWorker::load(&manifest, &self.blobs, 1024 * 1024)
                .await
                .unwrap()
        }
    }

    #[compio::test]
    async fn binds_pinned_code_descriptor_identity_policy_and_meter_without_evaluation() {
        let fixture = Fixture::new().await;
        let source = "throw new Error('creator-module-evaluated'); export default {};";
        let descriptor = json!({"version":2, "collections":{}});
        let executable = fixture.executable(source, Some(descriptor.clone())).await;
        let assignment = fixture.assignment();
        let policy = NetPolicy::rules(Vec::new(), 3, 1024).unwrap();
        fixture.contexts.0.borrow_mut().net_policy = policy.clone();
        let (runtime, env) = fixture.loader().build(&assignment, &executable).unwrap();
        assert_eq!(
            runtime.clone().into_inner_probe_for_test().strong_count(),
            1
        );
        assert_eq!(runtime.app_id(), Some(&fixture.contexts.0.borrow().app));
        assert_eq!(runtime.limits(), fixture.contexts.0.borrow().limits);
        assert_eq!(runtime.modules()[0].source, source);
        {
            let state = runtime.state();
            let state = state.borrow();
            assert_eq!(
                state.env_vars["APP_ID"],
                fixture.contexts.0.borrow().app.as_str()
            );
            assert_eq!(
                state.env_vars["ZEROSHIP_DEPLOY_ID"],
                assignment.invocation.deploy_hash
            );
            assert_eq!(state.env_vars.len(), 2);
            assert_eq!(state.net_policy, policy);
            assert_eq!(
                serde_json::from_str::<Value>(state.runtime_descriptor.as_ref().unwrap()).unwrap(),
                descriptor
            );
            let meter = state.meter.as_ref().unwrap();
            assert_eq!(meter.app_id(), fixture.contexts.0.borrow().app.as_str());
            meter.record("egress_bytes", 7);
        }
        let events = fixture.contexts.0.borrow().meter.as_ref().unwrap().drain();
        assert!(events
            .iter()
            .any(|event| event.subject.app.as_ref() == runtime.app_id()
                && event.meter == "egress_bytes"
                && event.value == 7));
        // Initialization remains the executor's responsibility; this marker
        // must be reached only when the host explicitly initializes the runtime.
        let error = runtime.initialize(&env).unwrap_err();
        assert!(error.contains("creator-module-evaluated"), "{error}");
        runtime.exit_isolate();
        runtime.shutdown().await;
    }

    #[compio::test]
    async fn refuses_unknown_app_mismatched_snapshot_and_foreign_backend() {
        let fixture = Fixture::new().await;
        let executable = fixture.executable("export default {};", None).await;
        let mut assignment = fixture.assignment();
        assignment.invocation.deploy_hash = "not-a-deployment-hash".into();
        assert!(matches!(
            fixture.loader().load(&assignment, &executable),
            Err(WorkflowServiceError::InvalidRequest(_))
        ));
        assignment.invocation.deploy_hash = "b".repeat(64);
        assignment.invocation.app_id = uuid::Uuid::new_v4().to_string();
        assert!(matches!(
            fixture.loader().load(&assignment, &executable),
            Err(WorkflowServiceError::InvalidRequest(_))
        ));
        assignment.invocation.app_id = AppId::mint().as_str().to_owned();
        assert!(matches!(
            fixture.loader().load(&assignment, &executable),
            Err(WorkflowServiceError::PermissionDenied)
        ));
        let dishonest = WorkerWorkflowRuntimeLoader::new(Rc::new(ForeignContext(
            fixture.contexts.0.borrow().clone(),
        )));
        assert!(matches!(
            dishonest.load(&assignment, &executable),
            Err(WorkflowServiceError::PermissionDenied)
        ));
        let assignment = fixture.assignment();
        fixture.contexts.0.borrow_mut().backend = fixture
            .service
            .for_app(AppId::mint())
            .into_backend(1024)
            .unwrap();
        assert!(matches!(
            fixture.loader().load(&assignment, &executable),
            Err(WorkflowServiceError::PermissionDenied)
        ));
    }

    #[compio::test]
    async fn refuses_incompatible_schema_conflicting_peers_and_trusted_network() {
        let fixture = Fixture::new().await;
        let executable = fixture.executable("export default {};", None).await;
        let assignment = fixture.assignment();
        fixture
            .contexts
            .0
            .borrow_mut()
            .peers
            .push(Arc::new(Peer("db")));
        // The normal creator schema is accepted before the rejection controls.
        drop(fixture.loader().load(&assignment, &executable).unwrap());
        fixture.contexts.0.borrow_mut().schema = SchemaName::new("other_customer").unwrap();
        assert!(matches!(
            fixture.loader().load(&assignment, &executable),
            Err(WorkflowServiceError::InvalidRequest(_))
        ));
        fixture.contexts.0.borrow_mut().peers = vec![Arc::new(Peer("workflows"))];
        assert!(matches!(
            fixture.loader().load(&assignment, &executable),
            Err(WorkflowServiceError::InvalidRequest(_))
        ));
        fixture.contexts.0.borrow_mut().peers =
            vec![Arc::new(Peer("auth")), Arc::new(Peer("auth"))];
        assert!(matches!(
            fixture.loader().load(&assignment, &executable),
            Err(WorkflowServiceError::InvalidRequest(_))
        ));
        fixture.contexts.0.borrow_mut().peers.clear();
        fixture.contexts.0.borrow_mut().net_policy = NetPolicy::trusted(3, 1024);
        assert!(matches!(
            fixture.loader().load(&assignment, &executable),
            Err(WorkflowServiceError::InvalidRequest(_))
        ));
    }

    #[compio::test]
    async fn resolves_rotated_context_and_owns_independent_isolates() {
        let fixture = Fixture::new().await;
        let executable = fixture
            .executable(
                r"
            let calls = 0;
            export default { async fetch(_request, env) {
                let netAvailable;
                try { await import('node:net'); netAvailable = true; }
                catch { netAvailable = false; }
                return Response.json({calls:++calls, color:env.COLOR,
                    app:process.env.APP_ID, deployment:process.env.ZEROSHIP_DEPLOY_ID,
                    netAvailable});
            }};
        ",
                None,
            )
            .await;
        let assignment = fixture.assignment();
        fixture.contexts.0.borrow_mut().net_policy = NetPolicy::rules(Vec::new(), 3, 1024).unwrap();
        let (first, first_env) = fixture.loader().build(&assignment, &executable).unwrap();
        first.exit_isolate();
        fixture.contexts.0.borrow_mut().env = EnvSnapshot::vars_only(json!({"COLOR":"green"}));
        fixture.contexts.0.borrow_mut().net_policy = NetPolicy::Denied;
        let (second, second_env) = fixture.loader().build(&assignment, &executable).unwrap();
        second.exit_isolate();
        assert!(!Rc::ptr_eq(&first.state(), &second.state()));
        let old = fetch(first, &first_env).await;
        let new = fetch(second, &second_env).await;
        assert_eq!(old["calls"], 1);
        assert_eq!(new["calls"], 1);
        assert_eq!(old["color"], "blue");
        assert_eq!(new["color"], "green");
        assert_eq!(old["netAvailable"], true);
        assert_eq!(new["netAvailable"], false);
        assert_eq!(new["app"], fixture.contexts.0.borrow().app.as_str());
        assert_eq!(new["deployment"], assignment.invocation.deploy_hash);
    }

    async fn fetch(runtime: Runtime, env: &EnvSnapshot) -> Value {
        runtime.start_pump();
        runtime.enter_isolate();
        let outcome = runtime.call_fetch_handler(
            "GET",
            "http://localhost/",
            &[],
            "",
            env,
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
                    _ => panic!("expected buffered workflow fixture response"),
                }
            }
            _ => panic!("expected buffered workflow fixture response"),
        };
        runtime.shutdown().await;
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
        serde_json::from_slice(&body).unwrap()
    }
}
