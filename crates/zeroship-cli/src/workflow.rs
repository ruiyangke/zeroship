//! The local workflow host, independent of HTTP request isolates.
//!
//! `zeroship serve` composes the native workflow manager over its local
//! platform metadata file with the ordinary job consumer over the app's own
//! database and storage. Publishing the app archive registers and activates
//! its schedules, so creator activation arrives as a delivered job.

#![expect(clippy::future_not_send, reason = "the host owns a compio thread")]

mod host;
mod manager;

use futures::channel::oneshot;
use serde::{Deserialize, Serialize};
use std::{
    cell::Cell,
    collections::HashMap,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::JoinHandle,
    time::Duration,
};
use zeroship_bundle::LoadedWorker;
use zeroship_core::app_id::AppId;
use zeroship_runtime::{NativePlugin, RuntimeLimits};
use zeroship_workflow::service::{
    collection::CollectionOptions,
    fanout::FanoutOptions,
    propagation::PropagationOptions,
    reconciliation::ReconciliationOptions,
    runner::{consumer::ConsumerOptions, delivery::DeliveryOptions, TaskPayloadLimits},
    store::HostStorage,
    AppBackend,
};
use zeroship_workflow_v8::WorkflowBinding;

pub use host::{Composition, Production};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalConfig {
    pub max_archive_bytes: usize,
    pub max_source_bytes: usize,
    pub consumer: ConsumerConfig,
    pub manager: ManagerConfig,
    pub payloads: TaskPayloadLimits,
}
impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            max_archive_bytes: zeroship_bundle::MAX_COMPRESSED_BYTES,
            max_source_bytes: 32 * 1024 * 1024,
            consumer: ConsumerConfig::default(),
            manager: ManagerConfig::default(),
            payloads: TaskPayloadLimits::default(),
        }
    }
}

/// Execution capacity and delivery bounds of the local job consumer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConsumerConfig {
    /// Delivered jobs executing at once on the workflow thread.
    pub slots: usize,
    /// Delay before claiming again after the queue had no eligible work.
    pub idle_poll_ms: u64,
    /// Delay before claiming again after a failed claim or delivery.
    pub error_backoff_ms: u64,
    /// Hard bound on one delivered job's execution.
    pub execution_timeout_ms: u64,
    /// Bound on each claim, renewal, settlement and journal finalization.
    pub operation_timeout_ms: u64,
    /// Delay between settlement retries after an uncertain reply.
    pub retry_delay_ms: u64,
}
impl Default for ConsumerConfig {
    fn default() -> Self {
        Self {
            slots: 1,
            idle_poll_ms: 50,
            error_backoff_ms: 1_000,
            execution_timeout_ms: 30_000,
            operation_timeout_ms: 5_000,
            retry_delay_ms: 100,
        }
    }
}

/// Bounds of the native manager running inside the local host.
#[expect(
    clippy::struct_field_names,
    reason = "configuration keys state their unit"
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ManagerConfig {
    /// Delivery lease; heartbeats renew it while a job executes.
    pub lease_ms: u64,
    /// Lifetime of this process's registration and placement. The host
    /// renews both well within it (`LocalConfig::renew_interval`).
    pub placement_ttl_ms: u64,
    /// Delay between bounded calendar, recovery and hold maintenance passes.
    pub driver_interval_ms: u64,
    /// Bound on each maintenance lane's pass.
    pub lane_timeout_ms: u64,
    /// Periodic reconciliation and collection deadline.
    pub recovery_interval_ms: u64,
    /// Inactivity after which the app's recovery responsibility may close.
    pub idle_close_ms: u64,
    /// Bound on a closing attempt's delivery before responsibility reopens.
    pub closing_timeout_ms: u64,
    /// Delay before retrying a closing attempt that did not retire; it
    /// doubles with each consecutive attempt up to `closing_backoff_max_ms`.
    pub closing_backoff_ms: u64,
    pub closing_backoff_max_ms: u64,
}
impl Default for ManagerConfig {
    fn default() -> Self {
        Self {
            lease_ms: 30_000,
            placement_ttl_ms: 30_000,
            driver_interval_ms: 1_000,
            lane_timeout_ms: 10_000,
            recovery_interval_ms: 30_000,
            idle_close_ms: 900_000,
            closing_timeout_ms: 300_000,
            closing_backoff_ms: 60_000,
            closing_backoff_max_ms: 3_600_000,
        }
    }
}

