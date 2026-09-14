use super::*;
use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
};
use zeroship_core::service_assertion::{ServiceSigningKey, ServiceTrustBundle};
use zeroship_data_orm::{
    binding::DbBinding, connection::ConnectionFactory, encryption::ProjectKeySource,
};
use zeroship_runtime::{transport::net_policy::NetPolicy, EnvSnapshot, RuntimeLimits};
use zeroship_storage::{LocalFs, StorageStore};
use zeroship_workflow::service::{schema, DeployRegistration, PolicySnapshot};

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
    pub fn replace(&self, resources: WorkflowResources) {
        *self.0.resources.borrow_mut() = resources;
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
    pub directory: tempfile::TempDir,
    pub scope: AssignedScope,
    pub worker: WorkerId,
    pub policies: Arc<HostPolicies>,
    pub policy: PolicyBinding,
    pub contexts: Rc<Contexts>,
    pub provider: Provider,
    pub deployments: deployment_fixture::Deployments,
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
        let schema = SchemaName::new(app.as_str()).unwrap();
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
                schema: schema.clone(),
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
        let storage = HostStorage {
            connection: ConnectionFactory::for_url(&format!(
                "sqlite:{}",
                directory.path().join("creator.sqlite").display()
            ))
            .unwrap(),
            keys: ProjectKeySource::unavailable(),
            binding: DbBinding::new(app.as_str(), "creator-fixture", schema),
            objects: StorageStore::from_backend(Arc::new(LocalFs::new(
                directory.path().join("objects"),
            ))),
        };
        let resources = WorkflowResources {
            storage,
            deployments: deployments.binding(&[&app]),
            signal_authority: Arc::new(
                SignalAuthority::new(
                    Arc::new(ServiceSigningKey::generate()),
                    ServiceTrustBundle::new(),
                )
                .unwrap(),
            ),
            contexts: contexts.clone(),
        };
        Self {
            directory,
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

    pub fn factory(&self) -> WorkflowCreatorFactory<Provider> {
        WorkflowCreatorFactory::new(
            self.provider.clone(),
            self.policies.clone(),
            &self.worker,
            TaskPayloadLimits::default(),
        )
        .unwrap()
    }

    pub fn assert_storage_unopened(&self) {
        assert!(
            self.directory.path().read_dir().unwrap().next().is_none(),
            "creator connection must not create files before identity validation"
        );
    }

    pub async fn provision(&self) {
        let store = self.provider.resources().storage.open().await.unwrap();
        schema::initialize_local(&store).await.unwrap();
    }

    pub async fn activate(&self, runtime: &CreatorRuntime, source: &str) -> DeploymentId {
        let deployment = self
            .deployments
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
            .unwrap();
        let deployment = DeploymentId::parse(&deployment.id).unwrap();
        let lease = self.grant(JobSpec {
            id: JobId::mint(),
            app_id: self.scope.app_id.clone(),
            operation: JobOperation::Activate {
                deployment_id: deployment.clone(),
                revision: 1.try_into().unwrap(),
            },
            available_at: 0.try_into().unwrap(),
        });
        assert_eq!(
            runtime.app.activate_job(&lease).await.unwrap().outcome,
            JobOutcome::Completed {}
        );
        self.deployments
            .assert_held(&self.scope.app_id, deployment.as_str())
            .await;
        deployment
    }

    pub fn grant(&self, job: JobSpec) -> Grant {
        Grant {
            delivery: Delivery {
                job,
                worker_id: self.worker.clone(),
                assignment_revision: self.scope.assignment_revision,
                attempt: 1.try_into().unwrap(),
                deadline: 0.try_into().unwrap(),
            },
            expires: Instant::now() + Duration::from_secs(30),
        }
    }

    pub async fn next(&self, app: &zeroship_workflow::service::AppWorkflows, run: &str) -> Grant {
        for job in app.pending_jobs(None, 16).await.unwrap() {
            if matches!(&job.operation, JobOperation::Advance { run_id, .. } if run_id.as_str() == run)
                && app.job_receipt(&job).await.unwrap().is_none()
            {
                return self.grant(job);
            }
        }
        panic!("run must publish its next unconsumed Advance");
    }
}

pub(super) struct Grant {
    pub delivery: Delivery,
    expires: Instant,
}
impl JobLease for Grant {
    fn delivery(&self) -> &Delivery {
        &self.delivery
    }
    fn remaining(&self) -> Option<Duration> {
        self.expires
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
    }
}

pub(super) async fn execute(
    runtime: &CreatorRuntime,
    task: &DeliveredTask,
) -> Result<zeroship_workflow::WorkflowExecution, WorkflowServiceError> {
    let guard = ExecutionGuard::new(task.remaining()?.min(Duration::from_secs(5))).unwrap();
    let mut execution = runtime.executor.start(task.assignment(), guard.budget())?;
    let result = execution.wait().await;
    execution.stop().await;
    drop(guard);
    result
}

pub(super) fn contains_file(directory: &Path) -> bool {
    directory.read_dir().unwrap().any(|entry| {
        let path = entry.unwrap().path();
        path.is_file() || (path.is_dir() && contains_file(&path))
    })
}
