use super::*;
use crate::deployment_fixture::Deployments;
use zeroship_workflow::{
    operations::StartOptions,
    service::{DeployRegistration, RequestId},
};
use compio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use std::{collections::VecDeque, path::Path};
use zeroship_core::{
    service_assertion::{
        InMemoryReplayStore, ServiceAssertionVerifier, ServiceIssuer, ServiceSigningKey,
        ServiceTrustBundle, TransportAssertionVerifier,
    },
    service_identity::{endpoints, verify_service_call, ServiceEndpoint},
    service_peers::{ServiceAuth, ServiceKeyring},
    workflow_coordination::{Assignment, AUDIENCE},
    workflow_jobs::{JobSpec, Settlement, SettlementReceipt},
    workflow_policy::{AppPolicy, EstablishIngress, PolicyLease, PolicyLeaseRequest},
};
use zeroship_data_orm::{
    binding::DbBinding, connection::ConnectionFactory, encryption::ProjectKeySource,
};
use zeroship_workflow_client::{LeasedJob, Options};

pub(super) struct Fixture {
    pub policies: Arc<HostPolicies>,
    pub worker: WorkerId,
    pub factory: Factory,
    pub ready: ReadyApps,
    /// Every job submission the manager fixture accepted, as sent.
    pub submitted: RefCell<Vec<Value>>,
    auth: Arc<ServiceAuth>,
}

impl Fixture {
    /// Accept one settlement exactly as sent and acknowledge it.
    pub fn settlement(&self, settlement: &Settlement) -> Exchange {
        Exchange::new(
            endpoints::WORKFLOW_JOB_SETTLE,
            json!(settlement),
            json!(SettlementReceipt {
                job_id: settlement.delivery.job.id.clone(),
                app_id: settlement.delivery.job.app_id.clone(),
                attempt: settlement.delivery.attempt,
                outcome: settlement.outcome.clone(),
            }),
        )
    }

    /// Accept the release a host sends when it gives a placement up.
    ///
    /// The request carries a freshly minted request identity, so it is matched
    /// by endpoint and recorded for the caller to assert on.
    pub fn release(&self) -> Exchange {
        Exchange {
            recorded: true,
            ..Exchange::new(endpoints::WORKFLOW_RELEASE, Value::Null, Value::Null)
        }
    }

    /// Accept one job submission and acknowledge exactly the submitted job.
    pub fn submission(&self) -> Exchange {
        Exchange {
            echo: Some("job"),
            ..Exchange::new(endpoints::WORKFLOW_JOB_SUBMIT, Value::Null, Value::Null)
        }
    }

    pub fn registration(
        &self,
        state: zeroship_core::workflow_coordination::WorkerState,
        capacity: u32,
    ) -> Exchange {
        use zeroship_core::workflow_coordination::{RegisterWorker, RegisteredWorker};
        Exchange::new(
            endpoints::WORKFLOW_REGISTER,
            json!(RegisterWorker {
                capacity: capacity.try_into().unwrap(),
                state,
            }),
            json!(RegisteredWorker {
                worker_id: self.worker.clone(),
                capacity: capacity.try_into().unwrap(),
                state,
                expires_at: 0.try_into().unwrap(),
            }),
        )
    }

    pub fn new() -> Self {
        let worker = WorkerId::mint();
        let issuer = ServiceIssuer::parse(&format!(
            "spiffe://zeroship.ai/svc/worker/{}",
            worker.as_str()
        ))
        .unwrap();
        let auth = Arc::new(ServiceAuth::new(
            ServiceKeyring::from_parts(
                issuer,
                ServiceSigningKey::generate(),
                ServiceTrustBundle::new(),
            )
            .unwrap(),
            Arc::new(TransportAssertionVerifier::new(ServiceTrustBundle::new())),
        ));
        let policies = Arc::new(HostPolicies::default());
        Self {
            factory: Factory(Rc::new(FactoryState {
                policies: policies.clone(),
                directory: tempfile::tempdir().unwrap(),
                calls: RefCell::new(Vec::new()),
                gates: RefCell::new(BTreeMap::new()),
                finished: RefCell::new(BTreeMap::new()),
                alternative: RefCell::new(None),
                foreign_backend: Cell::new(false),
                deployments: RefCell::new(None),
                leftover: Cell::new(false),
            })),
            policies,
            worker,
            ready: ReadyApps::default(),
            submitted: RefCell::new(Vec::new()),
            auth,
        }
    }