pub fn config_from_args(args: &[String]) -> Result<LocalConfig, String> {
    let path = crate::parse_flag(args, "--workflow-config").map(PathBuf::from);
    LocalConfig::read(path.as_deref())
}
impl LocalConfig {
    pub fn read(path: Option<&Path>) -> Result<Self, String> {
        path.map_or_else(
            || Ok(Self::default()),
            |path| {
                let source = std::fs::read_to_string(path)
                    .map_err(|error| format!("read workflow config {}: {error}", path.display()))?;
                toml::from_str(&source).map_err(|error| format!("invalid workflow config: {error}"))
            },
        )
    }

    fn validate(self) -> Result<Self, String> {
        let consumer = self.consumer;
        let manager = self.manager;
        if self.max_archive_bytes == 0
            || self.max_archive_bytes > zeroship_bundle::MAX_COMPRESSED_BYTES
            || self.max_source_bytes == 0
            || consumer.slots == 0
            || [
                consumer.idle_poll_ms,
                consumer.error_backoff_ms,
                consumer.execution_timeout_ms,
                consumer.operation_timeout_ms,
                consumer.retry_delay_ms,
                manager.lease_ms,
                manager.driver_interval_ms,
                manager.lane_timeout_ms,
                manager.recovery_interval_ms,
            ]
            .contains(&0)
            // The renewal interval derived from the lifetime must be positive.
            || self.renew_interval().is_zero()
            || manager::recovery_options(self.manager_options())
                .validate()
                .is_err()
        {
            return Err("invalid local workflow limits".into());
        }
        self.payloads
            .validate()
            .map_err(|error| error.to_string())?;
        Ok(self)
    }

    fn consumer_options(&self) -> ConsumerOptions {
        let consumer = self.consumer;
        ConsumerOptions {
            slots: consumer.slots,
            max_scopes: 1,
            idle_poll: Duration::from_millis(consumer.idle_poll_ms),
            error_backoff: Duration::from_millis(consumer.error_backoff_ms),
            delivery: DeliveryOptions {
                execution_timeout: Duration::from_millis(consumer.execution_timeout_ms),
                operation_timeout: Duration::from_millis(consumer.operation_timeout_ms),
                retry_delay: Duration::from_millis(consumer.retry_delay_ms),
                reconciliation: ReconciliationOptions::default(),
                collection: CollectionOptions::default(),
                fanout: FanoutOptions::default(),
                propagation: PropagationOptions::default(),
            },
        }
    }

    const fn manager_options(&self) -> manager::ManagerOptions {
        manager::ManagerOptions {
            lease: Duration::from_millis(self.manager.lease_ms),
            placement_ttl: Duration::from_millis(self.manager.placement_ttl_ms),
            recovery_interval: Duration::from_millis(self.manager.recovery_interval_ms),
            lane_timeout: Duration::from_millis(self.manager.lane_timeout_ms),
            driver_interval: Duration::from_millis(self.manager.driver_interval_ms),
            idle_close: Duration::from_millis(self.manager.idle_close_ms),
            closing_timeout: Duration::from_millis(self.manager.closing_timeout_ms),
            closing_backoff: Duration::from_millis(self.manager.closing_backoff_ms),
            closing_backoff_max: Duration::from_millis(self.manager.closing_backoff_max_ms),
        }
    }

    const fn renew_interval(&self) -> Duration {
        Duration::from_millis(self.manager.placement_ttl_ms / 3)
    }

    /// A predecessor may hold the activation's delivery lease; its expiry,
    /// redelivery and the bounded activation itself must fit this wait.
    const fn activation_wait(&self) -> Duration {
        Duration::from_millis(
            self.manager
                .lease_ms
                .saturating_add(self.consumer.execution_timeout_ms)
                .saturating_add(self.consumer.operation_timeout_ms),
        )
    }
}

