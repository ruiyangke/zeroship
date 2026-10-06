//! The worker's workflow host: one `WorkerHost` on a dedicated compio thread.
//!
//! The host pulls claimable jobs from this worker's execution zone under the
//! process's enrolled instance identity, prepares each claimed app on demand
//! from resources this worker is independently authorized to use, and runs
//! the deliveries in its execution slots. HTTP runtime threads never construct
//! workflow authority: they resolve a backend from the [`RemoteWorkflows`] the
//! host builds for every app the worker's zone may act for, and the service
//! admits each call by the zone frozen on the authenticating instance.
//!
//! One host per process, never one per HTTP thread: the slot count is this
//! process's, and independent hosts would each claim a full batch for it.

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
    app_id::AppId, config::PlaintextPeers, service_peers::ServiceAuth,
};
use zeroship_data_v8::service::DbService;
use zeroship_runtime::NativePlugin;
use zeroship_storage::{StorageBackendConfig, StorageStore};
use zeroship_workflow::WorkflowServiceError;
use zeroship_workflow_client::{Options as ClientOptions, Transport, WorkerCoordinator};
use zeroship_workflow_runner::{
    consumer::ConsumerOptions,
    delivery::DeliveryOptions,
    host::{HostOptions, WorkerHost},
    prepared::{AppFeed, PreparedOptions},
    remote::RemoteWorkflows,
    PayloadObjects, TaskPayloadLimits,
};

/// This host's ceiling on one delivered job's execution.
///
/// Not the definition of an attempt's length: the manager's attempt cap is,
/// and every delivery carries what remains of it and of its lease. Delivery
/// cuts an execution at the earliest of the three, so a manager configured
/// with a shorter attempt is obeyed and this ceiling only stops one longer
/// than this host will run.
const EXECUTION_CEILING: Duration = Duration::from_secs(30);
/// Bound on each claim, renewal, settlement, give-back and journal finalization.
const OPERATION_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound on preparing one claimed app: its metadata, environment and creator
/// runtime, before its delivery can run.
const PREPARATION_TIMEOUT: Duration = Duration::from_secs(10);
/// Delay between settlement retries after an uncertain reply.
const RETRY_DELAY: Duration = Duration::from_millis(200);
/// Delay before claiming again after a claim that walked the whole zone and
/// found nothing claimable.
const IDLE_POLL: Duration = Duration::from_millis(500);
/// Delay before claiming again after a failed claim or delivery.
const ERROR_BACKOFF: Duration = Duration::from_secs(1);
/// Bound on an app's retained source when loading its executable.
const MAX_SOURCE_BYTES: u64 = zeroship_bundle::MAX_DECOMPRESSED_BYTES;

/// What the grace must hold for a delivery claimed just before the stop: its
/// app's preparation, its execution to this host's ceiling and its settlement.
const DELIVERY_GRACE: Duration = PREPARATION_TIMEOUT
    .saturating_add(EXECUTION_CEILING)
    .saturating_add(OPERATION_TIMEOUT);

/// What a claim the stop finds pending takes to end: it waits one operation
/// bound for its reply and is cut one operation bound after that, and the
/// deliveries the reply brings go back side by side inside a third.
const CLAIM_TAIL: Duration = OPERATION_TIMEOUT.saturating_mul(3);

/// The longer of two bounds, in a constant.
const fn longer(left: Duration, right: Duration) -> Duration {
    if left.as_nanos() >= right.as_nanos() {
        left
    } else {
        right
    }
}

/// The shortest drain a stopping worker may be given.
///
/// Its first part is the grace the host's running deliveries get, counted from
/// the stop: a delivery claimed just before the stop has its app prepared, runs
/// to this host's execution ceiling and is then settled. A claim the stop finds
/// pending runs on to its reply beside that grace and gives back what it
/// delivers, so the grace holds that tail too. The last operation bound is for
/// what the grace could not finish: it is cancelled when the grace runs out
/// and released within it. A drain cut shorter leaves a delivery leased until
/// its lease lapses and runs it again elsewhere; the deployment's termination
/// grace is stated from `worker.shutdown_timeout`, so the requirement is
/// enforced there.
pub const MIN_SHUTDOWN_TIMEOUT: Duration =
    longer(DELIVERY_GRACE, CLAIM_TAIL).saturating_add(OPERATION_TIMEOUT);

