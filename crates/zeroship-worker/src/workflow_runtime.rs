//! Exclusive workflow isolates built from retained code and trusted app context.

use std::{collections::HashMap, rc::Rc, sync::Arc};
use zeroship_bundle::LoadedWorker;
use zeroship_core::app_id::AppId;
use zeroship_metering::Meter;
use zeroship_runtime::{
    transport::net_policy::NetPolicy, EnvSnapshot, ModuleEntry, NativePlugin, Runtime,
    RuntimeLimits,
};
use zeroship_workflow::{service::TaskAssignment, WorkflowServiceError};
use zeroship_workflow_runner::remote::RemoteBackend;
use zeroship_workflow_v8::{LoadedWorkflow, WorkflowBinding, WorkflowRuntimeLoader};

/// A snapshot already authorized by the worker's app metadata provider.
///
/// `env_vars` is visible to app JavaScript. Database credentials and schema
/// metadata belong in the native host; they must never be placed in that map.
/// Peers contain the ordinary native primitives, excluding `workflows`.
#[derive(Clone)]
pub struct WorkflowAppContext {
    pub app: AppId,
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
///
/// The owning host initializes V8 and drives the executor on its compio thread.
/// Runtime metadata may refresh, but the workflow backend retains the original
/// placement for every isolate built by this loader.
pub struct WorkerWorkflowRuntimeLoader {
    contexts: Rc<dyn WorkflowContextProvider>,
    backend: RemoteBackend,
}

impl std::fmt::Debug for WorkerWorkflowRuntimeLoader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkerWorkflowRuntimeLoader")
            .field("app", self.backend.app_id())
            .finish_non_exhaustive()
    }
}

