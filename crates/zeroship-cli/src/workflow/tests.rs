use super::{manager::LocalTransport, *};
use serde_json::json;
use std::sync::Mutex;
use zeroship_core::workflow_jobs::{
    JobId, JobOperation, JobOutcome, Settlement, SettlementReceipt,
};
use zeroship_workflow::{
    backend::WorkflowBackend,
    operations::{RunState, RunStatus, SignalOptions, StartOptions},
    service::{
        runner::{delivery::JobTransport, ExecutionBudget, TaskExecution, TaskExecutor},
        AppPolicy, AppWorkflows, HostPolicies, PolicySnapshot, RequestId, TaskAssignment,
        WorkflowService,
    },
    WorkflowServiceError,
};
use zeroship_workflow_manager::{
    local::LocalPlatform,
    scheduling::{Options as SchedulingOptions, Scheduler, Selection},
    DeliveryGrant, Options as QueueOptions,
};

#[test]
fn local_configuration_rejects_unknown_and_invalid_limits() {
    assert!(toml::from_str::<LocalConfig>("unknown = true").is_err());
    assert!(toml::from_str::<LocalConfig>("bundle = 'built.zship'").is_err());
    assert!(toml::from_str::<LocalConfig>("journal = 'custom.sqlite'").is_err());
    assert!(toml::from_str::<LocalConfig>("objects = 'custom-objects'").is_err());
    assert!(toml::from_str::<LocalConfig>("[worker]\ntask_slots = 2").is_err());
    assert!(toml::from_str::<LocalConfig>("[manager]\ndatabase = 'queue.sqlite'").is_err());
    let valid: LocalConfig =
        toml::from_str("[consumer]\nslots = 2\n[manager]\nlease_ms = 5000").unwrap();
    let valid = valid.validate().unwrap();
    assert_eq!(valid.consumer.slots, 2);
    assert_eq!(valid.manager.lease_ms, 5000);
    assert_eq!(valid.consumer_options().slots, 2);
    for invalid in [
        LocalConfig {
            max_source_bytes: 0,
            ..LocalConfig::default()
        },
        LocalConfig {
            consumer: ConsumerConfig {
                slots: 0,
                ..ConsumerConfig::default()
            },
            ..LocalConfig::default()
        },
        LocalConfig {
            manager: ManagerConfig {
                lease_ms: 0,
                ..ManagerConfig::default()
            },
            ..LocalConfig::default()
        },
        LocalConfig {
            manager: ManagerConfig {
                placement_ttl_ms: 2,
                ..ManagerConfig::default()
            },
            ..LocalConfig::default()
        },
    ] {
        assert!(invalid.validate().is_err());
    }
}

fn publish(root: &Path, version: &str, cooldown: &str) -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest.parent().unwrap().parent().unwrap();
    let output = std::process::Command::new("pnpm")
        .current_dir(workspace.join("packages/vite-plugin"))
        .args(["exec", "tsx"])
        .arg(manifest.join("tests/fixtures/app-bundle.ts"))
        .arg(root)
        .arg(version)
        .arg(cooldown)
        .output()
        .expect("run the workflow deploy compiler with pnpm");
    assert!(
        output.status.success(),
        "workflow fixture build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    root.join("app.zship")
}

fn test_storage(root: &Path, app: &AppId) -> HostStorage {
    HostStorage {
        connection: zeroship_data_orm::connection::ConnectionFactory::for_url(&format!(
            "sqlite:{}",
            root.join(".zeroship/dev.sqlite").display()
        ))
        .unwrap(),
        keys: zeroship_data_orm::encryption::ProjectKeySource::unavailable(),
        binding: zeroship_data_orm::binding::DbBinding::new(
            app.as_str(),
            "test-deployment",
            zeroship_core::schema_name::SchemaName::new(
                &zeroship_core::app_derivation::schema_name(app),
            )
            .unwrap(),
        ),
        objects: zeroship_storage::StorageStore::from_backend(Arc::new(
            zeroship_storage::LocalFs::new(root.join(".zeroship/storage")),
        )),
    }
}

