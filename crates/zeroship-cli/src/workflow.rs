//! Customer-owned local workflow host, independent of HTTP request isolates.

#![expect(clippy::future_not_send, reason = "the worker owns a compio thread")]

use futures::{channel::oneshot, FutureExt};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::JoinHandle,
};
use zeroship_core::{app_id::AppId, typed_id};
use zeroship_runtime::{NativePlugin, RuntimeLimits};
use zeroship_storage::{LocalFs, StorageStore};
use zeroship_workflow::{
    service::{
        runner::{TaskPayloadLimits, WorkerOptions, WorkflowWorker},
        schema,
        store::SqliteStore,
        AppBackend, AppPolicy, ExecutableSnapshot, HostPolicies, PolicySnapshot, SnapshotStore,
        WorkerIdentity, WorkflowService,
    },
    WorkflowServiceError,
};
use zeroship_workflow_v8::{AppRuntimeLoader, V8TaskExecutor, WorkflowBinding};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalConfig {
    pub journal: PathBuf,
    pub objects: PathBuf,
    pub max_archive_bytes: usize,
    pub max_source_bytes: usize,
    pub max_snapshot_bytes: usize,
    pub worker: WorkerOptions,
    pub payloads: TaskPayloadLimits,
}
impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            journal: ".zeroship/workflows.sqlite".into(),
            objects: ".zeroship/workflow-objects".into(),
            max_archive_bytes: zeroship_bundle::MAX_COMPRESSED_BYTES,
            max_source_bytes: 32 * 1024 * 1024,
            max_snapshot_bytes: 64 * 1024 * 1024,
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

    fn resolve(mut self, root: &Path) -> Result<Self, String> {
        if self.max_archive_bytes == 0
            || self.max_archive_bytes > zeroship_bundle::MAX_COMPRESSED_BYTES
            || self.max_source_bytes == 0
            || self.max_snapshot_bytes == 0
        {
            return Err("invalid local workflow limits".into());
        }
        self.payloads
            .validate()
            .map_err(|error| error.to_string())?;
        self.journal = root.join(self.journal);
        self.objects = root.join(self.objects);
        Ok(self)
    }
}

pub struct LocalHost {
    pub app: AppId,
    pub binding: WorkflowBinding,
    pub executable: Option<ExecutableSnapshot>,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
    stopping: Arc<AtomicBool>,
}
impl LocalHost {
    pub fn start(
        root: &Path,
        config: LocalConfig,
        deployment: Option<PathBuf>,
        env_vars: HashMap<String, String>,
        peers: Vec<Arc<dyn NativePlugin>>,
        limits: RuntimeLimits,
    ) -> Result<Self, String> {
        let config = config.resolve(root)?;
        let app = project_identity(root)?;
        let deployment = deployment
            .map(|path| crate::deployment::AppDeployment::new(root, &path))
            .transpose()?;
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
                        deployment.as_ref(),
                        &worker_app,
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
                    let executable = installed.map(|app| app.executable.snapshot().clone());
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

fn project_identity(root: &Path) -> Result<AppId, String> {
    let state = root.join(".zeroship");
    std::fs::create_dir_all(&state).map_err(|error| format!("create project state: {error}"))?;
    let path = state.join("app-id");
    if !path.exists() {
        let app = AppId::mint();
        let mut pending =
            tempfile::NamedTempFile::new_in(&state).map_err(|error| error.to_string())?;
        pending
            .write_all(app.as_str().as_bytes())
            .map_err(|error| error.to_string())?;
        pending
            .as_file()
            .sync_all()
            .map_err(|error| error.to_string())?;
        match pending.persist_noclobber(&path) {
            Ok(_) => std::fs::File::open(&state)
                .and_then(|dir| dir.sync_all())
                .map_err(|error| error.to_string())?,
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    read_project_identity(root)
}

fn read_project_identity(root: &Path) -> Result<AppId, String> {
    let encoded = std::fs::read_to_string(root.join(".zeroship/app-id"))
        .map_err(|error| format!("read persisted project identity: {error}"))?;
    AppId::parse(encoded.trim()).map_err(|_| "invalid persisted project app identity".into())
}

async fn initialize(
    config: &LocalConfig,
    deployment: Option<&crate::deployment::AppDeployment>,
    app: &AppId,
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
    schema::initialize_sqlite(&config.journal)?;
    let storage = StorageStore::from_backend(Arc::new(LocalFs::new(&config.objects)));
    let service = WorkflowService::open(
        Arc::new(SqliteStore::new(&config.journal)),
        Arc::new(HostPolicies::default()),
    )
    .await?
    .with_payload_storage(storage.clone())?
    .with_snapshots(SnapshotStore::new(&storage, config.max_snapshot_bytes)?);
    service
        .register_app(
            app,
            PolicySnapshot::configuration(
                1.try_into().expect("initial host policy revision"),
                AppPolicy::default(),
            )?,
        )
        .await?;
    let installed = install_bundle(config, deployment, &service, app).await?;
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
    deployment: Option<&crate::deployment::AppDeployment>,
    service: &WorkflowService,
    app: &AppId,
) -> Result<Option<crate::deployment::LoadedApp>, WorkflowServiceError> {
    let Some(deployment) = deployment else {
        return Ok(None);
    };
    let installed = deployment
        .load(app, config.max_archive_bytes, config.max_source_bytes)
        .await?;
    let executable = &installed.executable;
    let registration = service
        .deployment_by_hash(app, &installed.deploy_hash)
        .await?
        .unwrap_or_else(|| {
            executable.registration(typed_id::generate("dep"), installed.deploy_hash.clone())
        });
    service
        .activate_deploy(app, &registration, executable.snapshot())
        .await?;
    Ok(Some(installed))
}

#[cfg(test)]
mod tests;