impl WorkerWorkflowRuntimeLoader {
    /// Retain workflow authority separately from refreshable app metadata.
    #[must_use]
    pub fn new(contexts: Rc<dyn WorkflowContextProvider>, backend: RemoteBackend) -> Self {
        Self { contexts, backend }
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
        if self.backend.app_id() != &app {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        let mut context = self.contexts.resolve(&app)?;
        if context.app != app {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        validate_context(&context)?;

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
            .push(Arc::new(WorkflowBinding::remote(self.backend.clone())));
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
        let descriptor = crate::executable::descriptor_document(executable);
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

pub(crate) fn validate_context(context: &WorkflowAppContext) -> Result<(), WorkflowServiceError> {
    if matches!(context.net_policy, NetPolicy::Trusted { .. }) {
        return Err(invalid(
            "creator workflow cannot use trusted network policy",
        ));
    }
    let mut namespaces = std::collections::HashSet::new();
    for peer in &context.peers {
        let namespace = peer.namespace();
        if namespace == "workflows" || !namespaces.insert(namespace) {
            return Err(invalid("conflicting workflow runtime native peers"));
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

    /// The creator seam's step-output reader, over a store beneath `directory`.
    fn payload_objects(directory: &std::path::Path) -> PayloadObjects {
        PayloadObjects::open(zeroship_storage::StorageStore::from_backend(Arc::new(
            zeroship_storage::LocalFs::new(directory.join("objects")),
        )))
        .unwrap()
    }
    use compio::io::AsyncWriteExt;
    use std::{
        cell::RefCell,
        collections::BTreeMap,
        time::Duration,
    };
    use zeroship_bundle::{
        BlobStore, LocalDiskBlobStore, Manifest, RuntimeDescriptorEntry, WorkerCode,
    };
    use zeroship_core::{
        service_assertion::{
            ServiceIssuer, ServiceSigningKey, ServiceTrustBundle, TransportAssertionVerifier,
        },
        service_peers::{ServiceAuth, ServiceKeyring},
        typed_id,
        workflow_coordination::{AssignedScope, WorkerId},
    };
    use zeroship_runtime::{
        plugin::NativeRegistrar, CancelFlag, FetchOutcome, RequestCtx, SettledFetch,
    };
    use zeroship_workflow_client::{Options as ClientOptions, WorkerCoordinator};
    use zeroship_workflow_runner::PayloadObjects;

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

    fn auth() -> Arc<ServiceAuth> {
        let issuer = ServiceIssuer::parse(&format!(
            "spiffe://zeroship.ai/svc/worker/{}",
            WorkerId::mint().as_str()
        ))
        .unwrap();
        Arc::new(ServiceAuth::new(
            ServiceKeyring::from_parts(
                issuer,
                ServiceSigningKey::generate(),
                ServiceTrustBundle::new(),
            )
            .unwrap(),
            Arc::new(TransportAssertionVerifier::new(ServiceTrustBundle::new())),
        ))
    }

    /// A client whose origin nothing binds, for the cases that make no call.
    ///
    /// Deliberately unreachable: a test asserting that identity is refused before
    /// any request would otherwise be able to pass by making one.
    fn unreachable() -> WorkerCoordinator {
        WorkerCoordinator::new(
            "http://127.0.0.1:1",
            auth(),
            ClientOptions {
                timeout: Duration::from_millis(250),
                ..ClientOptions::default()
            },
        )
        .unwrap()
    }

    fn scope(app: &AppId, revision: i64) -> AssignedScope {
        AssignedScope {
            app_id: app.clone(),
            assignment_revision: revision.try_into().unwrap(),
        }
    }

    fn backend(directory: &std::path::Path, scope: AssignedScope) -> RemoteBackend {
        RemoteBackend::new(unreachable(), scope, payload_objects(directory), 64 * 1024).unwrap()
    }

    /// A workflow service answering each creator call by the PLACEMENT its body
    /// names, and recording which placements called.
    struct Service {
        url: String,
        seen: Rc<RefCell<Vec<i64>>>,
        stop: Option<futures::channel::oneshot::Sender<()>>,
        handle: Option<compio::runtime::JoinHandle<()>>,
    }

    impl Service {
        async fn new(replies: BTreeMap<i64, (u16, Value)>) -> Self {
            let listener = compio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let seen = Rc::new(RefCell::new(Vec::new()));
            let recorded = seen.clone();
            let (stop, stopped) = futures::channel::oneshot::channel::<()>();
            let handle = compio::runtime::spawn(async move {
                let mut stopped = stopped;
                loop {
                    let (mut stream, _) =
                        match futures::future::select(stopped, Box::pin(listener.accept())).await {
                            futures::future::Either::Left(_) => break,
                            futures::future::Either::Right((accepted, remaining)) => {
                                stopped = remaining;
                                accepted.unwrap()
                            }
                        };
                    let body = read_request(&mut stream).await;
                    let revision = body["scope"]["assignmentRevision"]
                        .as_i64()
                        .expect("a creator call names its placement");
                    recorded.borrow_mut().push(revision);
                    let (status, reply) = replies
                        .get(&revision)
                        .expect("an unscripted placement reached the service");
                    let encoded = serde_json::to_vec(reply).unwrap();
                    let mut response = format!(
                        "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\n\
                         Connection: close\r\nContent-Length: {}\r\n\r\n",
                        encoded.len()
                    )
                    .into_bytes();
                    response.extend(encoded);
                    let _ = stream.write_all(response).await;
                }
            });
            Self {
                url,
                seen,
                stop: Some(stop),
                handle: Some(handle),
            }
        }

        fn backend(&self, directory: &std::path::Path, scope: AssignedScope) -> RemoteBackend {
            RemoteBackend::new(
                WorkerCoordinator::new(&self.url, auth(), ClientOptions::default()).unwrap(),
                scope,
                payload_objects(directory),
                64 * 1024,
            )
            .unwrap()
        }

        /// The placements every call this service served named.
        async fn calls(mut self) -> Vec<i64> {
            drop(self.stop.take());
            if let Some(handle) = self.handle.take() {
                let _ = handle.await;
            }
            self.seen.borrow().clone()
        }
    }

    async fn read_request(stream: &mut compio::net::TcpStream) -> Value {
        let mut seen = Vec::new();
        loop {
            let (read, buffer) =
                compio::io::AsyncRead::read(stream, Vec::with_capacity(8192)).await.unwrap();
            assert!(read > 0, "a creator call must send a body");
            seen.extend_from_slice(&buffer[..read]);
            let Some(at) = seen.windows(4).position(|window| window == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8_lossy(&seen[..at]).to_lowercase();
            let length = headers
                .split("content-length:")
                .nth(1)
                .and_then(|rest| rest.split("\r\n").next()?.trim().parse::<usize>().ok())
                .expect("a creator call declares its body length");
            if seen.len() >= at + 4 + length {
                return serde_json::from_slice(&seen[at + 4..at + 4 + length]).unwrap();
            }
        }
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        app: AppId,
        contexts: Rc<Contexts>,
        backend: RemoteBackend,
        blobs: LocalDiskBlobStore,
    }

    impl Fixture {
        async fn new() -> Self {
            zeroship_runtime::init_v8();
            let directory = tempfile::tempdir().unwrap();
            let app = AppId::mint();
            let context = WorkflowAppContext {
                app: app.clone(),
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
                backend: backend(directory.path(), scope(&app, 1)),
                app,
                contexts: Rc::new(Contexts(RefCell::new(context))),
                blobs: LocalDiskBlobStore::new(directory.path().join("bundles")).unwrap(),
                _directory: directory,
            }
        }

        fn loader(&self) -> WorkerWorkflowRuntimeLoader {
            WorkerWorkflowRuntimeLoader::new(self.contexts.clone(), self.backend.clone())
        }

        fn assignment(&self) -> TaskAssignment {
            // TYPED at the boundary now: the remote backend parses a run id into
            // `RunId` before a call crosses, so a fixture id of the wrong prefix
            // is refused there rather than handed to SQL as text.
            let run = typed_id::generate(typed_id::WORKFLOW_RUN_PREFIX);
            serde_json::from_value(json!({
                "id":typed_id::generate("wft"), "token":"a".repeat(64),
                "generation":1, "epoch":1, "deadline":10000, "leaseMs":1000,
                "invocation": {
                    "appId":self.contexts.0.borrow().app.as_str(),
                    "deployId":typed_id::generate("dep"), "deployHash":"b".repeat(64),
                    "runId":run, "generation":1, "workflowName":"Example", "phase":"forward",
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
                manifest.runtime_descriptor = vec![RuntimeDescriptorEntry {
                    label: "main".into(),
                    database_id: zeroship_core::DatabaseId::mint(),
                    primary: true,
                    hash,
                }];
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
            // The slot carries the descriptor DOCUMENT of the pinned deploy:
            // one entry per database it declares, each holding that database's
            // own schema. The pinned schema is what a replay must see, and the
            // entry it arrives in is what names the database it belongs to -
            // so both are asserted, against the manifest the executable was
            // loaded from rather than against a second copy of the literal.
            assert_eq!(
                crate::executable::primary_schema_json(state.runtime_descriptor.as_deref()),
                descriptor,
                "the pinned deploy's own schema reaches the isolate"
            );
            let document =
                serde_json::from_str::<Value>(state.runtime_descriptor.as_ref().unwrap()).unwrap();
            let declared = executable.databases();
            assert_eq!(declared.len(), 1, "the fixture declares one database");
            assert_eq!(
                document["databases"],
                json!([{
                    "label": declared[0].label,
                    "database_id": declared[0].database_id.as_str(),
                    "primary": true,
                    "schema": descriptor,
                }]),
                "the document entry names the database the manifest declared"
            );
            let meter = state.meter.as_ref().unwrap();
            assert_eq!(meter.app_id(), &fixture.contexts.0.borrow().app);
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
        let error = runtime.initialize(&env).await.unwrap_err();
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
        let mut foreign_context = fixture.contexts.0.borrow().clone();
        foreign_context.app = AppId::mint();
        let dishonest = WorkerWorkflowRuntimeLoader::new(
            Rc::new(ForeignContext(foreign_context)),
            fixture.backend.clone(),
        );
        assert!(matches!(
            dishonest.load(&fixture.assignment(), &executable),
            Err(WorkflowServiceError::PermissionDenied)
        ));
        // A backend whose placement names another app. The identity the loader
        // can check is the one the backend carries, and it carries a placement.
        let assignment = fixture.assignment();
        let foreign_backend = backend(fixture._directory.path(), scope(&AppId::mint(), 1));
        assert!(matches!(
            WorkerWorkflowRuntimeLoader::new(fixture.contexts.clone(), foreign_backend)
                .load(&assignment, &executable),
            Err(WorkflowServiceError::PermissionDenied)
        ));
    }

    #[compio::test]
    async fn refuses_conflicting_peers_and_trusted_network() {
        let fixture = Fixture::new().await;
        let executable = fixture.executable("export default {};", None).await;
        let assignment = fixture.assignment();
        fixture
            .contexts
            .0
            .borrow_mut()
            .peers
            .push(Arc::new(Peer("db")));
        // A db peer on its own is accepted, so the refusals below are about the
        // namespaces rather than about having one at all.
        drop(fixture.loader().load(&assignment, &executable).unwrap());
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

    /// A metadata refresh cannot swap the backend an isolate was built with.
    ///
    /// The loader keeps the backend it was constructed from, so two isolates built
    /// across a refresh reach two DIFFERENT placements. That is asserted by what
    /// each call carried rather than by the error it got back, and the two
    /// placements are answered differently so the codes distinguish them:
    ///
    /// - the placement that still holds gets `NotFound` for an absent run, which
    ///   is the ordinary creator answer;
    /// - the superseded one gets `Conflict`, because the placement moved under a
    ///   caller that was legitimately placed and a retry is the honest
    ///   instruction. `PermissionDenied` would say "not allowed", which is both
    ///   false and non-retryable. The reason-to-code pairing itself is bound where
    ///   a real coordinator answers, in `zeroship-workflow-server`'s
    ///   `a_moved_assignment_revision_conflicts_rather_than_denying`.
    #[compio::test]
    async fn metadata_refresh_cannot_replace_a_retired_workflow_backend() {
        let fixture = Fixture::new().await;
        let service = Service::new(BTreeMap::from([
            (
                1,
                (
                    409,
                    json!({"code":"conflict","message":"workflow placement is no longer current"}),
                ),
            ),
            (
                2,
                (404, json!({"code":"not_found","message":"workflow run not found"})),
            ),
        ]))
        .await;
        let retired_backend = service.backend(fixture._directory.path(), scope(&fixture.app, 1));
        let fresh_backend = service.backend(fixture._directory.path(), scope(&fixture.app, 2));
        let assignment = fixture.assignment();
        fixture
            .contexts
            .0
            .borrow_mut()
            .env_vars
            .insert("MISSING_RUN".into(), assignment.invocation.run_id.clone());
        let executable = fixture
            .executable(
                r"
            export default { async fetch(_request, env) {
                try {
                    await env.workflows.Example.get(process.env.MISSING_RUN)
                        .signal({type:'wake', payload:null});
                    return Response.json({color:env.COLOR, code:'unexpected_success'});
                } catch (error) {
                    return Response.json({color:env.COLOR, code:error.code});
                }
            }};
        ",
                None,
            )
            .await;
        let loader =
            WorkerWorkflowRuntimeLoader::new(fixture.contexts.clone(), retired_backend.clone());
        let (before, before_env) = loader.build(&assignment, &executable).unwrap();
        before.exit_isolate();
        assert_eq!(
            fetch(before, &before_env).await,
            json!({"color":"blue", "code":"workflow_conflict"})
        );

        fixture.contexts.0.borrow_mut().env = EnvSnapshot::vars_only(json!({"COLOR":"green"}));
        // The SAME loader after the refresh: its backend is the one it was built
        // with, so this isolate still reaches the superseded placement.
        let (retired, retired_env) = loader.build(&assignment, &executable).unwrap();
        retired.exit_isolate();
        let fresh_loader =
            WorkerWorkflowRuntimeLoader::new(fixture.contexts.clone(), fresh_backend);
        let (fresh, fresh_env) = fresh_loader.build(&assignment, &executable).unwrap();
        fresh.exit_isolate();
        assert_eq!(
            fetch(retired, &retired_env).await,
            json!({"color":"green", "code":"workflow_conflict"})
        );
        assert_eq!(
            fetch(fresh, &fresh_env).await,
            json!({"color":"green", "code":"workflow_not_found"})
        );
        // What each call actually named. Without this the codes above could agree
        // for a reason other than which backend was reached.
        assert_eq!(service.calls().await, vec![1, 1, 2]);
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