/// Push reconciliation past every wait here, so only immediate publication
/// after ingress and settlement can make committed intents deliverable.
fn without_reconciliation() -> LocalConfig {
    LocalConfig {
        manager: ManagerConfig {
            recovery_interval_ms: 3_600_000,
            ..ManagerConfig::default()
        },
        ..LocalConfig::default()
    }
}

fn start(root: &Path, app: &AppId, config: LocalConfig, bundle: Option<&Path>) -> LocalHost {
    start_with(root, app, config, bundle, vec![], Production)
}

fn start_with<C: Composition>(
    root: &Path,
    app: &AppId,
    config: LocalConfig,
    bundle: Option<&Path>,
    peers: Vec<Arc<dyn NativePlugin>>,
    composition: C,
) -> LocalHost {
    LocalHost::start_with(
        root,
        app.clone(),
        config,
        bundle,
        test_storage(root, app),
        [("APP_ID".into(), "untrusted-variable".into())].into(),
        peers,
        RuntimeLimits::default(),
        composition,
    )
    .unwrap()
}

/// The metered `env.db` plugin `zeroship serve` gives every isolate. Building a
/// workflow isolate stamps its meter on that thread for all later ORM calls.
fn metered_database(
    root: &Path,
    app: &AppId,
) -> (Vec<Arc<dyn NativePlugin>>, Arc<zeroship_metering::Meter>) {
    let meter = Arc::new(zeroship_metering::Meter::new());
    let service =
        zeroship_data_v8::service::DbService::new(zeroship_data_v8::service::DbServiceConfig {
            connection: zeroship_data_orm::connection::ConnectionFactory::for_url(&format!(
                "sqlite:{}",
                root.join(".zeroship/dev.sqlite").display()
            ))
            .unwrap(),
            project_keys: crate::project_keys::load(&root.join(".zeroship/private"), app).unwrap(),
            cdc_relay: None,
            meter: Some(meter.clone()),
        })
        .unwrap();
    (vec![service.plugin()], meter)
}

/// A second creator handle on the app database, outside the host.
async fn client(root: &Path, app: &AppId) -> AppWorkflows {
    retry(async || {
        let policies = Arc::new(HostPolicies::default());
        let binding = policies.bind(app.clone())?;
        binding
            .begin_refresh()?
            .install(PolicySnapshot::configuration(
                1.try_into().unwrap(),
                AppPolicy::default(),
            )?)?;
        let service =
            WorkflowService::open(Rc::new(test_storage(root, app).open().await?), policies).await?;
        service.register_app(&binding).await
    })
    .await
}

/// Manager metadata read through a second binding to the platform file.
async fn selection(root: &Path, app: &AppId) -> Option<Selection> {
    Box::pin(retry(async || {
        let platform = LocalPlatform::open(&root.join(".zeroship/platform/metadata.sqlite"))
            .await
            .map_err(manager::catalog_error)?;
        let queue = platform
            .queue(QueueOptions::default())
            .await
            .map_err(manager::manager_error)?;
        Scheduler::new(queue, SchedulingOptions::default())
            .map_err(manager::manager_error)?
            .selection(app)
            .await
            .map_err(manager::manager_error)
    }))
    .await
}

async fn until<T>(mut observe: impl AsyncFnMut() -> Option<T>) -> T {
    compio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(value) = observe().await {
                return value;
            }
            compio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the local host converged")
}

/// The host and these test handles use separate database connections, so
/// one side's commit can briefly refuse the other's writer reservation.
async fn retry<T>(mut operation: impl AsyncFnMut() -> Result<T, WorkflowServiceError>) -> T {
    until(async || match operation().await {
        Ok(value) => Some(value),
        Err(WorkflowServiceError::Unavailable(_)) => None,
        Err(error) => panic!("{error:?}"),
    })
    .await
}

/// Start through the host's own ingress. The business key makes a retried
/// start join the run an earlier attempt may have accepted.
async fn start_run(backend: &AppBackend, key: &str) -> String {
    retry(async || {
        backend
            .start(
                "Example".into(),
                StartOptions {
                    key: Some(key.into()),
                    ..StartOptions::default()
                },
            )
            .await
    })
    .await
    .id
}

