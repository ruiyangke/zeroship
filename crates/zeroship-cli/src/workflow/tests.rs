use super::{manager::LocalTransport, *};
use serde_json::json;
use std::sync::Mutex;
use zeroship_core::workflow_jobs::{
    JobId, JobOperation, JobOutcome, JobSpec, Settlement, SettlementReceipt,
};
use zeroship_workflow::{
    backend::WorkflowBackend,
    operations::{RunState, RunStatus, SignalOptions, StartOptions},
    service::{
        AppPolicy, AppWorkflows, HostPolicies, PolicySnapshot, RequestId, TaskAssignment,
        WorkflowService,
    },
    WorkflowServiceError,
};
use zeroship_workflow_runner::{
    delivery::{Claimed, Completed, JobTransport, Renewed},
    ExecutionBudget, TaskExecution, TaskExecutor,
};
use zeroship_workflow_manager::{
    local::LocalPlatform,
    recovery::{DutyKind, Options as RecoveryOptions, Recovery, Responsibility, ScopeState},
    scheduling::{Options as SchedulingOptions, Scheduler, Selection},
    Claimant, DeliveryGrant, Options as QueueOptions,
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
        // A grace inside the manager transaction budget could release a hold
        // before the dependency confirmed with it commits.
        LocalConfig {
            manager: ManagerConfig {
                hold_grace_ms: 5_000,
                ..ManagerConfig::default()
            },
            ..LocalConfig::default()
        },
    ] {
        assert!(invalid.validate().is_err());
    }
    let valid: LocalConfig = toml::from_str("[manager]\nhold_grace_ms = 5001").unwrap();
    assert_eq!(
        valid.validate().unwrap().manager_options().hold_grace,
        Duration::from_millis(5_001)
    );
}

/// `[payloads]` configures the budget this host reads staged payloads through,
/// so a value below the platform ceiling would start a host that cannot
/// materialize what admission may let an app stage.
#[test]
fn local_configuration_refuses_a_payload_budget_below_the_platform_ceiling() {
    let ceiling = zeroship_core::workflow_policy::MAX_PAYLOAD_BYTES_CEILING;
    let at_ceiling = format!(
        "[payloads]\nmax_inline_bytes = 1024\nmax_payload_bytes = {ceiling}\n\
         max_result_bytes = {ceiling}"
    );
    let config: LocalConfig = toml::from_str(&at_ceiling).unwrap();
    assert_eq!(
        config.validate().unwrap().payloads.max_payload_bytes,
        ceiling
    );
    let below = format!(
        "[payloads]\nmax_inline_bytes = 1024\nmax_payload_bytes = {}\n\
         max_result_bytes = {ceiling}",
        ceiling - 1
    );
    let config: LocalConfig = toml::from_str(&below).unwrap();
    // The control: the refused budget is internally consistent, so only the
    // ceiling comparison can account for the refusal.
    config.payloads.validate().unwrap();
    assert!(config.validate().is_err());
}

/// Where the workspace install puts the TypeScript runner the bundle compiler
/// runs under.
fn typescript_runner(workspace: &Path) -> PathBuf {
    workspace.join("packages/vite-plugin/node_modules/.bin/tsx")
}

/// The bundle compiler runs under the installed TypeScript runner, addressed by
/// path, and inside the project it is building.
///
/// Reaching the runner through a package manager instead would make this
/// fixture write outside that project: a package manager relinks the workspace
/// it runs in, `workspace` derives from `CARGO_MANIFEST_DIR`, and a git
/// worktree's `packages/*/node_modules` are links into the primary checkout, so
/// the relink lands in the primary checkout and repoints it at the worktree.
/// The runner is a plain child process and rewrites nothing.
fn bundle_compiler(root: &Path, version: &str, cooldown: &str) -> std::process::Command {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest.parent().unwrap().parent().unwrap();
    let runner = typescript_runner(workspace);
    assert!(
        runner.is_file(),
        "the workflow fixture needs the TypeScript runner installed at {}; \
         install the workspace dependencies from {} and retry",
        runner.display(),
        workspace.display()
    );
    let mut command = std::process::Command::new(runner);
    command
        .current_dir(root)
        .arg(manifest.join("tests/fixtures/app-bundle.ts"))
        .arg(root)
        .arg(version)
        .arg(cooldown);
    command
}