    pub fn assignment(&self, scope: &AssignedScope) -> Assignment {
        Assignment {
            app_id: scope.app_id.clone(),
            worker_id: self.worker.clone(),
            revision: scope.assignment_revision,
            // Placement timestamps are deliberately not worker-clock authority.
            expires_at: 0.try_into().unwrap(),
        }
    }

    pub fn page(&self, after: Option<&AppId>, scopes: &[AssignedScope]) -> Exchange {
        Exchange::new(
            endpoints::WORKFLOW_ASSIGNMENTS,
            json!(ScopePage {
                after: after.cloned()
            }),
            json!(scopes
                .iter()
                .map(|scope| self.assignment(scope))
                .collect::<Vec<_>>()),
        )
    }

    /// A renewal of a placement whose binding already holds an ingress epoch.
    pub fn refresh(&self, scope: &AssignedScope) -> Vec<Exchange> {
        vec![
            self.renewal(scope),
            self.policy(scope, AppPolicy::default(), 1, 60_000),
        ]
    }

    /// The first preparation of a placement establishes an ingress epoch
    /// before the app can accept ingress.
    pub fn establish(&self, scope: &AssignedScope) -> Vec<Exchange> {
        vec![
            self.renewal(scope),
            self.lease(
                scope,
                Some(EstablishIngress { after: None }),
                AppPolicy::default(),
                1,
                60_000,
            ),
        ]
    }

    fn renewal(&self, scope: &AssignedScope) -> Exchange {
        Exchange::new(
            endpoints::WORKFLOW_RENEW,
            json!(scope),
            json!(self.assignment(scope)),
        )
    }

    pub fn policy(
        &self,
        scope: &AssignedScope,
        policy: AppPolicy,
        revision: i64,
        remaining: u64,
    ) -> Exchange {
        self.lease(scope, None, policy, revision, remaining)
    }

    fn lease(
        &self,
        scope: &AssignedScope,
        establish: Option<EstablishIngress>,
        policy: AppPolicy,
        revision: i64,
        remaining: u64,
    ) -> Exchange {
        Exchange::new(
            endpoints::WORKFLOW_POLICY_LEASE,
            json!(PolicyLeaseRequest {
                scope: scope.clone(),
                establish,
                ingress_used: false,
            }),
            json!(PolicyLease {
                app_id: scope.app_id.clone(),
                worker_id: self.worker.clone(),
                signing_key_id: self.auth.signing_identity().unwrap().1.key_id(),
                assignment_revision: scope.assignment_revision,
                policy_revision: revision.try_into().unwrap(),
                policy,
                ingress_epoch: Some(1.try_into().unwrap()),
                remaining_ms: remaining.try_into().unwrap(),
            }),
        )
    }

    pub fn scan(&self, scopes: &[AssignedScope]) -> Vec<Exchange> {
        let mut pages = vec![self.page(None, scopes)];
        if let Some(last) = scopes.last() {
            pages.push(self.page(Some(&last.app_id), &[]));
        }
        pages
    }