/// Refuse a `worker.shutdown_timeout` shorter than [`MIN_SHUTDOWN_TIMEOUT`].
///
/// # Errors
/// Names the setting and the bounds the requirement is made of.
pub fn validate_shutdown_timeout(secs: u64) -> Result<u64, String> {
    if Duration::from_secs(secs) < MIN_SHUTDOWN_TIMEOUT {
        return Err(format!(
            "worker.shutdown_timeout must be at least {}s: a stopping worker lets a delivered \
             workflow execution prepare its app within {}s, run to its {}s ceiling and settle \
             within {}s, and alongside it lets a claim pending at the stop reply and give back \
             what it brought within {}s, then releases within another {}s whatever is still \
             running",
            MIN_SHUTDOWN_TIMEOUT.as_secs(),
            PREPARATION_TIMEOUT.as_secs(),
            EXECUTION_CEILING.as_secs(),
            OPERATION_TIMEOUT.as_secs(),
            CLAIM_TAIL.as_secs(),
            OPERATION_TIMEOUT.as_secs(),
        ));
    }
    Ok(secs)
}

/// The workflow host settings resolved from the worker's configuration.
#[derive(Debug, Clone)]
pub struct WorkflowHostConfig {
    /// Manager origin; HTTPS, or HTTP to a literal loopback address or to an
    /// origin named in [`Self::plaintext_peers`].
    pub manager_url: String,
    /// Apps whose prepared resources may remain resident.
    pub prepared_apps: usize,
    /// Delivered jobs executing at once.
    pub slots: usize,
    /// Origins this process may reach over plaintext HTTP. Empty by default.
    pub plaintext_peers: PlaintextPeers,
    /// `worker.shutdown_timeout`: the whole drain a stop allows the host.
    pub shutdown_timeout: Duration,
}

impl WorkflowHostConfig {
    /// Refuse an unusable manager origin or bound without keys or sockets.
    ///
    /// # Errors
    /// Names the setting that cannot run a host.
    pub fn validate(&self) -> Result<(), String> {
        Transport::validate_config(&self.manager_url, &self.client_options()).map_err(|_| {
            "worker.workflow_manager_url must be an HTTPS origin, or HTTP to a literal \
             loopback address or an origin named in plaintext_peers, with no path, query \
             or credentials"
                .to_owned()
        })?;
        if self.slots == 0 {
            return Err("worker.workflow_slots must be positive".into());
        }
        if self.prepared_apps < self.slots {
            return Err(
                "worker.workflow_prepared_apps must be at least worker.workflow_slots, so \
                 every executing delivery's app stays prepared"
                    .into(),
            );
        }
        Ok(())
    }

    fn host_options(&self) -> HostOptions {
        HostOptions {
            consumer: ConsumerOptions {
                slots: self.slots,
                idle_poll: IDLE_POLL,
                error_backoff: ERROR_BACKOFF,
                // The drain's last operation bound releases what the grace
                // could not finish, so the grace ends that much earlier.
                drain: self.shutdown_timeout.saturating_sub(OPERATION_TIMEOUT),
                delivery: DeliveryOptions {
                    execution_timeout: EXECUTION_CEILING,
                    operation_timeout: OPERATION_TIMEOUT,
                    retry_delay: RETRY_DELAY,
                },
            },
            prepared: PreparedOptions {
                capacity: self.prepared_apps,
                operation_timeout: PREPARATION_TIMEOUT,
            },
        }
    }

    /// The exchange bounds and plaintext allowance of the manager client, the
    /// one client this host builds: claims, deliveries, task payloads and the
    /// request path's run calls all cross on it.
    fn client_options(&self) -> ClientOptions {
        ClientOptions {
            plaintext_peers: self.plaintext_peers.clone(),
            ..ClientOptions::default()
        }
    }
}

