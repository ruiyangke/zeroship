//! The worker's workflow host: one `WorkerHost` on a dedicated compio thread.
//!
//! The host owns this process's enrolled instance identity towards the
//! workflow manager, its advertised capacity and its assignment registry. It
//! registers, polls and renews placements, prepares each assigned app from
//! resources this worker is independently authorized to use, and consumes
//! delivered jobs. HTTP runtime threads never construct workflow authority:
//! they resolve the fixed backends the host publishes in [`ReadyApps`], and an
//! app that is unknown or not ready here is refused as retryable.
//!
//! One host per process, never one per HTTP thread: independent hosts under
//! the same enrolled identity would each advertise the capacity and retire
//! each other's policy generations.

#![expect(
    clippy::future_not_send,
    reason = "the host's creator engine and V8 executor live on its compio thread"
)]

use crate::{
    cache,
    sync::{self, SharedEnvs, SharedVersions},
    workflow_creator::{WorkflowCreatorFactory, WorkflowResourceProvider, WorkflowResources},
    workflow_runtime::{WorkflowAppContext, WorkflowContextProvider},
};
use futures::{
    channel::oneshot,
    future::{FutureExt, LocalBoxFuture, Shared},
};
use std::{
    collections::HashMap,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::JoinHandle,
    time::Duration,
};
use zeroship_bundle::BlobStore;
use zeroship_core::{
    app_derivation, app_id::AppId, schema_name::SchemaName, service_peers::ServiceAuth,
    workflow_coordination::AssignedScope,
};
use zeroship_data_orm::{
    binding::{DbBinding, COLD_START_DEPLOY_TOKEN},
    encryption::ProjectKeySource,
};
use zeroship_data_v8::service::DbService;
use zeroship_runtime::NativePlugin;
use zeroship_storage::{StorageBackendConfig, StorageStore};
use zeroship_workflow::{
    deployment_holds::RemoteDeploymentHolds,
    service::{
        collection::CollectionOptions,
        fanout::FanoutOptions,
        propagation::PropagationOptions,
        reconciliation::ReconciliationOptions,
        runner::{
            assignments::AssignmentOptions,
            consumer::ConsumerOptions,
            delivery::DeliveryOptions,
            host::{HostOptions, WorkerHost},
            ready::ReadyApps,
            TaskPayloadLimits,
        },
        store::HostStorage,
        AppDeployments, HostPolicies,
    },
    WorkflowServiceError,
};
use zeroship_workflow_client::{Options as ClientOptions, Transport, WorkerCoordinator};

/// Bound on one delivered job's execution.
const EXECUTION_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound on each claim, renewal, settlement, publication and journal finalization.
const OPERATION_TIMEOUT: Duration = Duration::from_secs(10);
/// Delay between settlement retries after an uncertain reply.
const RETRY_DELAY: Duration = Duration::from_millis(200);
/// Delay before an app with no claimable work is claimed again.
const IDLE_POLL: Duration = Duration::from_millis(500);
/// Delay before claiming again after a failed claim or delivery.
const ERROR_BACKOFF: Duration = Duration::from_secs(1);
/// Delay between registration renewals; well inside the manager's worker lifetime.
const REGISTRATION_INTERVAL: Duration = Duration::from_secs(5);
/// Delay between placement scans; bounds how soon a new assignment is prepared.
const ASSIGNMENT_INTERVAL: Duration = Duration::from_secs(1);
/// Delay between policy lease renewals of prepared assignments.
const POLICY_INTERVAL: Duration = Duration::from_secs(5);
/// Bound on an app's retained source when loading its executable.
const MAX_SOURCE_BYTES: u64 = zeroship_bundle::MAX_DECOMPRESSED_BYTES;

/// The workflow host settings resolved from the worker's configuration.
#[derive(Debug, Clone)]
pub struct WorkflowHostConfig {
    /// Manager origin; HTTPS, or HTTP to a literal loopback address.
    pub manager_url: String,
    /// App placements advertised to the manager.
    pub capacity: usize,
    /// Delivered jobs executing at once.
    pub slots: usize,
}

