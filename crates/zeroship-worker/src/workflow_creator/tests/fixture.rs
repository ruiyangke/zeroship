use super::*;
use std::collections::{BTreeMap, HashMap};
use compio::io::{AsyncRead, AsyncWriteExt};
use zeroship_core::service_assertion::{
    ServiceIssuer, ServiceTrustBundle, TransportAssertionVerifier,
};
use zeroship_core::service_peers::{
    service_issuer, InstanceSigningKey, ServiceAuth, WORKER_SERVICE_NAME,
};
use zeroship_runtime::{transport::net_policy::NetPolicy, EnvSnapshot, RuntimeLimits};
use zeroship_storage::{LocalFs, StorageStore};
use zeroship_core::workflow_coordination::Revision;
use serde_json::{json, Value};
use zeroship_core::{typed_id, workflow_jobs::DeploymentId};
use zeroship_workflow::service::{
    AppPolicy, DeployRegistration, IngressEpochs, PolicySnapshot, TaskAssignment,
};
use zeroship_core::workflow_coordination::WorkerId;
use zeroship_workflow_client::{Options as ClientOptions, WorkerCoordinator};

#[derive(Clone)]
pub(super) struct Provider(Rc<ProviderState>);

struct ProviderState {
    expected: AssignedScope,
    resources: RefCell<WorkflowResources>,
    calls: RefCell<Vec<AssignedScope>>,
    gate: RefCell<Option<Gate>>,
    dropped: Cell<bool>,
}

struct Gate {
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

struct Resolving(Rc<ProviderState>);
impl Drop for Resolving {
    fn drop(&mut self) {
        self.0.dropped.set(true);
    }
}

impl Provider {
    pub fn resources(&self) -> WorkflowResources {
        self.0.resources.borrow().clone()
    }
    pub fn calls(&self) -> Vec<AssignedScope> {
        self.0.calls.borrow().clone()
    }
    pub fn dropped(&self) -> bool {
        self.0.dropped.get()
    }

    pub fn gate(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered, observed) = oneshot::channel();
        let (release, blocked) = oneshot::channel();
        assert!(self
            .0
            .gate
            .borrow_mut()
            .replace(Gate {
                entered,
                release: blocked
            })
            .is_none());
        (observed, release)
    }
}

impl WorkflowResourceProvider for Provider {
    async fn resolve(
        &self,
        scope: &AssignedScope,
    ) -> Result<WorkflowResources, WorkflowServiceError> {
        self.0.calls.borrow_mut().push(scope.clone());
        if scope != &self.0.expected {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        let _resolving = Resolving(self.0.clone());
        let gate = self.0.gate.borrow_mut().take();
        if let Some(gate) = gate {
            gate.entered.send(()).unwrap();
            gate.release.await.unwrap();
        }
        Ok(self.resources())
    }
}

pub(super) struct Contexts {
    pub current: RefCell<WorkflowAppContext>,
    pub calls: Cell<usize>,
}

impl WorkflowContextProvider for Contexts {
    fn resolve(&self, _: &AppId) -> Result<WorkflowAppContext, WorkflowServiceError> {
        self.calls.set(self.calls.get() + 1);
        Ok(self.current.borrow().clone())
    }
}

pub(super) struct Fixture {
    /// Held so the object and artifact stores outlive the fixture.
    _directory: tempfile::TempDir,
    pub scope: AssignedScope,
    pub worker: WorkerId,
    pub policies: Arc<HostPolicies>,
    pub policy: PolicyBinding,
    pub contexts: Rc<Contexts>,
    pub provider: Provider,
    pub deployments: deployment_fixture::Deployments,
}

/// The placement's ingress establishment, as `AssignedPolicies` provides it:
/// each request installs the manager's next epoch into the policy binding.
pub(super) struct Epochs {
    binding: PolicyBinding,
    pub requested: RefCell<Vec<Option<Revision>>>,
    pub accepted: Cell<usize>,
}

impl IngressEpochs for Epochs {
    fn establish(
        &self,
        after: Option<Revision>,
    ) -> futures::future::LocalBoxFuture<'_, Result<(), WorkflowServiceError>> {
        self.requested.borrow_mut().push(after);
        Box::pin(async move {
            let next = after.map_or(1, |after| after.get() + 1);
            self.binding.begin_refresh()?.install(
                PolicySnapshot::configuration(1.try_into().unwrap(), AppPolicy::default())?
                    .with_ingress_epoch(Some(next.try_into().unwrap())),
            )
        })
    }