fn publish(root: &Path, version: &str, cooldown: &str) -> PathBuf {
    let output = bundle_compiler(root, version, cooldown)
        .output()
        .expect("run the workflow deploy compiler");
    assert!(
        output.status.success(),
        "workflow fixture build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    root.join("app.zship")
}

/// A package manager would arrive as a bare program name resolved from `PATH`,
/// and would run in the checkout that installed it rather than in the project
/// being built. Both halves are asserted: the program is the installed runner
/// addressed absolutely, and the working directory is the fixture's own
/// temporary project.
#[test]
fn the_bundle_compiler_runs_an_installed_runner_inside_the_project() {
    let root = tempfile::tempdir().unwrap();
    let command = bundle_compiler(root.path(), "original", "10ms");
    let program = Path::new(command.get_program());
    assert!(
        program.is_absolute(),
        "a bare program name resolves from PATH: {}",
        program.display()
    );
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    assert_eq!(program, typescript_runner(workspace));
    assert_eq!(command.get_current_dir(), Some(root.path()));
}

fn test_storage(root: &Path, app: &AppId) -> HostStorage {
    HostStorage {
        connection: zeroship_data_orm::connection::ConnectionFactory::for_app_url(&format!(
            "sqlite:{}",
            root.join(".zeroship/dev.sqlite").display()
        ))
        .unwrap(),
        keys: zeroship_data_orm::encryption::ProjectKeySource::unavailable(),
        binding: zeroship_data_orm::binding::DbBinding::platform(
            app.as_str(),
            "test-deployment",
            zeroship_core::schema_name::SchemaName::new(
                &zeroship_core::app_derivation::schema_name(app),
            )
            .unwrap(),
        ),
    }
}

fn test_objects(root: &Path) -> zeroship_storage::StorageStore {
    zeroship_storage::StorageStore::from_backend(Arc::new(zeroship_storage::LocalFs::new(
        root.join(".zeroship/storage"),
    )))
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
        test_objects(root),
        [("APP_ID".into(), "untrusted-variable".into())].into(),
        peers,
        RuntimeLimits::default(),
        composition,
    )
    .unwrap()
}

/// The database a project file declares, standing in for the one
/// `resolve_dev_database` reads off `databases.<label>.id`. `zeroship serve`
/// binds what the file says, so a harness that minted its own would exercise
/// an identity the host has no way to produce.
fn declared_database() -> zeroship_core::DatabaseId {
    zeroship_core::DatabaseId::parse("dbs_03evr3oqx1200cfkwyailh8l8")
        .expect("a canonical database id")
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
            connection: zeroship_data_orm::connection::ConnectionFactory::for_app_url(&format!(
                "sqlite:{}",
                root.join(".zeroship/dev.sqlite").display()
            ))
            .unwrap(),
            project_keys: crate::project_keys::load(&root.join(".zeroship/private"), app).unwrap(),
            app_bindings: crate::dev_binding::load(
                &root.join(".zeroship/private"),
                app,
                Some(&declared_database()),
            )
            .unwrap(),
            cdc_relay: None,
            meter: Some(meter.clone()),
        })
        .unwrap();
    (vec![service.plugin()], meter)
}

/// A second creator handle on the app database, outside the host. It holds
/// the manager's current ingress epoch, as another host would once it had
/// established that epoch.
async fn client(root: &Path, app: &AppId) -> AppWorkflows {
    let epoch = responsibility(root, app)
        .await
        .map(|current| current.ingress_epoch);
    retry(async || {
        let policies = Arc::new(HostPolicies::default());
        let binding = policies.bind(app.clone())?;
        binding.begin_refresh()?.install(
            PolicySnapshot::configuration(1.try_into().unwrap(), AppPolicy::default())?
                .with_ingress_epoch(epoch),
        )?;
        let service =
            WorkflowService::open(Rc::new(test_storage(root, app).open().await?), policies).await?;
        service.register_app(&binding).await
    })
    .await
}

