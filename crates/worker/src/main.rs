mod handler;
mod sync;
mod cache;
mod metrics;
mod logs;

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use clap::Parser;
use ntex::web;
use zeroship_core::config::{
    env_is_exact, load_overlay_or_exit, require_unless_dev, resolve_observability,
};
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_runtime::init::init_v8;

use crate::sync::{SharedEnvs, SharedVersions};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// zeroship worker startup configuration.
#[derive(Debug, Parser)]
#[command(name = "zeroship-worker")]
struct WorkerCli {
    /// HTTP listen port.
    #[arg(long, env = "WORKER_PORT", default_value_t = 8080)]
    port: u16,

    /// Number of ntex worker threads.
    #[arg(long = "worker-threads", env = "WORKER_THREADS")]
    worker_threads: Option<usize>,

    /// Control-plane API base URL.
    #[arg(long = "control", env = "CONTROL_URL", default_value = "http://localhost:9090")]
    control: String,

    /// Admin/control API shared secret.
    #[arg(long = "control-key", env = "CONTROL_KEY", default_value = "", hide_env_values = true)]
    control_key: String,

    /// Allow explicitly insecure local development startup.
    #[arg(long = "dev-insecure", action = clap::ArgAction::SetTrue)]
    dev_insecure: bool,

    /// Environment half of `--dev-insecure`; only `1` is truthy.
    #[arg(skip = env_is_exact("ZEROSHIP_DEV_INSECURE", "1"))]
    dev_insecure_env: bool,

    // No --trust-proxy: the worker has no client-facing IP logic.

    /// Maximum number of cached app isolates.
    #[arg(long = "max-isolates", env = "MAX_ISOLATES", default_value = "200")]
    max_isolates: usize,

    /// Control-plane polling interval in seconds.
    #[arg(long = "poll-interval", env = "POLL_INTERVAL", default_value = "5")]
    poll_interval: u64,

    /// PostgreSQL DSN for runtime env/db state.
    #[arg(long = "db", env = "DATABASE_URL", default_value = "")]
    db: String,

    /// Shared secret for gateway dispatch endpoints.
    #[arg(long = "worker-key", env = "WORKER_KEY", default_value = "", hide_env_values = true)]
    worker_key: String,

    /// Shutdown drain timeout in seconds.
    #[arg(long = "shutdown-timeout", env = "SHUTDOWN_TIMEOUT", default_value = "30")]
    shutdown_timeout: u64,

    /// Root directory for content-addressed deploy blobs.
    #[arg(long = "blob-store", env = "BLOB_STORE", default_value = "./bundles")]
    blob_store: String,

    /// HTTP bind host.
    #[arg(long = "bind", env = "WORKER_BIND", default_value = "127.0.0.1")]
    bind: String,

    /// Optional Unix domain socket path.
    #[arg(long = "socket", env = "WORKER_SOCKET", default_value = "")]
    socket: String,

    /// Optional shared config overlay path.
    #[arg(long = "config", env = "ZEROSHIP_CONFIG")]
    config_path: Option<PathBuf>,

    /// Validate config (CLI + overlay + guards) and print the resolved non-secret config, then exit without starting the server.
    #[arg(long = "check-config")]
    check_config: bool,

    /// Observability CLI/env overrides.
    #[command(flatten)]
    obs: zeroship_core::config::ObservabilityFlags,
}

impl WorkerCli {
    fn insecure_dev(&self) -> bool {
        self.dev_insecure || self.dev_insecure_env
    }
}

fn resolve_worker_threads(worker_threads: Option<usize>) -> usize {
    worker_threads.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    })
}

#[allow(missing_debug_implementations)]
pub struct WorkerConfig {
    pub control_url: String,
    pub control_key: String,
    pub db_url: Option<String>,
    pub max_isolates: usize,
    pub poll_interval_secs: u64,
    /// Shared secret with the gateway. When non-empty, every /dispatch call
    /// must present `Authorization: Bearer <worker_key>` and the
    /// `ZeroShip-User` header must carry a valid HMAC. When empty, both
    /// checks are bypassed — dev mode only.
    pub worker_key: String,
    /// Seconds ntex will wait after SIGTERM for in-flight requests to
    /// finish. Requests still running after the deadline are dropped and
    /// the worker exits. `0` means "wait forever" — useful locally but
    /// fatal for Kubernetes preemption (which will SIGKILL after its own
    /// `terminationGracePeriodSeconds`).
    pub shutdown_timeout_secs: u64,
    /// Content-addressed blob store. The worker fetches bundle bytes
    /// here directly instead of round-tripping through the control
    /// plane. In dev and single-host production the gateway, control,
    /// and worker all point at the same path; in multi-host production
    /// each crate keeps its own `Arc` over a shared remote backend
    /// (for example S3 with an on-disk LRU).
    pub blob_store: Arc<dyn BlobStore>,
}