    fn accepted(&self) {
        self.accepted.set(self.accepted.get() + 1);
    }
}

pub(super) fn install(policies: &Arc<HostPolicies>, app: AppId) -> PolicyBinding {
    let binding = policies.bind(app).unwrap();
    binding
        .begin_refresh()
        .unwrap()
        .install(
            PolicySnapshot::configuration(1.try_into().unwrap(), AppPolicy::default()).unwrap(),
        )
        .unwrap();
    binding
}

impl Fixture {
    pub async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let app = AppId::mint();
        let policies = Arc::new(HostPolicies::default());
        let policy = install(&policies, app.clone());
        let deployments = deployment_fixture::Deployments::new().await;
        let scope = AssignedScope {
            app_id: app.clone(),
            assignment_revision: 3.try_into().unwrap(),
        };
        let contexts = Rc::new(Contexts {
            current: RefCell::new(WorkflowAppContext {
                app: app.clone(),
                env_vars: HashMap::new(),
                env: EnvSnapshot::new(BTreeMap::new(), BTreeMap::new(), Vec::new()),
                limits: RuntimeLimits {
                    cpu_limit: Some(Duration::from_secs(2)),
                    wall_timeout: Some(Duration::from_secs(5)),
                    heap_limit_bytes: Some(64 * 1024 * 1024),
                },
                net_policy: NetPolicy::Denied,
                peers: Vec::new(),
                meter: None,
            }),
            calls: Cell::new(0),
        });
        let resources = WorkflowResources {
            objects: PayloadObjects::open(StorageStore::from_backend(Arc::new(LocalFs::new(
                directory.path().join("objects"),
            ))))
            .unwrap(),
            artifacts: deployments.source.clone(),
            max_source_bytes: 1024 * 1024,
            contexts: contexts.clone(),
        };
        Self {
            _directory: directory,
            scope: scope.clone(),
            worker: WorkerId::mint(),
            policies,
            policy,
            contexts,
            provider: Provider(Rc::new(ProviderState {
                expected: scope,
                resources: RefCell::new(resources),
                calls: RefCell::new(Vec::new()),
                gate: RefCell::new(None),
                dropped: Cell::new(false),
            })),
            deployments,
        }
    }

    /// Ingress establishment for the fixture's placement and policy binding.
    pub fn ingress(&self) -> Rc<Epochs> {
        Rc::new(Epochs {
            binding: self.policy.clone(),
            requested: RefCell::new(Vec::new()),
            accepted: Cell::new(0),
        })
    }

    /// An enrolled client of the worker's own identity, against `origin`.
    fn client(&self, origin: &str) -> WorkerCoordinator {
        let role = service_issuer(WORKER_SERVICE_NAME).expect("worker issuer");
        let instance =
            ServiceIssuer::parse(&format!("{}/{}", role.as_str(), self.worker.as_str()))
                .expect("worker instance issuer");
        let keyring = InstanceSigningKey::generate()
            .into_keyring(instance, ServiceTrustBundle::new())
            .expect("worker instance keyring");
        let auth = Arc::new(ServiceAuth::new(
            keyring,
            Arc::new(TransportAssertionVerifier::new(ServiceTrustBundle::new())),
        ));
        WorkerCoordinator::new(
            origin,
            auth,
            ClientOptions {
                timeout: Duration::from_secs(5),
                ..ClientOptions::default()
            },
        )
        .expect("a client against a syntactically valid loopback origin")
    }

    /// A factory whose client reaches a loopback port nothing binds.
    ///
    /// For the refusals that happen BEFORE any request: a factory that reached a
    /// live peer could pass such a test by making a call and being answered,
    /// which is the opposite of what is being asserted.
    pub fn factory(&self) -> WorkflowCreatorFactory<Provider> {
        self.factory_through(self.client("http://127.0.0.1:1"))
    }

    pub fn factory_through(&self, client: WorkerCoordinator) -> WorkflowCreatorFactory<Provider> {
        WorkflowCreatorFactory::new(
            self.provider.clone(),
            self.policies.clone(),
            client,
            TaskPayloadLimits::default(),
            Duration::from_secs(5),
        )
        .unwrap()
    }

    /// Publish a deployment's sources into the artifact store this host loads
    /// from, and answer with the hash the service would pin a claim to.
    pub async fn publish(&self, source: &str) -> String {
        self.deployments
            .publish(
                &self.scope.app_id,
                &DeployRegistration {
                    id: DeploymentId::mint().as_str().to_owned(),
                    hash: String::new(),
                    workflows: ["Example".into()].into(),
                    schedules: Vec::new(),
                },
                &deployment_fixture::Sources::single(source),
            )
            .await
            .unwrap()
            .hash
    }

    /// A client against a peer that answers every request with `reply`, counting
    /// them.
    ///
    /// The count is the bound that makes an execution test say something: it is
    /// what separates "the loader crossed for the pin" from "the loader had it
    /// already", and it is asserted rather than assumed.
    pub async fn serving(&self, reply: Value) -> Peer {
        let listener = compio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = self.client(&format!("http://{}", listener.local_addr().unwrap()));
        let requests = Rc::new(Cell::new(0usize));
        let counted = requests.clone();
        let (stop, stopped) = oneshot::channel::<()>();
        let body = serde_json::to_vec(&reply).unwrap();
        let handle = compio::runtime::spawn(async move {
            let mut stopped = stopped;
            loop {
                let (mut stream, _) = match futures::future::select(
                    stopped,
                    Box::pin(listener.accept()),
                )
                .await
                {
                    Either::Left(_) => break,
                    Either::Right((accepted, remaining)) => {
                        stopped = remaining;
                        accepted.unwrap()
                    }
                };
                counted.set(counted.get() + 1);
                let mut seen = Vec::new();
                loop {
                    let (read, buffer) = stream.read(Vec::with_capacity(4096)).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    seen.extend_from_slice(&buffer[..read]);
                    if seen.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let mut response = format!(
                    "HTTP/1.1 200 Test\r\nContent-Type: application/json\r\n\
                     Connection: close\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                response.extend(body.clone());
                let _ = stream.write_all(response).await;
            }
        });
        Peer {
            client,
            requests,
            stop: Some(stop),
            handle: Some(handle),
        }
    }

    /// The assignment a claim would hand the executor, pinned to `deployment`.
    ///
    /// Hand-built, because the journal that builds one is the service's now. What
    /// it cannot therefore prove is that the service produces this shape -- that
    /// is bound where the service answers -- and what it does prove is what the
    /// assembled executor does with one.
    pub fn assignment(&self, deploy_hash: &str) -> TaskAssignment {
        serde_json::from_value(json!({
            "id": typed_id::generate("wft"),
            "token": "a".repeat(64),
            "generation": 1,
            "epoch": 1,
            "deadline": 10_000,
            "leaseMs": 5_000,
            "invocation": {
                "appId": self.scope.app_id.as_str(),
                "deployId": typed_id::generate("dep"),
                "deployHash": deploy_hash,
                "runId": typed_id::generate(typed_id::WORKFLOW_RUN_PREFIX),
                "generation": 1,
                "workflowName": "Example",
                "phase": "forward",
                "trigger": {
                    "input": {"value": "creator-owned-input"},
                    "startedAt": "2026-01-01T00:00:00Z",
                    "runId": typed_id::generate(typed_id::WORKFLOW_RUN_PREFIX),
                    "workflowName": "Example"
                },
                "journal": []
            }
        }))
        .unwrap()
    }
}

/// A scripted workflow service, and the count of what actually reached it.
pub(super) struct Peer {
    pub client: WorkerCoordinator,
    requests: Rc<Cell<usize>>,
    stop: Option<oneshot::Sender<()>>,
    handle: Option<compio::runtime::JoinHandle<()>>,
}

impl Peer {
    /// Stop serving and answer how many requests were made.
    pub async fn served(mut self) -> usize {
        drop(self.stop.take());
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
        self.requests.get()
    }
}

pub(super) async fn execute(
    runtime: &CreatorRuntime<()>,
    assignment: &TaskAssignment,
) -> Result<zeroship_workflow::WorkflowExecution, WorkflowServiceError> {
    let guard = ExecutionGuard::new(Duration::from_secs(5)).unwrap();
    let mut execution = runtime.executor.start(assignment, guard.budget())?;
    let result = execution.wait().await;
    execution.stop().await;
    drop(guard);
    result
}