/// Wait until the creator outbox holds no unconfirmed publication intent.
async fn published(creator: &AppWorkflows) {
    until(async || match creator.pending_jobs(None, 16).await {
        Ok(pending) => pending.is_empty().then_some(()),
        Err(WorkflowServiceError::Unavailable(_)) => None,
        Err(error) => panic!("{error:?}"),
    })
    .await;
}

async fn state(backend: &AppBackend, run: &str, expected: RunState) -> RunStatus {
    until(async || {
        let status = match backend.status(run.into()).await {
            Ok(status) => status,
            Err(WorkflowServiceError::Unavailable(_)) => return None,
            Err(error) => panic!("{error:?}"),
        };
        assert_ne!(status.state, RunState::Failed, "{status:?}");
        (status.state == expected).then_some(status)
    })
    .await
}

async fn signal(backend: &AppBackend, run: &str) {
    retry(async || {
        backend
            .signal(
                run.into(),
                SignalOptions {
                    signal_type: "resume".into(),
                    payload: json!(null),
                },
            )
            .await
    })
    .await;
}

#[compio::test]
async fn delivered_activation_selects_the_archive_and_runs_complete_through_the_manager() {
    let root = tempfile::tempdir().unwrap();
    let bundle = publish(root.path(), "original", "10ms");
    let app = zeroship_core::app_id::local_dev_app_id();
    // Workflow isolates get the app's metered database, as under `zeroship serve`.
    let (peers, meter) = metered_database(root.path(), &app);
    let host = start_with(
        root.path(),
        &app,
        without_reconciliation(),
        Some(bundle.as_path()),
        peers,
        Production,
    );
    assert!(root
        .path()
        .join(".zeroship/platform/metadata.sqlite")
        .exists());
    assert!(!root
        .path()
        .join(".zeroship/deployments/index.sqlite")
        .exists());
    assert!(!root.path().join(".zeroship/workflows.sqlite").exists());
    assert!(!root.path().join(".zeroship/app-id").exists());

    // Startup waits for the creator receipt of the manager's Activation job;
    // the manager then records its settlement as dispatch readiness.
    let selected = until(async || {
        selection(root.path(), &app)
            .await
            .and_then(|selection| selection.activation)
            .filter(|activation| activation.ready)
    })
    .await;
    assert_eq!(selected.revision.get(), 1);
    assert!(matches!(
        &selected.job.operation,
        JobOperation::Activate { deployment_id, .. } if deployment_id == &selected.deployment_id
    ));
    let creator = client(root.path(), &app).await;
    let receipt = retry(async || creator.job_receipt(&selected.job).await)
        .await
        .expect("the creator applied the delivered activation");
    assert_eq!(receipt.outcome, JobOutcome::Completed {});

    let run = start_run(&host.backend, "delivered").await;
    state(&host.backend, &run, RunState::Waiting).await;
    // Ingress and delivered successors, including the signal wait's timeout,
    // are published after their commits without waiting for reconciliation.
    published(&creator).await;
    signal(&host.backend, &run).await;
    assert_eq!(
        state(&host.backend, &run, RunState::Completed).await.output,
        Some(json!("original:original:lazy"))
    );
    // The workflow thread ran metered app isolates, yet delivery kept working:
    // platform metadata never runs under the app's thread-local meter.
    assert!(meter.tracked_app_count() > 0);
    drop(host);
}

#[compio::test]
async fn sleeping_run_resumes_from_queue_metadata_after_restart() {
    let root = tempfile::tempdir().unwrap();
    let bundle = publish(root.path(), "original", "3s");
    let app = AppId::mint();
    let host = start(
        root.path(),
        &app,
        without_reconciliation(),
        Some(bundle.as_path()),
    );
    let run = start_run(&host.backend, "sleeping").await;
    state(&host.backend, &run, RunState::Sleeping).await;
    // The wake-up is manager metadata now; the journal has nothing to publish.
    published(&client(root.path(), &app).await).await;
    drop(host);

    let host = start(
        root.path(),
        &app,
        without_reconciliation(),
        Some(bundle.as_path()),
    );
    assert_eq!(
        selection(root.path(), &app).await.unwrap().revision.get(),
        1,
        "restarting the same archive keeps its activation"
    );
    state(&host.backend, &run, RunState::Waiting).await;
    signal(&host.backend, &run).await;
    assert_eq!(
        state(&host.backend, &run, RunState::Completed).await.output,
        Some(json!("original:original:lazy"))
    );
}