    pub fn consumer(&self, limit: usize) -> (JobConsumer<Probe>, Rc<Probe>) {
        let probe = Rc::new(Probe::default());
        let consumer = JobConsumer::new(
            probe.clone(),
            self.worker.clone(),
            ConsumerOptions {
                slots: limit,
                max_scopes: limit,
                idle_poll: Duration::from_secs(1),
                error_backoff: Duration::from_secs(1),
                delivery: DeliveryOptions {
                    execution_timeout: Duration::from_secs(5),
                    operation_timeout: Duration::from_secs(5),
                    retry_delay: Duration::from_secs(1),
                    reconciliation: ReconciliationOptions::default(),
                    collection: zeroship_workflow::service::collection::CollectionOptions::default(),
                    fanout: zeroship_workflow::service::fanout::FanoutOptions::default(),
                    propagation: zeroship_workflow::service::propagation::PropagationOptions::default(),
                },
            },
        )
        .unwrap();
        (consumer, probe)
    }

    pub fn bindings(
        &self,
        client: WorkerCoordinator,
        consumer: &JobConsumer<Probe>,
        limit: usize,
    ) -> AssignmentBindings<Factory> {
        AssignmentBindings::new(
            client,
            self.policies.clone(),
            consumer.bindings(),
            self.factory.clone(),
            self.ready.clone(),
            AssignmentOptions {
                max_scopes: limit,
                operation_timeout: Duration::from_secs(5),
            },
        )
        .unwrap()
    }
}

pub(super) struct Gate {
    arrived: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

impl Gate {
    pub fn new() -> (Self, oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (arrived, observed) = oneshot::channel();
        let (release, blocked) = oneshot::channel();
        (
            Self {
                arrived,
                release: blocked,
            },
            observed,
            release,
        )
    }

    async fn wait(self) {
        self.arrived.send(()).unwrap();
        self.release.await.unwrap();
    }
}

pub(super) struct Exchange {
    endpoint: ServiceEndpoint,
    request: Value,
    response: Value,
    status: u16,
    gate: Option<Gate>,
    /// Match any request to the endpoint and reply with this request field.
    echo: Option<&'static str>,
    /// Match any request to the endpoint, keep the declared response, and
    /// record the body. For requests that carry a minted identity.
    recorded: bool,
}

impl Exchange {
    pub fn conflict(mut self) -> Self {
        self.status = 409;
        self.response = json!({"code":"conflict"});
        self
    }

    pub fn denied(mut self) -> Self {
        self.status = 403;
        self.response = json!({"code":"denied"});
        self
    }

    fn new(endpoint: ServiceEndpoint, request: Value, response: Value) -> Self {
        Self {
            endpoint,
            request,
            response,
            status: 200,
            gate: None,
            echo: None,
            recorded: false,
        }
    }

    pub fn unavailable(mut self) -> Self {
        self.status = 503;
        self.response = json!({"code":"unavailable"});
        self
    }