fn main() -> std::io::Result<()> {
    let cli = WorkerCli::parse();
    let file = load_overlay_or_exit(cli.config_path.as_deref(), "worker");
    let (filter, format) = resolve_observability(
        &cli.obs,
        &file.observability,
        "info,zeroship_worker=debug,zeroship_runtime=info",
    );
    zeroship_core::observability::init_tracing_with(&filter, format.as_deref());

    let insecure_dev = cli.insecure_dev();
    let port = cli.port;
    let workers_count = resolve_worker_threads(cli.worker_threads);
    let control_url = cli.control;
    let control_key = cli.control_key;
    let max_isolates = cli.max_isolates;
    let poll_interval = cli.poll_interval;
    let db_url = cli.db;
    let worker_key = cli.worker_key;
    let shutdown_timeout = cli.shutdown_timeout;
    let blob_store_root = cli.blob_store;
    let bind_host = cli.bind;
    let socket_path = cli.socket;

    if let Err(message) =
        require_unless_dev("CONTROL_KEY / --control-key", &control_key, insecure_dev)
    {
        tracing::error!(error = %message, "worker: refusing to start without control key");
        std::process::exit(1);
    }

    if worker_key.is_empty() {
        if bind_host == "127.0.0.1" || bind_host == "::1" || bind_host == "localhost" {
            tracing::warn!(
                bind = %bind_host,
                "WORKER_KEY not set — dispatch endpoints unauthenticated (loopback-only, dev mode)"
            );
        } else {
            tracing::error!(
                bind = %bind_host,
                "refusing to bind non-loopback without WORKER_KEY — would expose unauthenticated code execution"
            );
            std::process::exit(1);
        }
    }

    if cli.check_config {
        println!("check-config: bind = {bind_host}");
        println!("check-config: port = {port}");
        println!("check-config: control_url = {control_url}");
        println!("check-config: worker_threads = {workers_count}");
        println!("check-config: max_isolates = {max_isolates}");
        println!("check-config: poll_interval_secs = {poll_interval}");
        println!("check-config: shutdown_timeout_secs = {shutdown_timeout}");
        println!("check-config: log_filter = {filter}");
        println!(
            "check-config: log_format = {}",
            format.as_deref().unwrap_or("auto")
        );
        println!("check-config: insecure_dev = {insecure_dev}");
        println!("check-config: blob_store = {blob_store_root}");
        println!("check-config: socket_configured = {}", !socket_path.is_empty());
        println!("check-config: db_configured = {}", !db_url.is_empty());
        return Ok(());
    }

    ntex::rt::System::build()
        .name("zeroship-worker")
        .build(ntex::rt::DefaultRuntime)
        .block_on(async move {
    init_v8();

    let blob_store: Arc<dyn BlobStore> = Arc::new(
        LocalDiskBlobStore::new(PathBuf::from(&blob_store_root))
            .expect("failed to initialise blob store"),
    );
    tracing::info!(blob_store_root = %blob_store_root, "worker blob store configured");

    let config = Arc::new(WorkerConfig {
        control_url,
        control_key,
        db_url: if db_url.is_empty() { None } else { Some(db_url) },
        max_isolates,
        poll_interval_secs: poll_interval,
        worker_key,
        shutdown_timeout_secs: shutdown_timeout,
        blob_store,
    });

    let bind_addr = format!("{bind_host}:{port}");

    // Shared version snapshot populated by a SINGLE process-wide poller and
    // observed by every ntex worker thread's reconcile loop. Previously every
    // thread made its own HTTP poll — this multiplied control-plane traffic
    // by `workers_count` with no benefit.
    let shared_versions: SharedVersions = Arc::new(RwLock::new(None));
    // Process-wide env cache (single source of truth across all ntex
    // worker threads). Reconcile loops read+write through it, the
    // dispatch handler reads under a brief read lock + Arc clone.
    let shared_envs: SharedEnvs = Arc::new(RwLock::new(std::collections::HashMap::new()));
    let shared_logs = logs::new_store();

    tracing::info!(
        bind = %bind_addr,
        threads = workers_count,
        max_isolates = config.max_isolates,
        shutdown_timeout_secs = config.shutdown_timeout_secs,
        "worker listening"
    );
    if !socket_path.is_empty() {
        tracing::info!(socket = %socket_path, "worker also bound to unix socket");
        // Remove stale socket file
        let _ = std::fs::remove_file(&socket_path);
    }

    // Start the single process-wide version poller BEFORE ntex spawns worker
    // threads so the shared map is already being populated when they come up.
    // Poller also GCs SharedEnvs against the current known-app set, so
    // env entries for deleted apps don't leak forever.
    sync::start_version_poller(config.clone(), shared_versions.clone(), shared_envs.clone());

    // ntex installs SIGINT/SIGTERM handlers by default; `shutdown_timeout`
    // bounds how long worker threads have to drain in-flight requests
    // before they're force-dropped. Wire our flag through.
    let shutdown_timeout_secs: u16 =
        u16::try_from(config.shutdown_timeout_secs).unwrap_or(u16::MAX);

    let mut server = web::server(async move || {
        let config = config.clone();
        let shared = shared_versions.clone();
        let envs = shared_envs.clone();
        let logs = shared_logs.clone();
        cache::init_cache(config.max_isolates, config.db_url.clone());
        // Per-thread reconcile loop — reads from the shared version map,
        // writes env into the process-wide env cache.
        sync::start_sync(config.clone(), shared, envs.clone());

        web::App::new()
            .state(config)
            .state(envs)
            .state(logs)
            .service(web::resource("/dispatch/{app_id}").route(web::post().to(handler::dispatch)))
            .service(web::resource("/logs/{app_id}").route(web::get().to(logs::get_logs)))
            .service(web::resource("/health").route(web::get().to(|| async {
                web::HttpResponse::Ok().body(r#"{"status":"ok"}"#)
            })))
            // Prometheus-text metrics. No auth — same policy as `/health`,
            // intended for intra-cluster scrapers. Expose behind a side-car
            // or ingress filter if the worker port is ever reachable from
            // outside the cluster.
            .service(web::resource("/metrics").route(web::get().to(|| async {
                web::HttpResponse::Ok()
                    .content_type("text/plain; version=0.0.4; charset=utf-8")
                    .body(metrics::render())
            })))
    })
    .workers(workers_count)
    .shutdown_timeout(ntex::time::Seconds(shutdown_timeout_secs))
    .bind(&bind_addr)?;

    // Also listen on Unix domain socket if configured
    if !socket_path.is_empty() {
        server = server.bind_uds(&socket_path)?;
    }

    // `server.run()` blocks until SIGINT/SIGTERM arrives; ntex then stops
    // accepting, waits up to `shutdown_timeout` for workers to finish
    // serving their current requests, and returns. Detached tasks
    // (fetch body readers, stream drainers) whose futures the pump is
    // polling get one last chance to run during the drain window.
    let run_result = server.run().await;
    tracing::info!("worker shutdown complete");
    run_result
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_control_key_rejects_missing_in_non_dev() {
        let err =
            require_unless_dev("CONTROL_KEY / --control-key", "", false).unwrap_err();
        assert!(err.contains("CONTROL_KEY"), "{err}");
    }

    #[test]
    fn worker_control_key_accepts_nonempty_in_non_dev() {
        assert!(require_unless_dev("CONTROL_KEY / --control-key", "secret", false).is_ok());
    }

    #[test]
    fn worker_control_key_allows_missing_in_insecure_dev() {
        assert!(require_unless_dev("CONTROL_KEY / --control-key", "", true).is_ok());
    }

    #[test]
    fn worker_threads_default_resolves_to_positive_count() {
        assert!(resolve_worker_threads(None) > 0);
        assert_eq!(resolve_worker_threads(Some(3)), 3);
    }

    #[test]
    fn worker_thread_flag_uses_unambiguous_name() {
        let cli = WorkerCli::try_parse_from([
            "zeroship-worker",
            "--worker-threads",
            "3",
            "--max-isolates",
            "200",
            "--poll-interval",
            "5",
            "--shutdown-timeout",
            "30",
        ])
        .expect("--worker-threads should parse");
        assert_eq!(cli.worker_threads, Some(3));

        let old_flag = WorkerCli::try_parse_from(["zeroship-worker", "--workers", "3"]);
        assert!(old_flag.is_err(), "--workers must not parse for worker threads");
    }

    #[test]
    fn worker_numeric_fields_reject_bad_input() {
        let err = WorkerCli::try_parse_from(["zeroship-worker", "--max-isolates", "abc"])
            .expect_err("bad max-isolates should be a clap error");
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
    }
}