impl WorkflowHostConfig {
    /// Refuse an unusable manager origin or bound without keys or sockets.
    ///
    /// # Errors
    /// Names the setting that cannot run a host.
    pub fn validate(&self) -> Result<(), String> {
        Transport::validate_config(&self.manager_url, client_options()).map_err(|_| {
            "worker.workflow_manager_url must be an HTTPS origin, or HTTP to a literal \
             loopback address, with no path, query or credentials"
                .to_owned()
        })?;
        if self.capacity == 0 || u32::try_from(self.capacity).is_err() {
            return Err("worker.workflow_capacity must be a positive placement count".into());
        }
        if self.slots == 0 {
            return Err("worker.workflow_slots must be positive".into());
        }
        Ok(())
    }

    fn host_options(&self) -> HostOptions {
        HostOptions {
            consumer: ConsumerOptions {
                slots: self.slots,
                max_scopes: self.capacity,
                idle_poll: IDLE_POLL,
                error_backoff: ERROR_BACKOFF,
                delivery: DeliveryOptions {
                    execution_timeout: EXECUTION_TIMEOUT,
                    operation_timeout: OPERATION_TIMEOUT,
                    retry_delay: RETRY_DELAY,
                    reconciliation: ReconciliationOptions::default(),
                    collection: CollectionOptions::default(),
                    fanout: FanoutOptions::default(),
                    propagation: PropagationOptions::default(),
                },
            },
            assignments: AssignmentOptions {
                max_scopes: self.capacity,
                operation_timeout: OPERATION_TIMEOUT,
            },
            registration_interval: REGISTRATION_INTERVAL,
            assignment_interval: ASSIGNMENT_INTERVAL,
            policy_interval: POLICY_INTERVAL,
        }
    }
}

fn client_options() -> ClientOptions {
    ClientOptions::default()
}

/// Process resources the host composes creator execution from. Every one of
/// them is already the worker's own: nothing here arrives with a placement.
#[allow(missing_debug_implementations)]
pub struct HostResources {
    /// The enrolled instance identity every manager and Control call uses.
    pub service_auth: Arc<ServiceAuth>,
    pub control_url: String,
    /// The creator database service `env.db` uses; journals share its login.
    pub db_service: Arc<DbService>,
    /// The creator object store `env.storage` uses; payloads live there.
    pub storage: StorageBackendConfig,
    pub kv_store: Option<zeroship_kv::KvStore>,
    /// Normal app artifacts, read by deployment hash.
    pub blob_store: Arc<dyn BlobStore>,
    pub meter: Arc<zeroship_metering::Meter>,
    /// Control's app metadata as polled by the version poller.
    pub versions: SharedVersions,
    /// App environments shared with every HTTP thread.
    pub envs: SharedEnvs,
}

type Exit = Shared<LocalBoxFuture<'static, Result<(), String>>>;

/// A running host thread. Dropping it without [`Self::shutdown`] stops the
/// host and joins its thread.
#[allow(missing_debug_implementations)]
pub struct WorkflowHost {
    stop: Option<oneshot::Sender<()>>,
    exit: Exit,
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl WorkflowHost {
    /// Start the host thread. It registers with the manager on its own; a
    /// manager that is not yet reachable is retried rather than fatal.
    ///
    /// # Errors
    /// Reports a thread or runtime that could not be created.
    pub fn start(
        config: WorkflowHostConfig,
        resources: HostResources,
        ready: ReadyApps,
    ) -> Result<Self, String> {
        config.validate()?;
        let (stop, stopped) = oneshot::channel::<()>();
        let (finished, exit) = oneshot::channel::<Result<(), String>>();
        let thread = std::thread::Builder::new()
            .name("workflow-host".into())
            .spawn(move || {
                let result = match compio::runtime::Runtime::new() {
                    Ok(runtime) => runtime.block_on(run(config, resources, ready, stopped)),
                    Err(error) => Err(format!("workflow host runtime: {error}")),
                };
                let _ = finished.send(result);
            })
            .map_err(|error| format!("start the workflow host thread: {error}"))?;
        Ok(Self {
            stop: Some(stop),
            exit: async move {
                exit.await
                    .unwrap_or_else(|_| Err("the workflow host thread panicked".into()))
            }
            .boxed_local()
            .shared(),
            stopping: Arc::new(AtomicBool::new(false)),
            thread: Some(thread),
        })
    }

    /// Resolves with the reason if the host stops without being asked to.
    /// Pending forever once [`Self::shutdown`] has begun.
    pub fn failure(&self) -> impl std::future::Future<Output = String> + 'static {
        let exit = self.exit.clone();
        let stopping = self.stopping.clone();
        async move {
            let result = exit.await;
            if stopping.load(Ordering::Acquire) {
                return std::future::pending().await;
            }
            match result {
                Ok(()) => "the workflow host stopped".to_owned(),
                Err(error) => error,
            }
        }
    }