    pub fn gated(mut self) -> (Self, oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (gate, observed, release) = Gate::new();
        self.gate = Some(gate);
        (self, observed, release)
    }
}

pub(super) fn peer<'a>(
    fixture: &'a Fixture,
    exchanges: Vec<Exchange>,
    test: impl AsyncFnOnce(WorkerCoordinator) + 'a,
) -> LocalBoxFuture<'a, ()> {
    Box::pin(async move {
        let listener = compio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = WorkerCoordinator::new(
            &format!("http://{}", listener.local_addr().unwrap()),
            fixture.auth.clone(),
            Options::default(),
        )
        .unwrap();
        let (issuer, key) = fixture.auth.signing_identity().unwrap();
        let mut trust = ServiceTrustBundle::new();
        trust.trust_signing_key(issuer, key.key_id(), key).unwrap();
        let verifier = ServiceAssertionVerifier::new(trust, Arc::new(InMemoryReplayStore::new()));
        let (done, completed) = oneshot::channel();
        let server = async {
            let mut exchanges = VecDeque::from(exchanges);
            let mut replies = Vec::new();
            let mut completed = completed;
            loop {
                let (mut stream, _) =
                    match futures::future::select(completed, listener.accept().boxed_local()).await
                    {
                        Either::Left((result, _)) => {
                            result.unwrap();
                            break;
                        }
                        Either::Right((socket, remaining)) => {
                            completed = remaining;
                            socket.unwrap()
                        }
                    };
                let observed = request(&mut stream).await;
                let index = exchanges
                    .iter()
                    .position(|exchange| {
                        exchange.endpoint.path_template() == observed.path
                            && (exchange.echo.is_some()
                                || exchange.recorded
                                || exchange.request == observed.body)
                    })
                    .unwrap_or_else(|| {
                        panic!(
                            "unexpected metadata request: {} {}",
                            observed.path, observed.body
                        )
                    });
                let mut exchange = exchanges.remove(index).unwrap();
                if let Some(field) = exchange.echo {
                    exchange.response = observed.body[field].clone();
                }
                if exchange.echo.is_some() || exchange.recorded {
                    fixture.submitted.borrow_mut().push(observed.body.clone());
                }
                verify_service_call(
                    &verifier,
                    Some(&observed.authorization),
                    AUDIENCE,
                    exchange.endpoint,
                )
                .await
                .unwrap();
                assert!(
                    verify_service_call(
                        &verifier,
                        Some(&observed.authorization),
                        AUDIENCE,
                        exchange.endpoint
                    )
                    .await
                    .is_err(),
                    "assertion replay must be rejected"
                );
                replies.push(compio::runtime::spawn(async move {
                if let Some(gate) = exchange.gate { gate.wait().await; }
                let body = serde_json::to_vec(&exchange.response).unwrap();
                let mut response = format!("HTTP/1.1 {} Test\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n", exchange.status, body.len()).into_bytes();
                response.extend(body);
                stream.write_all(response).await.0.unwrap();
                stream.flush().await.unwrap();
            }));
            }
            assert!(
                exchanges.is_empty(),
                "expected metadata exchanges were omitted"
            );
            for reply in replies {
                reply.await.unwrap();
            }
        };
        compio::time::timeout(Duration::from_secs(15), async {
            futures::join!(server, async {
                test(client).await;
                done.send(()).unwrap();
            });
        })
        .await
        .expect("assignment HTTP fixture hung");
    })
}

struct Request {
    path: String,
    authorization: String,
    body: Value,
}

async fn request(stream: &mut compio::net::TcpStream) -> Request {
    let mut bytes = Vec::new();
    loop {
        let compio::BufResult(read, buffer) = stream.read(vec![0; 1024]).await;
        let read = read.unwrap();
        assert_ne!(read, 0);
        bytes.extend_from_slice(&buffer[..read]);
        assert!(bytes.len() <= 16 * 1024);
        let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
            continue;
        };
        let header = std::str::from_utf8(&bytes[..end]).unwrap();
        let mut lines = header.lines();
        let mut start = lines.next().unwrap().split_whitespace();
        assert_eq!(start.next(), Some("POST"));
        let path = start.next().unwrap().to_owned();
        let mut length = None;
        let mut authorization = None;
        for line in lines {
            let (name, value) = line.split_once(':').unwrap();
            if name.eq_ignore_ascii_case("content-length") {
                assert!(length.is_none());
                length = Some(value.trim().parse::<usize>().unwrap());
            }
            if name.eq_ignore_ascii_case("authorization") {
                assert!(authorization.is_none());
                authorization = Some(value.trim().to_owned());
            }
        }
        let length = length.unwrap();
        if bytes.len() < end + 4 + length {
            continue;
        }
        assert_eq!(bytes.len(), end + 4 + length);
        return Request {
            path,
            authorization: authorization.unwrap(),
            body: serde_json::from_slice(&bytes[end + 4..]).unwrap(),
        };
    }
}

#[derive(Clone)]
pub(super) struct Factory(Rc<FactoryState>);

