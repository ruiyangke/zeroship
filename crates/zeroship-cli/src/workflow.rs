//! Customer-owned local workflow host, independent of HTTP request isolates.

#![expect(clippy::future_not_send, reason = "the worker owns a compio thread")]

use compio::io::AsyncReadAtExt;
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
    time::Duration,
};
use zeroship_bundle::{BlobStore, LocalDiskBlobStore, Manifest};
use zeroship_core::{app_id::AppId, typed_id};
use zeroship_runtime::{NativePlugin, RuntimeLimits};
use zeroship_storage::{LocalFs, StorageStore};
use zeroship_workflow::{
    service::{
        runner::{TaskPayloadLimits, WorkerOptions, WorkflowWorker},
        schema,
        store::SqliteStore,
        AppBackend, AppPolicy, BundleExecutable, HostPolicies, PolicySnapshot, SnapshotStore,
        WorkerIdentity, WorkflowService,
    },
    WorkflowServiceError,
};
use zeroship_workflow_v8::{AppRuntimeLoader, V8TaskExecutor, WorkflowBinding};

mod reset;
mod state;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalConfig {
    pub journal: PathBuf,
    pub objects: PathBuf,
    pub bundle: Option<PathBuf>,
    pub max_archive_bytes: usize,
    pub max_source_bytes: usize,
    pub max_snapshot_bytes: usize,
    pub bundle_poll_ms: u64,
    pub worker: WorkerOptions,
    pub payloads: TaskPayloadLimits,
}
impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            journal: ".zeroship/workflows.sqlite".into(),
            objects: ".zeroship/workflow-objects".into(),
            bundle: None,
            max_archive_bytes: zeroship_bundle::MAX_COMPRESSED_BYTES,
            max_source_bytes: 32 * 1024 * 1024,
            max_snapshot_bytes: 64 * 1024 * 1024,
            bundle_poll_ms: 250,
            worker: WorkerOptions::default(),
            payloads: TaskPayloadLimits::default(),
        }
    }
}
pub fn config_from_args(args: &[String]) -> Result<LocalConfig, String> {
    let path = crate::parse_flag(args, "--workflow-config").map(PathBuf::from);
    let mut config = LocalConfig::read(path.as_deref())?;
    if let Some(path) = zeroship_core::declared_env_os!(
        cli,
        "ZEROSHIP_WORKFLOW_SQLITE_PATH",
        crate::ZeroshipCliConsumer
    ) {
        config.journal = path.into();
    }
    if let Some(path) = crate::parse_flag(args, "--workflow-bundle") {
        config.bundle = Some(path.into());
    }
    Ok(config)
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
            || self.bundle_poll_ms == 0
        {
            return Err("invalid local workflow limits".into());
        }
        self.payloads
            .validate()
            .map_err(|error| error.to_string())?;
        self.journal = root.join(self.journal);
        self.objects = root.join(self.objects);
        self.bundle = self.bundle.map(|path| root.join(path));
        Ok(self)
    }
}