#[compio::test]
async fn republished_bundle_activates_while_existing_runs_keep_their_pins() {
    let root = tempfile::tempdir().unwrap();
    let bundle = publish(root.path(), "original", "10ms");
    let app = zeroship_core::app_id::local_dev_app_id();
    let host = start(
        root.path(),
        &app,
        LocalConfig::default(),
        Some(bundle.as_path()),
    );
    let first = selection(root.path(), &app).await.unwrap();
    let old = start_run(&host.backend, "original").await;
    state(&host.backend, &old, RunState::Waiting).await;
    drop(host);

    // Hot reload republishes the archive and restarts the host.
    publish(root.path(), "replacement", "10ms");
    let host = start(
        root.path(),
        &app,
        LocalConfig::default(),
        Some(bundle.as_path()),
    );
    let second = selection(root.path(), &app).await.unwrap();
    assert_eq!(second.revision.get(), first.revision.get() + 1);
    assert_ne!(
        second.activation.as_ref().unwrap().deployment_id,
        first.activation.unwrap().deployment_id
    );
    let new = start_run(&host.backend, "replacement").await;
    state(&host.backend, &new, RunState::Waiting).await;
    drop(host);

    // Without an archive, the host keeps the selection and retained code.
    std::fs::remove_dir_all(root.path().join("src")).unwrap();
    std::fs::remove_file(&bundle).unwrap();
    let host = start(root.path(), &app, LocalConfig::default(), None);
    assert_eq!(
        selection(root.path(), &app).await.unwrap().revision,
        second.revision
    );
    for (run, expected) in [
        (&old, "original:original:lazy"),
        (&new, "replacement:replacement:lazy"),
    ] {
        signal(&host.backend, run).await;
        assert_eq!(
            state(&host.backend, run, RunState::Completed).await.output,
            Some(json!(expected))
        );
    }
}

#[compio::test]
async fn reconciliation_publishes_work_committed_outside_the_host() {
    let root = tempfile::tempdir().unwrap();
    let bundle = publish(root.path(), "original", "10ms");
    let app = AppId::mint();
    let config = LocalConfig {
        manager: ManagerConfig {
            driver_interval_ms: 50,
            recovery_interval_ms: 200,
            ..ManagerConfig::default()
        },
        ..LocalConfig::default()
    };
    let host = start(root.path(), &app, config, Some(bundle.as_path()));
    // This commit gives the host no hint; only the manager's periodic
    // reconciliation job can publish its intent.
    let creator = client(root.path(), &app).await;
    let request = RequestId::mint();
    let run = retry(async || {
        creator
            .start(&request, "Example", StartOptions::default())
            .await
    })
    .await;
    state(&host.backend, &run.id, RunState::Waiting).await;
    signal(&host.backend, &run.id).await;
    state(&host.backend, &run.id, RunState::Completed).await;
}

#[derive(Default)]
struct Deliveries {
    /// The first Advance job and every settlement attempt the host made for it.
    target: Mutex<Option<JobId>>,
    settlements: Mutex<Vec<(u32, bool)>>,
    /// The replayed journal length of every started execution.
    starts: Mutex<Vec<usize>>,
}

/// Loses every acknowledgement of the first Advance job's first attempt before
/// it reaches the manager, as a network failure after the creator commit would.
struct LostAcknowledgement(Arc<Deliveries>);

impl Composition for LostAcknowledgement {
    type Transport = LossyTransport;

    fn transport(&self, transport: LocalTransport) -> LossyTransport {
        LossyTransport {
            inner: transport,
            observed: self.0.clone(),
        }
    }

    fn executor(&self, executor: Rc<dyn TaskExecutor>) -> Rc<dyn TaskExecutor> {
        Rc::new(CountingExecutor {
            inner: executor,
            observed: self.0.clone(),
        })
    }
}

struct LossyTransport {
    inner: LocalTransport,
    observed: Arc<Deliveries>,
}

impl JobTransport for LossyTransport {
    type Lease = DeliveryGrant;