struct FactoryState {
    policies: Arc<HostPolicies>,
    directory: tempfile::TempDir,
    calls: RefCell<Vec<Rc<Opening>>>,
    gates: RefCell<BTreeMap<AppId, Gate>>,
    finished: RefCell<BTreeMap<AppId, oneshot::Sender<()>>>,
    alternative: RefCell<Option<AppId>>,
    foreign_backend: Cell<bool>,
    deployments: RefCell<Option<Rc<Deployments>>>,
    leftover: Cell<bool>,
}

pub(super) struct Opening {
    pub scope: AssignedScope,
    pub policy: PolicyBinding,
    pub dropped: Cell<bool>,
    pub runtime: RefCell<Option<CreatorRuntime>>,
}

struct OpeningGuard(Rc<Opening>);
impl Drop for OpeningGuard {
    fn drop(&mut self) {
        self.0.dropped.set(true);
    }
}

impl Factory {
    pub fn calls(&self) -> Vec<Rc<Opening>> {
        self.0.calls.borrow().clone()
    }

    pub fn gate(&self, app: &AppId) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (gate, observed, release) = Gate::new();
        assert!(self
            .0
            .gates
            .borrow_mut()
            .insert(app.clone(), gate)
            .is_none());
        (observed, release)
    }

    pub fn wrong_binding(&self, app: AppId) {
        *self.0.alternative.borrow_mut() = Some(app);
    }

    /// Return the exact app, but a request backend from another generation.
    pub fn foreign_backend(&self) {
        self.0.foreign_backend.set(true);
    }

    /// Open creators with an activated deployment of an `Example` workflow.
    /// With `leftover`, each opening also commits a start whose intent a
    /// previous process would have left unpublished.
    pub async fn deployed(&self, leftover: bool) {
        *self.0.deployments.borrow_mut() = Some(Rc::new(Deployments::new().await));
        self.0.leftover.set(leftover);
    }

    pub fn completed(&self, app: &AppId) -> oneshot::Receiver<()> {
        let (finished, completed) = oneshot::channel();
        assert!(self
            .0
            .finished
            .borrow_mut()
            .insert(app.clone(), finished)
            .is_none());
        completed
    }
}

impl CreatorFactory for Factory {
    async fn open(
        &self,
        scope: &AssignedScope,
        policy: &PolicyBinding,
        ingress: Rc<dyn IngressEpochs>,
    ) -> Result<CreatorRuntime, WorkflowServiceError> {
        let opening = Rc::new(Opening {
            scope: scope.clone(),
            policy: policy.clone(),
            dropped: Cell::new(false),
            runtime: RefCell::new(None),
        });
        self.0.calls.borrow_mut().push(opening.clone());
        let _guard = OpeningGuard(opening.clone());
        let gate = self.0.gates.borrow_mut().remove(&scope.app_id);
        if let Some(gate) = gate {
            gate.wait().await;
        }
        let alternative = self.0.alternative.borrow().clone();
        let (policies, policy) = if let Some(app) = alternative {
            let policies = Arc::new(HostPolicies::default());
            let binding = policies.bind(app)?;
            binding
                .begin_refresh()?
                .install(PolicySnapshot::configuration(
                    1.try_into().unwrap(),
                    AppPolicy::default(),
                )?)?;
            (policies, binding)
        } else {
            (self.0.policies.clone(), policy.clone())
        };
        let mut service = creator(self.0.directory.path(), &scope.app_id, policies).await;
        let deployments = self.0.deployments.borrow().clone();
        if let Some(deployments) = &deployments {
            service = service.with_deployments(deployments.binding(&[&scope.app_id]));
        }
        let app = service.register_app(&policy).await?;
        if let Some(deployments) = &deployments {
            deployments
                .activate(
                    &service,
                    &scope.app_id,
                    &DeployRegistration {
                        id: zeroship_core::typed_id::generate("dep"),
                        hash: String::new(),
                        workflows: ["Example".into()].into(),
                        schedules: Vec::new(),
                    },
                )
                .await?;
            if self.0.leftover.get() {
                app.start(&RequestId::mint(), "Example", StartOptions::default())
                    .await?;
            }
        }
        let objects = crate::PayloadObjects::open(zeroship_storage::StorageStore::from_backend(
            Arc::new(zeroship_storage::LocalFs::new(
                self.0.directory.path().join("objects"),
            )),
        ))?;
        let outputs: zeroship_workflow::SharedStepOutputs =
            Arc::new(crate::ObjectStepOutputs::new(objects.clone(), 1024)?);
        let backend = if self.0.foreign_backend.get() {
            let policies = Arc::new(HostPolicies::default());
            let binding = policies.bind(scope.app_id.clone())?;
            binding
                .begin_refresh()?
                .install(PolicySnapshot::configuration(
                    1.try_into().unwrap(),
                    AppPolicy::default(),
                )?)?;
            let foreign = creator(self.0.directory.path(), &scope.app_id, policies).await;
            foreign
                .register_app(&binding)
                .await?
                .into_backend(&foreign, outputs.clone(), Arc::new(objects.clone()))?
        } else {
            app.clone()
                .into_backend(&service, outputs.clone(), Arc::new(objects.clone()))?
        };
        let runtime = CreatorRuntime {
            app: app.with_ingress(ingress),
            executor: Rc::new(NoExecution),
            backend,
            objects,
        };
        *opening.runtime.borrow_mut() = Some(runtime.clone());
        if let Some(finished) = self.0.finished.borrow_mut().remove(&scope.app_id) {
            finished.send(()).unwrap();
        }
        Ok(runtime)
    }
}

