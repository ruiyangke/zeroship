//! Customer-owned local workflow host, independent of HTTP request isolates.

#![expect(clippy::future_not_send, reason = "the worker owns a compio thread")]

use futures::{channel::oneshot, FutureExt};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::JoinHandle,
};
use zeroship_bundle::LoadedWorker;
use zeroship_core::{app_id::AppId, typed_id, workflow_deployments::HoldScope};
use zeroship_runtime::{NativePlugin, RuntimeLimits};
use zeroship_workflow::{
    service::{
        runner::{TaskPayloadLimits, WorkerOptions, WorkflowWorker},
        schema,
        store::HostStorage,
        AppBackend, AppPolicy, HostPolicies, PolicySnapshot, WorkerIdentity, WorkflowService,
    },
    WorkflowServiceError,
};
use zeroship_workflow_manager::deployments::DeploymentHolds;
use zeroship_workflow_v8::{AppRuntimeLoader, V8TaskExecutor, WorkflowBinding};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalConfig {
    pub max_archive_bytes: usize,
    pub max_source_bytes: usize,
    pub worker: WorkerOptions,
    pub payloads: TaskPayloadLimits,
}
impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            max_archive_bytes: zeroship_bundle::MAX_COMPRESSED_BYTES,
            max_source_bytes: 32 * 1024 * 1024,
            worker: WorkerOptions::default(),
            payloads: TaskPayloadLimits::default(),
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
        if self.max_archive_bytes == 0
            || self.max_archive_bytes > zeroship_bundle::MAX_COMPRESSED_BYTES
            || self.max_source_bytes == 0
        {
            return Err("invalid local workflow limits".into());
        }
        self.payloads
            .validate()
            .map_err(|error| error.to_string())?;
        Ok(self)
    }
}

pub struct LocalHost {
    pub app: AppId,
    pub binding: WorkflowBinding,
    pub executable: Option<LoadedWorker>,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
    stopping: Arc<AtomicBool>,
}
impl LocalHost {
    pub fn start(
        root: &Path,
        app: AppId,
        config: LocalConfig,
        deployment: Option<PathBuf>,
        storage: HostStorage,
        env_vars: HashMap<String, String>,
        peers: Vec<Arc<dyn NativePlugin>>,
        limits: RuntimeLimits,
    ) -> Result<Self, String> {
        let config = config.validate()?;
        let deployment = crate::deployment::AppDeployment::new(root, deployment.as_deref())?;
        let worker_app = app.clone();
        let (ready, receive) = std::sync::mpsc::sync_channel(1);
        let (stop, stopped) = oneshot::channel();
        let stopping = Arc::new(AtomicBool::new(false));
        let worker_stopping = stopping.clone();
        zeroship_runtime::init_v8();
        let thread = std::thread::Builder::new()
            .name("workflow-worker".into())
            .spawn(move || {
                let runtime = match compio::runtime::Runtime::new() {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready.send(Err(error.to_string()));
                        return;
                    }
                };
                runtime.block_on(async move {
                    let initialized = initialize(
                        &config,
                        &deployment,
                        &worker_app,
                        storage,
                        env_vars,
                        peers,
                        limits,
                    )
                    .await;
                    let (backend, mut worker, installed) = match initialized {
                        Ok(host) => host,
                        Err(error) => {
                            let _ = ready.send(Err(error.to_string()));
                            return;
                        }
                    };
                    let executable = installed.map(|app| app.executable.into_executable());
                    if ready.send(Ok((backend, executable))).is_err() {
                        return;
                    }
                    let _liveness = WorkerLiveness(worker_stopping);
                    worker.run_until(stopped.map(|_| ())).await;
                });
            })
            .map_err(|error| format!("start workflow worker: {error}"))?;
        let (backend, executable) = match receive.recv() {
            Ok(Ok(backend)) => backend,
            outcome => {
                let _ = thread.join();
                return Err(match outcome {
                    Ok(Err(error)) => error,
                    _ => "workflow worker stopped during startup".into(),
                });
            }
        };
        Ok(Self {
            app,
            binding: WorkflowBinding::service(backend),
            executable,
            stop: Some(stop),
            thread: Some(thread),
            stopping,
        })
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

// A request server must not keep accepting durable work after its worker dies.
struct WorkerLiveness(Arc<AtomicBool>);
impl Drop for WorkerLiveness {
    fn drop(&mut self) {
        if !self.0.load(Ordering::Acquire) {
            eprintln!("[zeroship] workflow worker stopped unexpectedly");
            std::process::exit(1);
        }
    }
}

async fn initialize(
    config: &LocalConfig,
    deployment: &crate::deployment::AppDeployment,
    app: &AppId,
    storage: HostStorage,
    env_vars: HashMap<String, String>,
    peers: Vec<Arc<dyn NativePlugin>>,
    limits: RuntimeLimits,
) -> Result<
    (
        AppBackend,
        WorkflowWorker,
        Option<crate::deployment::LoadedApp>,
    ),
    WorkflowServiceError,
> {
    let store = storage.open().await?;
    schema::initialize_local(&store).await?;
    let storage = storage.objects;
    let catalog = deployment.catalog().await?;
    let client = crate::deployment::LocalDeploymentHolds::new(
        catalog.clone(),
        HoldScope::for_app(app.clone()),
    );
    let service = WorkflowService::open(Rc::new(store), Arc::new(HostPolicies::default()))
        .await?
        .with_payload_storage(storage)?
        .with_deployments(
            deployment
                .artifacts(config.max_source_bytes)?
                .with_hold_client(Rc::new(client)),
        );
    service
        .register_app(
            app,
            PolicySnapshot::configuration(
                1.try_into().expect("initial host policy revision"),
                AppPolicy::default(),
            )?,
        )
        .await?;
    let installed = install_bundle(config, deployment, &catalog, &service, app).await?;
    let backend = service
        .for_app(app.clone())
        .into_backend(config.payloads.max_payload_bytes)?;
    let tasks = Rc::new(service.tasks(WorkerIdentity::new(typed_id::generate("wkr"))?));
    let env = zeroship_runtime::serve::app_env_from_prefixed_vars(&env_vars);
    let loader = Rc::new(AppRuntimeLoader::new(
        backend.clone(),
        env_vars,
        env,
        peers,
        limits,
    )?);
    let executor = Rc::new(V8TaskExecutor::new(loader, tasks.clone(), config.payloads)?);
    let worker = WorkflowWorker::new(tasks, executor, config.worker)?;
    Ok((backend, worker, installed))
}

async fn install_bundle(
    config: &LocalConfig,
    deployment: &crate::deployment::AppDeployment,
    catalog: &DeploymentHolds,
    service: &WorkflowService,
    app: &AppId,
) -> Result<Option<crate::deployment::LoadedApp>, WorkflowServiceError> {
    let Some(installed) = deployment
        .load(
            app,
            catalog,
            config.max_archive_bytes,
            config.max_source_bytes,
        )
        .await?
    else {
        return Ok(None);
    };
    let registration = &installed.registration;
    service.activate_deploy(app, registration).await?;
    Ok(Some(installed))
}

#[cfg(test)]
mod tests;