pub struct LocalHost {
    pub app: AppId,
    pub binding: WorkflowBinding,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
    stopping: Arc<AtomicBool>,
    _state: state::StateLock,
}
impl LocalHost {
    pub fn start(
        root: &Path,
        config: LocalConfig,
        env_vars: HashMap<String, String>,
        peers: Vec<Arc<dyn NativePlugin>>,
        limits: RuntimeLimits,
    ) -> Result<Self, String> {
        let mut config = config.resolve(root)?;
        let app = project_identity(root)?;
        let paths = state::StatePaths::new(root, &config, &app)?;
        let state = paths.lock(false)?;
        paths.ensure_ready()?;
        config.journal = paths.journal;
        config.objects = paths.objects;
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
                    let initialized =
                        initialize(&config, &worker_app, env_vars, peers, limits).await;
                    let (service, backend, mut worker, archive_hash) = match initialized {
                        Ok(host) => host,
                        Err(error) => {
                            let _ = ready.send(Err(error.to_string()));
                            return;
                        }
                    };
                    if ready.send(Ok(backend)).is_err() {
                        return;
                    }
                    let _liveness = WorkerLiveness(worker_stopping);
                    let shutdown = stopped.map(|_| ()).boxed_local().shared();
                    futures::join!(worker.run_until(shutdown.clone()), async {
                        let updates = watch_bundle(&config, &service, &worker_app, archive_hash)
                            .boxed_local();
                        let _ = futures::future::select(shutdown, updates).await;
                    });
                });
            })
            .map_err(|error| format!("start workflow worker: {error}"))?;
        let backend = match receive.recv() {
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
            stop: Some(stop),
            thread: Some(thread),
            stopping,
            _state: state,
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

pub fn command(args: &[String]) -> Result<(), String> {
    const USAGE: &str =
        "Usage: zeroship workflows reset [--workflow-config=PATH] (local workflow state only)";
    if args.get(2).map(String::as_str) != Some("reset") {
        return Err(USAGE.into());
    }
    let mut arguments = args.iter().skip(3);
    let mut configured = false;
    while let Some(argument) = arguments.next() {
        if configured {
            return Err(format!("unexpected reset argument: {argument}; {USAGE}"));
        }
        if argument == "--workflow-config" {
            arguments
                .next()
                .filter(|value| !value.is_empty() && !value.starts_with("--"))
                .ok_or("--workflow-config requires a path")?;
        } else if let Some(value) = argument.strip_prefix("--workflow-config=") {
            if value.is_empty() {
                return Err("--workflow-config requires a path".into());
            }
        } else {
            return Err(format!("unexpected reset argument: {argument}; {USAGE}"));
        }
        configured = true;
    }
    reset::reset(
        &std::env::current_dir().map_err(|error| error.to_string())?,
        config_from_args(args)?,
    )?;
    eprintln!("[zeroship] local workflow state reset; project identity and other stores preserved");
    Ok(())
}

async fn initialize(
    config: &LocalConfig,
    app: &AppId,
    env_vars: HashMap<String, String>,
    peers: Vec<Arc<dyn NativePlugin>>,
    limits: RuntimeLimits,
) -> Result<(WorkflowService, AppBackend, WorkflowWorker, Option<String>), WorkflowServiceError> {
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
    let archive_hash = install_bundle(config, &service, app, None).await?;
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
    Ok((service, backend, worker, archive_hash))
}

async fn install_bundle(
    config: &LocalConfig,
    service: &WorkflowService,
    app: &AppId,
    previous: Option<&str>,
) -> Result<Option<String>, WorkflowServiceError> {
    let Some(path) = &config.bundle else {
        return Ok(None);
    };
    let file = compio::fs::File::open(path)
        .await
        .map_err(|_| artifact_unavailable())?;
    let size = usize::try_from(
        file.metadata()
            .await
            .map_err(|_| artifact_unavailable())?
            .len(),
    )
    .map_err(|_| WorkflowServiceError::PayloadTooLarge)?;
    if size > config.max_archive_bytes {
        return Err(WorkflowServiceError::PayloadTooLarge);
    }
    let (result, archive) = file.read_exact_at(vec![0; size], 0).await.into();
    result.map_err(|_| artifact_unavailable())?;
    let hash = zeroship_bundle::sha256_hex(&archive);
    if previous == Some(hash.as_str()) {
        return Ok(Some(hash));
    }
    let directory = tempfile::tempdir().map_err(|_| artifact_unavailable())?;
    let blobs: Arc<dyn BlobStore> = Arc::new(
        LocalDiskBlobStore::new(directory.path().to_path_buf())
            .map_err(|_| artifact_unavailable())?,
    );
    let ingested = zeroship_bundle::ingest(&blobs, &app.uuid(), &archive)
        .await
        .map_err(|_| artifact_unavailable())?;
    let manifest: Manifest =
        serde_json::from_str(&ingested.manifest_json).map_err(|_| artifact_unavailable())?;
    let executable =
        BundleExecutable::load(&manifest, blobs.as_ref(), config.max_source_bytes).await?;
    let registration = service
        .deployment_by_hash(app, executable.content_hash())
        .await?
        .unwrap_or_else(|| {
            executable.registration(typed_id::generate("dep"), executable.content_hash().into())
        });
    service
        .activate_deploy(app, &registration, executable.snapshot())
        .await?;
    Ok(Some(hash))
}

async fn watch_bundle(
    config: &LocalConfig,
    service: &WorkflowService,
    app: &AppId,
    mut hash: Option<String>,
) {
    let mut error_code = None;
    loop {
        compio::time::sleep(Duration::from_millis(config.bundle_poll_ms)).await;
        match install_bundle(config, service, app, hash.as_deref()).await {
            Ok(current) => {
                hash = current;
                error_code = None;
            }
            Err(error) => {
                if error_code != Some(error.code()) {
                    eprintln!("[zeroship] workflow bundle update failed ({}); retained workflows remain available", error.code());
                    error_code = Some(error.code());
                }
            }
        }
    }
}

fn artifact_unavailable() -> WorkflowServiceError {
    WorkflowServiceError::Unavailable("workflow bundle could not be read or validated".into())
}

#[cfg(test)]
mod tests;