async fn creator(directory: &Path, app: &AppId, policies: Arc<HostPolicies>) -> WorkflowService {
    let directory = directory.join(app.as_str());
    std::fs::create_dir_all(&directory).unwrap();
    let store = OrmStore::connect(
        DbBinding::platform(
            app.as_str(),
            "assignment-fixture",
            SchemaName::new(app.as_str()).unwrap(),
        ),
        &ConnectionFactory::for_platform_url(&format!(
            "sqlite:{}",
            directory.join("orm.sqlite").display()
        ))
        .unwrap(),
        ProjectKeySource::unavailable(),
    )
    .await
    .unwrap();
    schema::initialize_local(&store).await.unwrap();
    WorkflowService::open(Rc::new(store), policies)
        .await
        .unwrap()
}

struct NoExecution;
impl TaskExecutor for NoExecution {
    fn start(
        &self,
        _: &TaskAssignment,
        _: ExecutionBudget,
    ) -> Result<Box<dyn TaskExecution>, WorkflowServiceError> {
        panic!("placement reconciliation must not execute creator code")
    }
}

#[derive(Default)]
pub(super) struct Probe {
    pub claims: RefCell<Vec<AssignedScope>>,
}

impl JobTransport for Probe {
    type Lease = LeasedJob;
    async fn claim(
        &self,
        scope: &AssignedScope,
    ) -> Result<Option<Self::Lease>, WorkflowServiceError> {
        self.claims.borrow_mut().push(scope.clone());
        futures::future::pending().await
    }
    async fn submit(
        &self,
        _: &AssignedScope,
        _: &JobSpec,
    ) -> Result<JobSpec, WorkflowServiceError> {
        panic!("no delivered job")
    }
    async fn heartbeat(&self, _: &Self::Lease) -> Result<Self::Lease, WorkflowServiceError> {
        panic!("no delivered job")
    }
    async fn settle(&self, _: &Settlement) -> Result<SettlementReceipt, WorkflowServiceError> {
        panic!("no delivered job")
    }
}

pub(super) async fn claims(consumer: &mut JobConsumer<Probe>, probe: &Probe) -> Vec<AssignedScope> {
    probe.claims.borrow_mut().clear();
    let mut running = Box::pin(consumer.run_until(futures::future::pending()));
    assert!(futures::poll!(running.as_mut()).is_pending());
    drop(running);
    consumer.drain().await;
    std::mem::take(&mut *probe.claims.borrow_mut())
}