    /// Stop the host: its assignment bindings close, withdrawing every
    /// published backend and revoking their policy generations; the manager
    /// is told this worker is draining while delivered executions join; then
    /// the thread is joined. Call after HTTP has drained and before the
    /// instance retires.
    ///
    /// # Errors
    /// Reports a host that failed while running or draining.
    pub async fn shutdown(mut self) -> Result<(), String> {
        self.stopping.store(true, Ordering::Release);
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let result = self.exit.clone().await;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        result
    }
}

impl Drop for WorkflowHost {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

async fn run(
    config: WorkflowHostConfig,
    resources: HostResources,
    ready: ReadyApps,
    stopped: oneshot::Receiver<()>,
) -> Result<(), String> {
    let client = WorkerCoordinator::new(
        &config.manager_url,
        resources.service_auth.clone(),
        client_options(),
    )
    .map_err(|error| format!("workflow manager client: {error}"))?;
    let worker = client.worker_id().clone();
    // The SAME enrolled client the host coordinates through. A refused journal is
    // reported under the worker instance identity that found it, so the manager
    // can attribute the repair.
    let repair = Rc::new(client.clone());
    let policies = Arc::new(HostPolicies::default());
    let provider = ProductionResources::open(resources)?;
    let factory = WorkflowCreatorFactory::new(
        provider,
        policies.clone(),
        &worker,
        TaskPayloadLimits::default(),
    )
    .map_err(|error| format!("workflow creator factory: {error}"))?
    .with_journal_repair(repair);
    let mut host = WorkerHost::new(client, policies, factory, ready, config.host_options())
        .map_err(|error| format!("workflow host: {error}"))?;
    tracing::info!(
        worker = worker.as_str(),
        capacity = config.capacity,
        slots = config.slots,
        "workflow host started"
    );
    // A dropped sender is a stop request as well: the owner is gone.
    host.run_until(stopped.map(|_| ()))
        .await
        .map_err(|error| format!("workflow host: {error}"))
}

/// Creator resources from the worker's own trusted app metadata.
///
/// The assignment names an app; it cannot select a login, schema, object
/// store or artifact source. Control, answering this enrolled instance,
/// confirms the app and supplies its environment and data key; the database,
/// storage and artifacts are the ones this worker serves every request with.
struct ProductionResources {
    control_url: String,
    service_auth: Arc<ServiceAuth>,
    db: Arc<DbService>,
    objects: StorageStore,
    blob_store: Arc<dyn BlobStore>,
    envs: SharedEnvs,
    contexts: Rc<SharedContexts>,
}

impl ProductionResources {
    fn open(resources: HostResources) -> Result<Self, String> {
        let objects = StorageStore::open(&resources.storage)
            .map_err(|error| format!("workflow payload storage: {error}"))?;
        let meter = Some(resources.meter.clone());
        // The ordinary native primitives of an HTTP isolate, minus workflows:
        // workflow isolates receive their own backend from the loader.
        let mut peers: Vec<Arc<dyn NativePlugin>> = vec![resources.db_service.plugin()];
        if let Some(store) = resources.kv_store.clone() {
            peers.push(Arc::new(zeroship_kv_v8::KvBinding::new(store, meter.clone())));
        }
        peers.push(Arc::new(zeroship_storage_v8::StorageBinding::new(
            objects.clone(),
            meter,
        )));
        peers.push(Arc::new(zeroship_runtime::auth::AuthPlugin));
        Ok(Self {
            control_url: resources.control_url,
            service_auth: resources.service_auth,
            db: resources.db_service,
            objects,
            blob_store: resources.blob_store,
            envs: resources.envs.clone(),
            contexts: Rc::new(SharedContexts {
                versions: resources.versions,
                envs: resources.envs,
                peers,
                meter: resources.meter,
            }),
        })
    }
}

impl WorkflowResourceProvider for ProductionResources {
    async fn resolve(
        &self,
        scope: &AssignedScope,
    ) -> Result<WorkflowResources, WorkflowServiceError> {
        let app = &scope.app_id;
        // Control authorizes this instance to read the app; an app it does
        // not serve to this worker is never prepared.
        let info = sync::fetch_app_version(&self.control_url, &self.service_auth, app)
            .await
            .map_err(|error| {
                tracing::warn!(app = app.as_str(), %error, "workflow app metadata unavailable");
                unavailable("workflow app metadata is unavailable")
            })?;
        if sync::cached_env_version(&self.envs, app) != Some(info.env_version) {
            let env = sync::fetch_app_env_supplying(
                &self.control_url,
                &self.service_auth,
                app,
                Some(self.db.project_keys()),
                Some(self.db.app_bindings()),
            )
            .await
            .map_err(|error| {
                tracing::warn!(app = app.as_str(), %error, "workflow app environment unavailable");
                unavailable("workflow app environment is unavailable")
            })?;
            sync::put_env_from_json(&self.envs, app.clone(), &env, info.env_version)
                .map_err(|_| unavailable("workflow app environment is invalid"))?;
        }
        let schema = app_schema(app)?;
        let holds = RemoteDeploymentHolds::new(
            &self.control_url,
            self.service_auth.clone(),
            scope,
            client_options(),
        )?;
        let deployments = AppDeployments::new(
            self.blob_store.clone(),
            usize::try_from(MAX_SOURCE_BYTES)
                .map_err(|_| unavailable("app source budget is not representable"))?,
        )?
        .with_hold_client(Rc::new(holds));
        Ok(WorkflowResources {
            storage: HostStorage {
                connection: self.db.connection().clone(),
                keys: ProjectKeySource::supplied(self.db.project_keys().clone()),
                binding: DbBinding::platform(app.as_str(), COLD_START_DEPLOY_TOKEN, schema),
                objects: self.objects.clone(),
            },
            deployments,
            // No signal-capability key is provisioned to workers yet.
            signal_authority: None,
            contexts: self.contexts.clone(),
        })
    }
}

/// Current metadata for each execution, read from the process-wide caches the
/// version poller and HTTP threads keep fresh.
struct SharedContexts {
    versions: SharedVersions,
    envs: SharedEnvs,
    peers: Vec<Arc<dyn NativePlugin>>,
    meter: Arc<zeroship_metering::Meter>,
}

impl WorkflowContextProvider for SharedContexts {
    fn resolve(&self, app: &AppId) -> Result<WorkflowAppContext, WorkflowServiceError> {
        // An app Control no longer lists, or one without a fetched
        // environment, gets no execution; the delivery retries later.
        let info = self
            .versions
            .read()
            .ok()
            .and_then(|versions| versions.as_ref()?.get(app).cloned())
            .ok_or_else(|| unavailable("workflow app metadata is not current"))?;
        let env = sync::get_env(&self.envs, app)
            .ok_or_else(|| unavailable("workflow app environment is not loaded"))?;
        Ok(WorkflowAppContext {
            app: app.clone(),
            schema: app_schema(app)?,
            env_vars: HashMap::new(),
            env: env.snapshot.clone(),
            limits: cache::runtime_limits_from_app(&info.runtime),
            net_policy: cache::net_policy_from_app(app, &info.net_policy),
            peers: self.peers.clone(),
            meter: Some(self.meter.clone()),
        })
    }
}

fn app_schema(app: &AppId) -> Result<SchemaName, WorkflowServiceError> {
    SchemaName::new(&app_derivation::schema_name(app))
        .map_err(|_| WorkflowServiceError::InvalidRequest("invalid app schema".into()))
}

fn unavailable(message: &str) -> WorkflowServiceError {
    WorkflowServiceError::Unavailable(message.to_owned())
}

#[cfg(test)]
mod tests;
