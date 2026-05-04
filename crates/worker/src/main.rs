mod handler;
mod sync;
mod cache;
mod metrics;

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use ntex::web;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_runtime::init::init_v8;

use crate::sync::{SharedEnvs, SharedVersions};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

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
    /// Content-addressed blob store. Phase 4b of the artifact rollout —
    /// the worker fetches worker-bundle bytes here directly instead of
    /// round-tripping through the control plane. In dev / single-host
    /// prod the gateway, control, and worker all point at the same path;
    /// in multi-host prod each crate keeps its own `Arc` over a shared
    /// remote backend (S3 + on-disk LRU, later phase).
    pub blob_store: Arc<dyn BlobStore>,
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    init_v8();

    let args: Vec<String> = std::env::args().collect();
    let port = arg_or_env(&args, "--port", "WORKER_PORT", "8080");
    let workers = arg_or_env(
        &args,
        "--workers",
        "WORKER_THREADS",
        &std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .to_string(),
    );
    let control_url = arg_or_env(&args, "--control", "CONTROL_URL", "http://localhost:9090");
    let control_key = arg_or_env(&args, "--control-key", "CONTROL_KEY", "");
    let max_isolates = arg_or_env(&args, "--max-isolates", "MAX_ISOLATES", "200");
    let poll_interval = arg_or_env(&args, "--poll-interval", "POLL_INTERVAL", "5");
    let db_url = arg_or_env(&args, "--db", "DATABASE_URL", "");
    let worker_key = arg_or_env(&args, "--worker-key", "WORKER_KEY", "");
    let shutdown_timeout = arg_or_env(&args, "--shutdown-timeout", "SHUTDOWN_TIMEOUT", "30");
    // Same default + flag name as zeroship-control / zeroship-gate so a
    // single-host dev box can point all three at one shared volume.
    let blob_store_root = arg_or_env(&args, "--blob-store", "BLOB_STORE", "./bundles");
    // Default to loopback. Operators must explicitly opt into a public bind
    // (--bind 0.0.0.0) after ensuring WORKER_KEY is set; without the shared
    // secret, any network-reachable caller can impersonate users and run code.
    let bind_host = arg_or_env(&args, "--bind", "WORKER_BIND", "127.0.0.1");

    if worker_key.is_empty() {
        if bind_host == "127.0.0.1" || bind_host == "::1" || bind_host == "localhost" {
            eprintln!(
                "[zeroship-worker] WARNING: WORKER_KEY not set — dispatch endpoints are unauthenticated (loopback-only, dev mode)"
            );
        } else {
            eprintln!(
                "[zeroship-worker] FATAL: refusing to bind non-loopback ({bind_host}) without WORKER_KEY — this would expose unauthenticated code execution"
            );
            std::process::exit(1);
        }
    }

    let blob_store: Arc<dyn BlobStore> = Arc::new(
        LocalDiskBlobStore::new(PathBuf::from(&blob_store_root))
            .expect("failed to initialise blob store"),
    );
    eprintln!("[zeroship-worker] blob store at {blob_store_root}");

    let config = Arc::new(WorkerConfig {
        control_url,
        control_key,
        db_url: if db_url.is_empty() { None } else { Some(db_url) },
        max_isolates: max_isolates.parse().unwrap_or(200),
        poll_interval_secs: poll_interval.parse().unwrap_or(5),
        worker_key,
        shutdown_timeout_secs: shutdown_timeout.parse().unwrap_or(30),
        blob_store,
    });

    let socket_path = arg_or_env(&args, "--socket", "WORKER_SOCKET", "");
    let workers_count: usize = workers.parse().unwrap_or(1);
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

    eprintln!(
        "[zeroship-worker] http://{bind_addr} ({workers_count} threads, MAX_ISOLATES={} per thread, graceful shutdown={}s)",
        config.max_isolates,
        config.shutdown_timeout_secs,
    );
    if !socket_path.is_empty() {
        eprintln!("[zeroship-worker] unix://{socket_path}");
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
        cache::init_cache(config.max_isolates, config.db_url.clone());
        // Per-thread reconcile loop — reads from the shared version map,
        // writes env into the process-wide env cache.
        sync::start_sync(config.clone(), shared, envs.clone());

        web::App::new()
            .state(config)
            .state(envs)
            .service(web::resource("/dispatch/{app_id}").route(web::post().to(handler::dispatch)))
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
    eprintln!("[zeroship-worker] shutdown complete");
    run_result
}

fn arg_or_env(args: &[String], flag: &str, env_key: &str, default: &str) -> String {
    for pair in args.windows(2) {
        if pair[0] == flag {
            return pair[1].clone();
        }
    }
    std::env::var(env_key).unwrap_or_else(|_| default.to_string())
}