/// Process resources the host composes creator execution from. Every one of
/// them is already the worker's own: nothing here arrives with a claimed job.
#[allow(missing_debug_implementations)]
pub struct HostResources {
    /// The enrolled instance identity every manager and Control call uses.
    pub service_auth: Arc<ServiceAuth>,
    pub control_url: String,
    /// The creator database service `env.db` uses.
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
    /// The process-wide registry of held apps. Each prepared app holds its
    /// app here, so its key, bindings and environment outlive every execution
    /// running from it and nothing longer.
    pub residency: crate::residency::AppResidency,
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
    /// Start the host thread. It claims from the manager on its own; a claim
    /// against a manager that is not yet reachable is retried rather than
    /// fatal.
    ///
    /// The enrolled client and the request path's backend registry are built
    /// HERE, on the calling thread, and the registry is returned so HTTP
    /// threads can serve `env.workflows` before the host has claimed anything.
    ///
    /// # Errors
    /// Reports an unusable manager origin, storage, thread or runtime.
    pub fn start(
        config: WorkflowHostConfig,
        resources: HostResources,
    ) -> Result<(Self, RemoteWorkflows), String> {
        config.validate()?;
        let client = WorkerCoordinator::new(
            &config.manager_url,
            resources.service_auth.clone(),
            config.client_options(),
        )
        .map_err(|error| format!("workflow manager client: {error}"))?;
        let objects = PayloadObjects::open(
            StorageStore::open(&resources.storage)
                .map_err(|error| format!("workflow payload storage: {error}"))?,
        )
        .map_err(|error| format!("workflow payload storage: {error}"))?;
        let workflows = RemoteWorkflows::new(
            client.clone(),
            objects,
            TaskPayloadLimits::default().max_payload_bytes,
        )
        .map_err(|error| format!("workflow remote registry: {error}"))?;
        let host_workflows = workflows.clone();
        let host =
            Self::spawn(move |stopped| run(config, resources, client, host_workflows, stopped))?;
        Ok((host, workflows))
    }