    async fn claim(
        &self,
        scope: &zeroship_core::workflow_coordination::AssignedScope,
    ) -> Result<Option<DeliveryGrant>, WorkflowServiceError> {
        self.inner.claim(scope).await
    }

    async fn submit(
        &self,
        scope: &zeroship_core::workflow_coordination::AssignedScope,
        job: &zeroship_core::workflow_jobs::JobSpec,
    ) -> Result<zeroship_core::workflow_jobs::JobSpec, WorkflowServiceError> {
        self.inner.submit(scope, job).await
    }

    async fn heartbeat(
        &self,
        lease: &DeliveryGrant,
    ) -> Result<DeliveryGrant, WorkflowServiceError> {
        self.inner.heartbeat(lease).await
    }

    async fn settle(
        &self,
        settlement: &Settlement,
    ) -> Result<SettlementReceipt, WorkflowServiceError> {
        let job = &settlement.delivery.job;
        let attempt = u32::try_from(settlement.delivery.attempt.get()).unwrap();
        let targeted = {
            let mut target = self.observed.target.lock().unwrap();
            if target.is_none() && matches!(job.operation, JobOperation::Advance { .. }) {
                *target = Some(job.id.clone());
            }
            target.as_ref() == Some(&job.id)
        };
        if targeted {
            let lost = attempt == 1;
            self.observed
                .settlements
                .lock()
                .unwrap()
                .push((attempt, !lost));
            if lost {
                return Err(WorkflowServiceError::Unavailable(
                    "acknowledgement lost before the manager".into(),
                ));
            }
        }
        self.inner.settle(settlement).await
    }
}

struct CountingExecutor {
    inner: Rc<dyn TaskExecutor>,
    observed: Arc<Deliveries>,
}

impl TaskExecutor for CountingExecutor {
    fn start(
        &self,
        assignment: &TaskAssignment,
        budget: ExecutionBudget,
    ) -> Result<Box<dyn TaskExecution>, WorkflowServiceError> {
        self.observed
            .starts
            .lock()
            .unwrap()
            .push(assignment.invocation.journal.len());
        self.inner.start(assignment, budget)
    }
}

#[compio::test]
async fn lost_acknowledgement_replays_the_committed_turn_without_executing_again() {
    let root = tempfile::tempdir().unwrap();
    let bundle = publish(root.path(), "original", "10ms");
    let app = AppId::mint();
    let observed = Arc::new(Deliveries::default());
    let config = LocalConfig {
        consumer: ConsumerConfig {
            operation_timeout_ms: 1_000,
            error_backoff_ms: 100,
            ..ConsumerConfig::default()
        },
        manager: ManagerConfig {
            lease_ms: 3_000,
            ..ManagerConfig::default()
        },
        ..LocalConfig::default()
    };
    let host = start_with(
        root.path(),
        &app,
        config,
        Some(bundle.as_path()),
        vec![],
        LostAcknowledgement(observed.clone()),
    );
    let run = start_run(&host.backend, "lost-acknowledgement").await;
    state(&host.backend, &run, RunState::Waiting).await;
    signal(&host.backend, &run).await;
    assert_eq!(
        state(&host.backend, &run, RunState::Completed).await.output,
        Some(json!("original:original:lazy"))
    );
    // The lost first attempt keeps its manager lease until expiry, which can
    // outlast the run's later turns. Its redelivery then settles the receipt.
    let settlements = until(async || {
        let settlements = observed.settlements.lock().unwrap().clone();
        settlements
            .iter()
            .any(|(attempt, delivered)| *attempt > 1 && *delivered)
            .then_some(settlements)
    })
    .await;
    assert!(
        settlements
            .iter()
            .any(|(attempt, delivered)| *attempt == 1 && !delivered),
        "{settlements:?}"
    );
    // Each execution starts from a longer journal. A redelivered first turn
    // replays its receipt instead of executing the empty journal again.
    let starts = observed.starts.lock().unwrap().clone();
    assert_eq!(
        starts.iter().filter(|replayed| **replayed == 0).count(),
        1,
        "{starts:?}"
    );
    assert!(
        starts.windows(2).all(|pair| pair[0] < pair[1]),
        "{starts:?}"
    );
    drop(host);
}