/// The app's recovery responsibility, read through a second binding to the
/// platform file.
async fn responsibility(root: &Path, app: &AppId) -> Option<Responsibility> {
    Box::pin(retry(async || {
        let platform = LocalPlatform::open(&root.join(".zeroship/platform/metadata.sqlite"))
            .await
            .map_err(manager::catalog_error)?;
        let queue = platform
            .queue(QueueOptions::default())
            .await
            .map_err(manager::manager_error)?;
        Recovery::new(queue, RecoveryOptions::default())
            .map_err(manager::manager_error)?
            .responsibility(app)
            .await
            .map_err(manager::manager_error)
    }))
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
                serde_json::Value::Null,
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

/// What a run returned, read back from the payload object it was staged into.
///
/// A run's result reaches the journal as a descriptor, so `status` names the
/// object and the bytes come from the store. Both halves are held here: the
/// descriptor `status` hands out is a reference, and it is the one whose object
/// holds these bytes.
async fn returned_value(backend: &AppBackend, run: &str) -> serde_json::Value {
    let described = state(backend, run, RunState::Completed)
        .await
        .output
        .expect("a run that returned a value names the payload holding it");
    assert_eq!(described["kind"], "ref", "{described}");
    let bytes = backend.read_output(run.into()).await.unwrap();
    assert_eq!(described["size"], json!(bytes.len()), "{described}");
    serde_json::from_slice(&bytes).unwrap()
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
        returned_value(&host.backend, &run).await,
        json!("original:original:lazy")
    );
    // The workflow thread ran metered app isolates, yet delivery kept working:
    // platform metadata never runs under the app's thread-local meter.
    //
    // This used to assert `meter.tracked_app_count() > 0`. That counted the
    // JOURNAL's own writes, which reached the meter only because usage was
    // attributed per process rather than per binding. Attribution is now
    // per binding (`zeroship-data-v8`'s `sink_for`), so workflow bookkeeping is
    // correctly NOT charged to the app, and this fixture's workflow - a single
    // `step.run` with no `env.db` call - legitimately meters nothing.
    //
    // What that assertion is NOT evidence of, and never was: that a metered
    // isolate still delivers. Binding it would need the fixture app to make a
    // real `env.db` call, which needs a table and a migration in the bundle.
    // Left unbound deliberately rather than kept as a line that passes for a
    // reason unrelated to its comment.
    assert_eq!(meter.tracked_app_count(), 0, "the journal is not app usage");
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
        returned_value(&host.backend, &run).await,
        json!("original:original:lazy")
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
        assert_eq!(returned_value(&host.backend, run).await, json!(expected));
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

/// Startup establishes the app's ingress epoch before the host accepts work.
/// Once the app idles, the manager's closing lane delivers Close and retires
/// its responsibility; the next acceptance is fenced, the host establishes a
/// newer epoch and retries it, and delivered work resumes. A restarted host
/// reopens a retired scope before accepting requests.
#[compio::test]
async fn idle_responsibility_retires_and_the_next_acceptance_reopens_it() {
    let root = tempfile::tempdir().unwrap();
    let bundle = publish(root.path(), "original", "10ms");
    let app = AppId::mint();
    let config = LocalConfig {
        manager: ManagerConfig {
            driver_interval_ms: 50,
            recovery_interval_ms: 3_600_000,
            idle_close_ms: 200,
            closing_timeout_ms: 10_000,
            closing_backoff_ms: 100,
            closing_backoff_max_ms: 400,
            ..ManagerConfig::default()
        },
        ..LocalConfig::default()
    };
    let host = start(root.path(), &app, config.clone(), Some(bundle.as_path()));
    let retired = until(async || {
        responsibility(root.path(), &app)
            .await
            .filter(|current| current.state == ScopeState::Retired)
    })
    .await;
    assert_eq!(
        retired.ingress_epoch.get(),
        1,
        "startup established epoch one"
    );
    let run = start_run(&host.backend, "reopened").await;
    let reopened = responsibility(root.path(), &app).await.unwrap();
    assert!(
        reopened.ingress_epoch.get() > retired.ingress_epoch.get(),
        "the fenced start established a newer epoch: {reopened:?}"
    );
    state(&host.backend, &run, RunState::Waiting).await;
    signal(&host.backend, &run).await;
    assert_eq!(
        returned_value(&host.backend, &run).await,
        json!("original:original:lazy")
    );
    let idle = until(async || {
        responsibility(root.path(), &app)
            .await
            .filter(|current| current.state == ScopeState::Retired)
    })
    .await;
    drop(host);

    let _host = start(root.path(), &app, config, Some(bundle.as_path()));
    let restarted = responsibility(root.path(), &app).await.unwrap();
    assert!(
        matches!(
            restarted.state,
            ScopeState::Open | ScopeState::Closing | ScopeState::Retired
        ) && restarted.ingress_epoch.get() > idle.ingress_epoch.get(),
        "startup reopened the retired scope at a newer epoch: {restarted:?}"
    );
}

/// The app's recovery operations through a second binding to the platform
/// file, as a manager replica would run them.
async fn recovery(root: &Path) -> Result<Recovery, WorkflowServiceError> {
    let platform = LocalPlatform::open(&root.join(".zeroship/platform/metadata.sqlite"))
        .await
        .map_err(manager::catalog_error)?;
    let queue = platform
        .queue(QueueOptions::default())
        .await
        .map_err(manager::manager_error)?;
    Recovery::new(queue, RecoveryOptions::default()).map_err(manager::manager_error)
}

/// A host restarted while a closing attempt is in flight establishes a newer
/// epoch before it accepts requests, which cancels the attempt. The stale
/// Close fences only the cancelled epoch, so the host keeps accepting work at
/// its own without establishing again.
#[compio::test]
async fn restart_cancels_an_in_flight_closing_attempt() {
    let root = tempfile::tempdir().unwrap();
    let bundle = publish(root.path(), "original", "10ms");
    let app = AppId::mint();
    let host = start(
        root.path(),
        &app,
        without_reconciliation(),
        Some(bundle.as_path()),
    );
    // Startup's maintenance duties are delivered and settled while the host
    // runs; a pending one would refuse every closing attempt once it stops.
    for kind in [DutyKind::Reconcile, DutyKind::Collect] {
        Box::pin(until(async || {
            let pending = retry(async || {
                recovery(root.path())
                    .await?
                    .dispatch(&app, kind)
                    .await
                    .map_err(manager::manager_error)
            })
            .await;
            pending.is_none().then_some(())
        }))
        .await;
    }
    drop(host);
    let close = Box::pin(until(async || {
        retry(async || {
            recovery(root.path())
                .await?
                .begin_close(&app)
                .await
                .map_err(manager::manager_error)
        })
        .await
    }))
    .await;
    let closing = responsibility(root.path(), &app).await.unwrap();
    assert_eq!(closing.state, ScopeState::Closing);
    assert_eq!(closing.ingress_epoch.get(), 1);

    let host = start(
        root.path(),
        &app,
        without_reconciliation(),
        Some(bundle.as_path()),
    );
    let reopened = responsibility(root.path(), &app).await.unwrap();
    assert_eq!(reopened.state, ScopeState::Open, "{reopened:?}");
    assert_eq!(reopened.ingress_epoch.get(), 2);
    assert!(reopened.close_job.is_none());
    // The restarted host delivers the stale Close, which fences epoch one.
    let creator = client(root.path(), &app).await;
    until(async || match creator.job_receipt(&close).await {
        Ok(receipt) => receipt,
        Err(WorkflowServiceError::Unavailable(_)) => None,
        Err(error) => panic!("{error:?}"),
    })
    .await;
    let run = start_run(&host.backend, "after-cancelled-closing").await;
    state(&host.backend, &run, RunState::Waiting).await;
    let kept = responsibility(root.path(), &app).await.unwrap();
    assert_eq!(kept.state, ScopeState::Open, "{kept:?}");
    assert_eq!(kept.ingress_epoch.get(), 2);
}

/// The app's responsibility once its recorded activity stops advancing, so a
/// later advance can only come from ingress the test drove.
async fn quiescent(root: &Path, app: &AppId) -> Responsibility {
    let mut previous = responsibility(root, app).await.unwrap();
    until(async || {
        compio::time::sleep(Duration::from_millis(500)).await;
        let current = responsibility(root, app).await.unwrap();
        let settled = current.active_at == previous.active_at;
        previous = current;
        settled.then(|| previous.clone())
    })
    .await
}

/// Ingress that commits no publication, such as a signal no wait expects,
/// still counts as activity: the host reports it with its next renewal, which
/// restarts the idle window of an app in use.
#[compio::test]
async fn reported_ingress_counts_as_activity() {
    let root = tempfile::tempdir().unwrap();
    let bundle = publish(root.path(), "original", "10ms");
    let app = AppId::mint();
    let config = LocalConfig {
        manager: ManagerConfig {
            driver_interval_ms: 50,
            placement_ttl_ms: 300,
            recovery_interval_ms: 3_600_000,
            idle_close_ms: 3_600_000,
            ..ManagerConfig::default()
        },
        ..LocalConfig::default()
    };
    let host = start(root.path(), &app, config, Some(bundle.as_path()));
    let run = start_run(&host.backend, "in-use").await;
    state(&host.backend, &run, RunState::Waiting).await;
    let quiet = quiescent(root.path(), &app).await;
    retry(async || {
        host.backend
            .signal(
                run.clone(),
                SignalOptions {
                    signal_type: "nudge".into(),
                    payload: json!(null),
                },
            )
            .await
    })
    .await;
    let reported = until(async || {
        responsibility(root.path(), &app)
            .await
            .filter(|current| current.active_at > quiet.active_at)
    })
    .await;
    assert_eq!(
        (
            reported.state,
            reported.ingress_epoch,
            reported.close_attempts
        ),
        (ScopeState::Open, quiet.ingress_epoch, 0),
        "{reported:?}"
    );
    // The unexpected signal moved no wait, so nothing was published for it.
    assert_eq!(
        host.backend.status(run.clone()).await.unwrap().state,
        RunState::Waiting
    );
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
    type Lease = DeliveryGrant;
    /// This host holds the journal, so an attempt is scoped here rather than
    /// server-side.
    type Journal = AppWorkflows;
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
    ) -> Result<Option<Claimed<DeliveryGrant>>, WorkflowServiceError> {
        self.inner.claim(journal, scope).await
    }

    async fn heartbeat(
        &self,
        journal: &zeroship_workflow::service::AppWorkflows,
        lease: &DeliveryGrant,
        task: &zeroship_workflow::service::delivery::DeliveredTask,
    ) -> Result<Renewed<DeliveryGrant>, WorkflowServiceError> {
        self.inner.heartbeat(journal, lease, task).await
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

    /// The journal commits first, then the acknowledgement this fixture loses.
    /// That is the partial commit the merge preserves rather than removes: a
    /// receipt the journal holds whose delivery the queue never settled.
    async fn complete(
        &self,
        journal: &zeroship_workflow::service::AppWorkflows,
        lease: &DeliveryGrant,
        task: &zeroship_workflow::service::delivery::DeliveredTask,
        execution: zeroship_workflow::WorkflowExecution,
        confirmed: Vec<zeroship_workflow::service::delivery::PayloadConfirmation>,
    ) -> Result<Completed, WorkflowServiceError> {
        let receipt = journal
            .complete_reported_job(task, lease, execution, &confirmed)
            .await?;
        let settlement = receipt.settlement(lease)?;
        Ok(Completed {
            settlement: JobTransport::settle(self, &settlement).await?,
            receipt,
        })
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
        returned_value(&host.backend, &run).await,
        json!("original:original:lazy")
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

/// Register a queue scope and submit a manager-origin sweep row into it,
/// through a second binding to the platform file. The host publishes a sweep
/// only on its own recovery deadline, so this is how a test puts a claimable
/// maintenance row in front of it.
async fn submit_sweep(root: &Path, job: &JobSpec) {
    Box::pin(retry(async || {
        let platform = LocalPlatform::open(&root.join(".zeroship/platform/metadata.sqlite"))
            .await
            .map_err(manager::catalog_error)?;
        let queue = platform
            .queue(QueueOptions::default())
            .await
            .map_err(manager::manager_error)?;
        queue
            .register_scope(&job.app_id)
            .await
            .map_err(manager::manager_error)?;
        queue.submit(job).await.map_err(manager::manager_error)
    }))
    .await;
}

/// The apps whose queue holds a maintenance row no claimant has settled, read
/// through a second binding to the platform file. A settled row never returns
/// to this list; a row leased by a claimant that never settles it does, as soon
/// as its lease lapses.
async fn sweepable(root: &Path) -> Vec<AppId> {
    Box::pin(retry(async || {
        let platform = LocalPlatform::open(&root.join(".zeroship/platform/metadata.sqlite"))
            .await
            .map_err(manager::catalog_error)?;
        platform
            .queue(QueueOptions::default())
            .await
            .map_err(manager::manager_error)?
            .claimable_apps(Claimant::Maintenance, None, None, false, 16)
            .await
            .map(|(apps, _)| apps)
            .map_err(manager::manager_error)
    }))
    .await
}

/// The local host owns the journal its sweeps maintain, so it claims them as
/// `Claimant::Maintenance` itself. Nothing else can: the consumer beside that
/// lane claims as `Claimant::Placed`, which admits only `advance`.
///
/// No archive is published, so this host holds no creator code at all. The
/// journal receipt therefore cannot have come from an executing task, and the
/// consumer never sees the row.
///
/// Both halves of one exchange are asserted, because either alone is satisfied
/// by a broken lane. The journal receipt says a claimant ran the operation and
/// committed its outcome; a lane that claimed and never settled would still
/// leave that receipt behind. So the queue row is read afterwards - but only
/// once the host has stopped and a lease short enough to have lapsed by then
/// has: a settled row is no longer a candidate for any maintenance claimant,
/// while a row a live lane keeps re-leasing and never settles is one the moment
/// that lane goes away. Reading it while the host still runs measures whichever
/// lease happened to be live, which is why the host is dropped first.
///
/// A second app's sweep row is the probe's nonempty control, and it does not
/// race the lane the way reading this app's row before the lane wakes would.
/// This host sweeps the one app it holds, so that row stays claimable for as
/// long as the test runs - which is what makes the hosted app's absence from
/// the same answer mean something.
#[compio::test]
async fn the_local_host_claims_runs_and_settles_its_own_journal_sweeps() {
    let root = tempfile::tempdir().unwrap();
    let app = AppId::mint();
    let config = LocalConfig {
        manager: ManagerConfig {
            // Short enough that the wait below outlasts any lease the lane took,
            // so a row it never settled is a candidate again by then.
            lease_ms: 2_000,
            // The recovery lane must publish no sweep of its own, so that the
            // rows this test reads back are the rows it submitted.
            recovery_interval_ms: 3_600_000,
            ..ManagerConfig::default()
        },
        ..LocalConfig::default()
    };
    let lease = Duration::from_millis(config.manager.lease_ms);
    let host = start(root.path(), &app, config, None);
    let sweep = JobSpec {
        id: JobId::mint(),
        app_id: app.clone(),
        operation: JobOperation::Reconcile {},
        available_at: 0.try_into().unwrap(),
    };
    let unhosted = JobSpec {
        id: JobId::mint(),
        app_id: AppId::mint(),
        operation: JobOperation::Reconcile {},
        available_at: 0.try_into().unwrap(),
    };
    submit_sweep(root.path(), &unhosted).await;
    submit_sweep(root.path(), &sweep).await;

    let creator = client(root.path(), &app).await;
    let receipt = until(async || match creator.job_receipt(&sweep).await {
        Ok(receipt) => receipt,
        Err(WorkflowServiceError::Unavailable(_)) => None,
        Err(error) => panic!("{error:?}"),
    })
    .await;
    assert_eq!(receipt.job, sweep);
    assert!(
        matches!(
            receipt.outcome,
            JobOutcome::Completed {} | JobOutcome::Waiting {}
        ),
        "{:?}",
        receipt.outcome
    );

    // Nothing competes for the row once the lane has stopped, so what the queue
    // offers after the lease window is what the lane left behind.
    drop(host);
    compio::time::sleep(lease * 2).await;
    assert_eq!(sweepable(root.path()).await, vec![unhosted.app_id]);
}