    /// Run `body` on a host thread of its own, under its own compio runtime.
    ///
    /// `body` is handed the stop request, which resolves when [`Self::stop`]
    /// is called or the host is dropped, and the host's exit is `body`'s
    /// result.
    pub(crate) fn spawn<B, F>(body: B) -> Result<Self, String>
    where
        B: FnOnce(oneshot::Receiver<()>) -> F + Send + 'static,
        F: std::future::Future<Output = Result<(), String>> + 'static,
    {
        let (stop, stopped) = oneshot::channel::<()>();
        let (finished, exit) = oneshot::channel::<Result<(), String>>();
        let thread = zeroship_workflow_runner::host::thread()
            .spawn(move || {
                let result = match compio::runtime::Runtime::new().map_err(zeroship_memlock::explain) {
                    Ok(runtime) => runtime.block_on(body(stopped)),
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
    /// Pending forever once [`Self::stop`] has been called.
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

    /// Stop claiming, now. Deliveries already running carry on to their
    /// settlement; [`Self::shutdown`] is what waits for them. Idempotent.
    pub fn stop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }

    /// Stop the host and wait at most `budget` for its running executions to
    /// finish and settle and its thread to end.
    ///
    /// A host still running at the budget is left to the process's exit rather
    /// than joined: its unfinished deliveries stay leased until their leases
    /// lapse and run again elsewhere, and joining it would hold the drain past
    /// the bound the deployment's termination grace is stated from.
    ///
    /// # Errors
    /// Reports a host that failed while running or draining, and one that did
    /// not drain within `budget`.
    pub async fn shutdown(mut self, budget: Duration) -> Result<(), String> {
        self.stop();
        let Ok(result) = compio::time::timeout(budget, self.exit.clone()).await else {
            drop(self.thread.take());
            return Err(format!(
                "the workflow host did not drain within {}s; its unfinished deliveries \
                 return to the queue when their leases lapse",
                budget.as_secs()
            ));
        };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        result
    }
}

impl Drop for WorkflowHost {
    fn drop(&mut self) {
        self.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Drain a stopping worker: its HTTP server and its workflow host, side by
/// side.
///
/// `stop` is the process's stop signal. When it arrives the host stops
/// claiming at once and its running executions finish and settle while `http`,
/// the server's own drain, completes. ntex bounds the HTTP drain by
/// `worker.shutdown_timeout` and `budget`, that same setting, bounds the
/// host's, so both end within one `shutdown_timeout` of the signal. A server
/// that stops without the signal, as when it is told to after the host failed,
/// drains first and the host after it. The instance retires once this
/// returns.
pub async fn drain<H: std::future::Future>(
    stop: impl std::future::Future<Output = ()>,
    http: H,
    host: Option<WorkflowHost>,
    budget: Duration,
) -> (H::Output, Result<(), String>) {
    let http = std::pin::pin!(http);
    let stop = std::pin::pin!(stop);
    match futures::future::select(stop, http).await {
        futures::future::Either::Left(((), http)) => {
            futures::future::join(http, shutdown(host, budget)).await
        }
        futures::future::Either::Right((served, _)) => (served, shutdown(host, budget).await),
    }
}

async fn shutdown(host: Option<WorkflowHost>, budget: Duration) -> Result<(), String> {
    match host {
        Some(host) => host.shutdown(budget).await,
        None => Ok(()),
    }
}

async fn run(
    config: WorkflowHostConfig,
    resources: HostResources,
    client: WorkerCoordinator,
    workflows: RemoteWorkflows,
    stopped: oneshot::Receiver<()>,
) -> Result<(), String> {
    let worker = client.worker_id().clone();
    // The version feed decides which prepared apps stay cached: Control lists
    // every live app, so one it omits is deleted. Before the first poll
    // answers, nothing is known to be gone.
    let versions = resources.versions.clone();
    let feed: AppFeed = Rc::new(move |app: &AppId| {
        versions.read().ok().is_none_or(|versions| {
            versions
                .as_ref()
                .is_none_or(|versions| versions.contains_key(app))
        })
    });
    let provider = ProductionResources::open(resources)?;
    // The SAME enrolled client the host claims through. Every creator call and
    // every task payload operation crosses on it, so a claimed job is served by
    // the identity its requests are signed with.
    let factory = WorkflowCreatorFactory::new(
        provider,
        client.clone(),
        workflows,
        TaskPayloadLimits::default(),
        OPERATION_TIMEOUT,
    )
    .map_err(|error| format!("workflow creator factory: {error}"))?;
    let mut host = WorkerHost::new(client, factory, feed, config.host_options())
        .map_err(|error| format!("workflow host: {error}"))?;
    tracing::info!(
        worker = worker.as_str(),
        prepared_apps = config.prepared_apps,
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
/// A claimed job names an app; it cannot select a login, schema, object store
/// or artifact source. Control, answering this enrolled instance, confirms the
/// app and supplies its environment and data key; the database, storage and
/// artifacts are the ones this worker serves every request with.
struct ProductionResources {
    control_url: String,
    service_auth: Arc<ServiceAuth>,
    db: Arc<DbService>,
    objects: StorageStore,
    blob_store: Arc<dyn BlobStore>,
    envs: SharedEnvs,
    contexts: Rc<SharedContexts>,
    residency: crate::residency::AppResidency,
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
            residency: resources.residency,
        })
    }
}

impl WorkflowResourceProvider for ProductionResources {
    async fn resolve(
        &self,
        app: &AppId,
    ) -> Result<WorkflowResources, WorkflowServiceError> {
        // FIRST, before the environment version and `is_bound` checks below:
        // they let this preparation skip a fetch because another holder
        // supplied the material, and only a residency taken before them keeps
        // that holder's last drop from withdrawing it in between. The prepared
        // app keeps it, and so does every execution holding that app.
        let residency = Rc::new(self.residency.reside(app.clone()));
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
        // THE RETENTION HOLD IS THE SERVICE'S. `resolve_task_executable` takes it
        // from the deployments source bound to the journal it reads, so the hold
        // that keeps a pinned artifact alive is taken where the pin is resolved.
        // A second hold from this side would name a scope the service does not
        // consult.
        Ok(WorkflowResources {
            objects: PayloadObjects::open(self.objects.clone())?,
            artifacts: self.blob_store.clone(),
            max_source_bytes: usize::try_from(MAX_SOURCE_BYTES)
                .map_err(|_| unavailable("app source budget is not representable"))?,
            contexts: self.contexts.clone(),
            residency,
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
            env_vars: HashMap::new(),
            env: env.snapshot.clone(),
            limits: cache::runtime_limits_from_app(&info.runtime),
            net_policy: cache::net_policy_from_app(app, &info.net_policy),
            peers: self.peers.clone(),
            meter: Some(self.meter.clone()),
        })
    }
}

fn unavailable(message: &str) -> WorkflowServiceError {
    WorkflowServiceError::Unavailable(message.to_owned())
}

#[cfg(test)]
mod tests;