pub struct LocalHost {
    pub app: AppId,
    /// The app's workflow client; the HTTP and workflow isolates share it.
    pub backend: AppBackend,
    pub executable: Option<LoadedWorker>,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
    stopping: Arc<AtomicBool>,
}
impl LocalHost {
    #[expect(
        clippy::too_many_arguments,
        reason = "the serve command supplies each independently configured host resource"
    )]
    pub fn start(
        root: &Path,
        app: AppId,
        config: LocalConfig,
        deployment: Option<&Path>,
        storage: HostStorage,
        env_vars: HashMap<String, String>,
        peers: Vec<Arc<dyn NativePlugin>>,
        limits: RuntimeLimits,
    ) -> Result<Self, String> {
        Self::start_with(
            root, app, config, deployment, storage, env_vars, peers, limits, Production,
        )
    }

    /// Start after the delivered activation of the published archive has
    /// committed, so new runs select that deployment.
    #[expect(
        clippy::too_many_arguments,
        reason = "tests substitute only the composition of an otherwise normal host"
    )]
    pub(crate) fn start_with<C: Composition>(
        root: &Path,
        app: AppId,
        config: LocalConfig,
        deployment: Option<&Path>,
        storage: HostStorage,
        env_vars: HashMap<String, String>,
        peers: Vec<Arc<dyn NativePlugin>>,
        limits: RuntimeLimits,
        composition: C,
    ) -> Result<Self, String> {
        let config = config.validate()?;
        let wait = config.activation_wait();
        let settings = host::Settings {
            app: app.clone(),
            config,
            deployment: crate::deployment::AppDeployment::new(root, deployment)?,
            storage,
            env_vars,
            peers,
            limits,
        };
        let (ready, receive) = std::sync::mpsc::sync_channel(1);
        let (stop, stopped) = oneshot::channel::<()>();
        let stopping = Arc::new(AtomicBool::new(false));
        let host_stopping = stopping.clone();
        zeroship_runtime::init_v8();
        let thread = std::thread::Builder::new()
            .name("workflow-host".into())
            .spawn(move || {
                let runtime = match compio::runtime::Runtime::new() {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready.send(Err(error.to_string()));
                        return;
                    }
                };
                runtime.block_on(async move {
                    let opened = match host::open(settings, composition).await {
                        Ok(opened) => opened,
                        Err(error) => {
                            let _ = ready.send(Err(error.to_string()));
                            return;
                        }
                    };
                    let host::Opened {
                        api,
                        backend,
                        executable,
                        activation,
                        host,
                    } = opened;
                    let (abandon, abandoned) = oneshot::channel::<()>();
                    let liveness = HostLiveness {
                        armed: Rc::new(Cell::new(false)),
                        stopping: host_stopping,
                    };
                    let armed = liveness.armed.clone();
                    let announce = async move {
                        let applied = match &activation {
                            Some(job) => host::applied(&api, job, wait).await,
                            None => Ok(()),
                        };
                        match applied {
                            Ok(()) => {
                                armed.set(true);
                                let _ = ready.send(Ok((backend, executable)));
                            }
                            Err(error) => {
                                let _ = ready.send(Err(error.to_string()));
                                let _ = abandon.send(());
                            }
                        }
                    };
                    // Only an explicit abandonment stops the host; a successful
                    // announcement drops its sender without sending.
                    let abandoned = async move {
                        if abandoned.await.is_err() {
                            std::future::pending::<()>().await;
                        }
                    };
                    let stop = async move {
                        futures::future::select(stopped, Box::pin(abandoned)).await;
                    };
                    futures::join!(host.run_until(stop), announce);
                    drop(liveness);
                });
            })
            .map_err(|error| format!("start workflow host: {error}"))?;
        let (backend, executable) = match receive.recv() {
            Ok(Ok(started)) => started,
            outcome => {
                let _ = thread.join();
                return Err(match outcome {
                    Ok(Err(error)) => error,
                    _ => "workflow host stopped during startup".into(),
                });
            }
        };
        Ok(Self {
            app,
            backend,
            executable,
            stop: Some(stop),
            thread: Some(thread),
            stopping,
        })
    }

    /// The `env.workflows` plugin for request isolates.
    #[must_use]
    pub fn binding(&self) -> WorkflowBinding {
        WorkflowBinding::service(self.backend.clone())
    }
}
impl Drop for LocalHost {
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

// A request server must not keep accepting durable work after its host dies.
struct HostLiveness {
    armed: Rc<Cell<bool>>,
    stopping: Arc<AtomicBool>,
}
impl Drop for HostLiveness {
    fn drop(&mut self) {
        if self.armed.get() && !self.stopping.load(Ordering::Acquire) {
            eprintln!("[zeroship] workflow host stopped unexpectedly");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests;
