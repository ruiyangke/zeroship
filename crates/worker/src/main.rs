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
    bootstrap_or_exit, parse_bool_flag, require_unless_dev, CheckConfigReport, CheckFormat,
    CheckValue,
};
use zeroship_bundle::{build_blob_store, BlobStore, StoreUrl};
use zeroship_plugin_storage::StorageBackendConfig;
use zeroship_runtime::init::init_v8;

use crate::sync::{SharedEnvs, SharedVersions};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// zeroship worker startup configuration.
///
/// No `#[derive(Debug)]`: this struct holds raw secrets (`control_key`,
/// `worker_key`, `db`) before they are consumed, and a `{:?}` would print
/// them in plaintext (S2).
#[derive(Parser)]
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
    ///
    /// `--dev-insecure` / `--dev-insecure=true` enables; `--dev-insecure=false`
    /// disables even when `ZEROSHIP_DEV_INSECURE=1` is set in the environment
    /// (CLI presence overrides env — proper `CLI > env` precedence, S1).
    #[arg(
        long = "dev-insecure",
        env = "ZEROSHIP_DEV_INSECURE",
        num_args = 0..=1,
        default_missing_value = "true",
        value_parser = parse_bool_flag
    )]
    dev_insecure: Option<bool>,

    // No --trust-proxy: the worker has no client-facing IP logic.

    /// Maximum number of cached app isolates.
    #[arg(long = "max-isolates", env = "MAX_ISOLATES", default_value = "200")]
    max_isolates: usize,

    /// Control-plane polling interval in seconds.
    #[arg(long = "poll-interval", env = "POLL_INTERVAL", default_value = "5")]
    poll_interval: u64,

    /// PostgreSQL DSN for runtime env/db state.
    #[arg(long = "db", env = "DATABASE_URL", default_value = "", hide_env_values = true)]
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

    /// Redis connection URL for the app `env.kv` namespace.
    ///
    /// Multi-node KV MUST be a SHARED store so a `set` on one worker node
    /// is visible on another — Redis is that store (the bespoke
    /// compio-redis driver; zero tokio). The URL selects single-node
    /// (`redis://host:port`) or cluster (`redis://seed/?cluster=true&seeds=...`)
    /// mode. When empty the `env.kv` namespace is absent (apps using
    /// `@zeroship/kv` then fail loudly rather than silently diverging on a
    /// per-process embedded store). The single-tenant CLI's per-process
    /// `redb` backend is deliberately NOT used here — it can't stay
    /// consistent across a worker fleet.
    #[arg(long = "kv-url", env = "ZEROSHIP_KV_URL", default_value = "", hide_env_values = true)]
    kv_url: String,

    /// Object-store location for the app `env.storage` namespace.
    ///
    /// A bare path or `file://…` selects the `LocalFs` backend; `s3://…`
    /// selects the S3 backend (S3/R2/MinIO/Spaces/B2), parsed through the
    /// same grammar as `--blob-store`. Multi-node storage MUST be shared so
    /// an object `put` on one worker node is readable on another: a `LocalFs`
    /// path is a shared volume mounted identically on every replica (the
    /// deploy-blob-store pattern); S3/R2 is inherently shared. S3 credentials
    /// resolve from the AWS env vars. When empty the `env.storage` namespace
    /// is absent.
    #[arg(long = "storage-url", env = "ZEROSHIP_STORAGE_URL", default_value = "")]
    storage_url: String,

    /// HTTP bind host.
    #[arg(long = "bind", env = "WORKER_BIND", default_value = "127.0.0.1")]
    bind: String,

    /// Optional Unix domain socket path.
    #[arg(long = "socket", env = "WORKER_SOCKET", default_value = "")]
    socket: String,

    /// Optional shared config overlay path.
    #[arg(long = "config", env = "ZEROSHIP_CONFIG")]
    config_path: Option<PathBuf>,

    /// Disable auto-discovery of the well-known overlay path; use compiled
    /// defaults even if `/etc/zeroship/zeroship.toml` exists (O5).
    #[arg(long = "no-config")]
    no_config: bool,

    /// Validate config (CLI + overlay + guards) and print the resolved non-secret config, then exit without starting the server.
    #[arg(long = "check-config")]
    check_config: bool,

    /// Output format for `--check-config`: `text` (default) or `json`.
    #[arg(long = "check-config-format", default_value = "text", value_parser = ["text", "json"])]
    check_config_format: String,

    /// Observability CLI/env overrides.
    #[command(flatten)]
    obs: zeroship_core::observability::ObservabilityFlags,
}

// Hand-written Debug that redacts the raw-secret fields (`control_key`,
// `worker_key`, `db`) so a `{:?}` never leaks credentials (S2). The derived
// Debug is intentionally NOT used.
impl std::fmt::Debug for WorkerCli {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerCli")
            .field("port", &self.port)
            .field("worker_threads", &self.worker_threads)
            .field("control", &self.control)
            .field("control_key", &"<redacted>")
            .field("dev_insecure", &self.dev_insecure)
            .field("max_isolates", &self.max_isolates)
            .field("poll_interval", &self.poll_interval)
            .field("db", &"<redacted>")
            .field("worker_key", &"<redacted>")
            .field("shutdown_timeout", &self.shutdown_timeout)
            .field("blob_store", &self.blob_store)
            // kv_url may embed `redis://user:pass@host`; redact like the DSNs.
            .field("kv_url", &"<redacted>")
            .field("storage_url", &self.storage_url)
            .field("bind", &self.bind)
            .field("socket", &self.socket)
            .field("config_path", &self.config_path)
            .field("no_config", &self.no_config)
            .field("check_config", &self.check_config)
            .field("check_config_format", &self.check_config_format)
            .field("obs", &self.obs)
            .finish()
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
    /// Redis URL for the app `env.kv` namespace. `None` ⇒ namespace absent.
    /// Shared across worker nodes — see `WorkerCli::kv_url`.
    pub kv_url: Option<String>,
    /// Object-store backend for the app `env.storage` namespace. `None` ⇒
    /// namespace absent. `LocalFs` (a shared volume across nodes) or `S3`
    /// (inherently shared) — see `WorkerCli::storage_url`.
    pub storage_backend: Option<StorageBackendConfig>,
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
    let boot = bootstrap_or_exit(
        cli.config_path.as_deref(),
        !cli.no_config,
        &cli.obs,
        "info,zeroship_worker=debug,zeroship_runtime=info",
        "worker",
    );

    // `[secrets]` overlay tier: reference-only values that back-fill any secret
    // the CLI/env leaves empty (CLI/env still wins). Clone the section once,
    // early, so later partial moves of the overlay config can't invalidate it.
    let file_secrets = boot.overlay.config.secrets.clone();

    // CLI presence overrides env: `--dev-insecure=false` disables even a stray
    // `ZEROSHIP_DEV_INSECURE=1` (S1).
    let insecure_dev = cli.dev_insecure.unwrap_or(false);
    let port = cli.port;
    let workers_count = resolve_worker_threads(cli.worker_threads);
    let control_url = cli.control;
    // Secret-reference resolution (env:/file:/vault:/awssm: indirection) with a
    // `[secrets]` overlay tier: CLI/env > `[secrets]` file (reference-only) > unset.
    // On the real boot path we resolve to the live value (side effects: env/file
    // read); under --check-config we only validate the reference FORMAT, leaving the
    // raw ref string in place so no env/file/network read happens during a dry run.
    let control_key = zeroship_core::config::obtain_secret(
        "CONTROL_KEY / --control-key",
        &cli.control_key,
        file_secrets.control_key.as_deref(),
        cli.check_config,
    );
    let max_isolates = cli.max_isolates;
    let poll_interval = cli.poll_interval;
    let db_url = zeroship_core::config::obtain_secret(
        "DATABASE_URL / --db",
        &cli.db,
        file_secrets.database_url.as_deref(),
        cli.check_config,
    );
    let worker_key = zeroship_core::config::obtain_secret(
        "WORKER_KEY / --worker-key",
        &cli.worker_key,
        file_secrets.worker_key.as_deref(),
        cli.check_config,
    );
    let shutdown_timeout = cli.shutdown_timeout;
    let blob_store_root = cli.blob_store;
    // `s3://…` → remote S3 store, bare path → local disk (dev default).
    // Validated now so a bad `s3://` URL fails fast.
    let store_url = match StoreUrl::parse(&blob_store_root) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("worker: invalid --blob-store: {e}");
            std::process::exit(2);
        }
    };
    let blob_store_is_remote = store_url.is_remote();
    // KV URL may embed credentials (`redis://user:pass@host`), so resolve it
    // through the secret indirection like the DSNs (env:/file:/vault: refs +
    // `[secrets]` overlay tier). The overlay's `kv_url` slot back-fills an
    // empty CLI/env value; CLI/env still wins.
    let kv_url = zeroship_core::config::obtain_secret(
        "ZEROSHIP_KV_URL / --kv-url",
        &cli.kv_url,
        file_secrets.kv_url.as_deref(),
        cli.check_config,
    );
    // `env.storage` backend. Empty ⇒ namespace absent. A bare path/`file://`
    // is `LocalFs`; `s3://…` is the S3 backend. Validated now (parse only —
    // S3 credentials are resolved when the plugin is built per worker thread)
    // so a malformed `s3://` URL fails fast.
    let storage_raw = cli.storage_url;
    let storage_backend = if storage_raw.is_empty() {
        None
    } else {
        // `file://` is config ergonomics for a local path; strip it so the
        // parser sees a bare path. `s3://` falls through to the S3 leg.
        let arg = storage_raw.strip_prefix("file://").unwrap_or(&storage_raw);
        match StorageBackendConfig::parse(arg) {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("worker: invalid --storage-url: {e}");
                std::process::exit(2);
            }
        }
    };
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
    } else if !cli.check_config || !zeroship_core::config::is_secret_ref(&worker_key) {
        // L6: a present-but-weak WORKER_KEY skips the empty-key loopback guard
        // above, can bind any interface, and is brute-forceable for the
        // ZeroShip-User HMAC. Hold a NON-EMPTY worker_key to the same ≥32-byte
        // strength floor as the stash key / pairwise salt (empty stays handled
        // by the dev-loopback branch above). Skipped for a secret REFERENCE
        // under --check-config (the raw ref text would wrongly fail the length
        // check); it runs on the resolved value at real boot.
        if let Err(message) =
            zeroship_core::config::validate_worker_key(&worker_key, insecure_dev)
        {
            tracing::error!(error = %message, "worker: refusing to start with unsafe WORKER_KEY");
            std::process::exit(1);
        }
    }

    if cli.check_config {
        let log_format = boot
            .log_format
            .map_or_else(|| "auto".to_string(), |f| f.to_string());
        let mut report = CheckConfigReport::new();
        report.field("bind", CheckValue::Plain(bind_host.clone()));
        report.field("port", CheckValue::Count(usize::from(port)));
        report.field(
            "config_source",
            CheckValue::Plain(boot.overlay.source.to_string()),
        );
        report.field("control_url", CheckValue::Plain(control_url.clone()));
        report.field("worker_threads", CheckValue::Count(workers_count));
        report.field("max_isolates", CheckValue::Count(max_isolates));
        report.field(
            "poll_interval_secs",
            CheckValue::Count(usize::try_from(poll_interval).unwrap_or(usize::MAX)),
        );
        report.field(
            "shutdown_timeout_secs",
            CheckValue::Count(usize::try_from(shutdown_timeout).unwrap_or(usize::MAX)),
        );
        report.field("log_filter", CheckValue::Plain(boot.log_filter.clone()));
        report.field("log_format", CheckValue::Plain(log_format));
        report.field("insecure_dev", CheckValue::Flag(insecure_dev));
        report.field("blob_store", CheckValue::Plain(blob_store_root.clone()));
        report.field("blob_store_remote", CheckValue::Flag(blob_store_is_remote));
        report.field("socket_configured", CheckValue::Flag(!socket_path.is_empty()));
        report.field("db_configured", CheckValue::Flag(!db_url.is_empty()));
        // Surface the kernel-namespace wiring without leaking the KV URL
        // (it may carry credentials) — booleans only, like `db_configured`.
        report.field("kv_configured", CheckValue::Flag(!kv_url.is_empty()));
        report.field(
            "storage_configured",
            CheckValue::Flag(storage_backend.is_some()),
        );
        report.field(
            "storage_kind",
            CheckValue::Plain(
                storage_backend
                    .as_ref()
                    .map_or("absent", StorageBackendConfig::kind)
                    .to_string(),
            ),
        );
        report.field(
            "storage_remote",
            CheckValue::Flag(storage_backend.as_ref().is_some_and(StorageBackendConfig::is_remote)),
        );

        let fmt = if cli.check_config_format == "json" {
            CheckFormat::Json
        } else {
            CheckFormat::Text
        };
        report.emit(fmt);
        return Ok(());
    }

    ntex::rt::System::build()
        .name("zeroship-worker")
        .build(ntex::rt::DefaultRuntime)
        .block_on(async move {
    init_v8();

    let blob_store: Arc<dyn BlobStore> =
        build_blob_store(&store_url).expect("failed to initialise blob store");
    tracing::info!(
        blob_store_root = %blob_store_root,
        blob_store_remote = blob_store_is_remote,
        "worker blob store configured"
    );

    let kv_url_opt = if kv_url.is_empty() { None } else { Some(kv_url) };
    // Resolve S3 credentials NOW (fail fast) for a remote storage backend, so
    // a misconfigured worker refuses to start rather than degrading the
    // namespace silently per thread.
    if let Some(cfg) = &storage_backend {
        if cfg.is_remote() {
            if let Err(e) = zeroship_plugin_storage::build_backend(cfg) {
                eprintln!("worker: --storage-url s3 backend init failed: {e}");
                std::process::exit(1);
            }
        }
    }
    // Announce the resolved app-kernel namespace surface so a deployment
    // that forgot to wire kv/storage is visible in the worker's boot log
    // (rather than only surfacing as a runtime "env.kv is undefined" in a
    // creator app). `auth` is always on; `db`/`kv`/`storage` track config.
    tracing::info!(
        db = !db_url.is_empty(),
        kv = kv_url_opt.is_some(),
        storage = storage_backend.is_some(),
        storage_kind = storage_backend.as_ref().map_or("absent", StorageBackendConfig::kind),
        auth = true,
        "worker app-kernel namespaces"
    );

    let config = Arc::new(WorkerConfig {
        control_url,
        control_key,
        db_url: if db_url.is_empty() { None } else { Some(db_url) },
        kv_url: kv_url_opt,
        storage_backend,
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

    // ── Metering infrastructure ──────────────────────────────────────────
    // ONE process-wide meter, shared with every ntex worker thread's
    // `create_plugins` (via KernelConfig) AND the single flush task spawned
    // here. Metering is infrastructure: there is NO `env.meter` creator API.
    // The worker emits the five platform counters (`record_request`) and the
    // db/kv/storage primitives emit raw usage metrics at their op boundary —
    // all into this instance. The flush task drains it every ~10s and POSTs a
    // `UsageReport` (idempotent, dedup'd on worker_id+sequence) to control.
    let meter = Arc::new(zeroship_metering::Meter::new());
    // Stable-ish worker identity for the dedup key. Prefer $HOSTNAME (stable
    // across restarts in k8s/compose); else bind addr; else a random id. A
    // restart with a fresh id simply forgoes cross-restart dedup — never a
    // false dedup, so it's safe.
    let worker_id = std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("{bind_addr}-{}", uuid::Uuid::new_v4()));
    zeroship_metering::spawn_flush_task(
        Arc::clone(&meter),
        zeroship_metering::FlushConfig {
            control_url: config.control_url.clone(),
            control_key: config.control_key.clone(),
            worker_id: worker_id.clone(),
            interval: zeroship_metering::DEFAULT_FLUSH_INTERVAL,
        },
    );
    tracing::info!(worker_id = %worker_id, "metering flush task started");

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
        cache::init_cache(
            config.max_isolates,
            cache::KernelConfig {
                db_url: config.db_url.clone(),
                kv_url: config.kv_url.clone(),
                storage_backend: config.storage_backend.clone(),
                // The ONE process-wide meter the flush task drains.
                meter: Arc::clone(&meter),
            },
        );
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

    // Serialise the tests that mutate the shared `ZEROSHIP_DEV_INSECURE`
    // process environment so they don't race each other.
    static DEV_INSECURE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// S1 regression: an explicit `--dev-insecure=false` on the CLI overrides a
    /// stray `ZEROSHIP_DEV_INSECURE=1` in the environment. Pre-fix the env half
    /// was a separate `SetTrue`-OR-`env_is_exact` pair, so env always won and the
    /// CLI could not turn insecure mode back off. Now both share one
    /// `Option<bool>` field and CLI presence wins.
    #[test]
    fn worker_dev_insecure_cli_false_overrides_env_one() {
        let _guard = DEV_INSECURE_ENV_LOCK.lock().unwrap();
        std::env::set_var("ZEROSHIP_DEV_INSECURE", "1");

        let cli = WorkerCli::try_parse_from(["zeroship-worker", "--dev-insecure=false"])
            .expect("--dev-insecure=false should parse");
        // CLI presence overrides the env var.
        assert_eq!(cli.dev_insecure, Some(false));
        assert!(
            !cli.dev_insecure.unwrap_or(false),
            "resolved insecure_dev must be false"
        );

        // Sanity: with no CLI flag the env var still flows through to `Some(true)`.
        let env_only = WorkerCli::try_parse_from(["zeroship-worker"])
            .expect("env-only parse should succeed");
        assert_eq!(env_only.dev_insecure, Some(true));

        std::env::remove_var("ZEROSHIP_DEV_INSECURE");
    }

    // --- secret-reference resolver wiring (control_key / worker_key / db) ---

    /// A literal secret resolves to itself byte-for-byte: the resolver is a
    /// pass-through for any value that is not a `urn:`/`arn:` reference. This is
    /// the contract the real boot path relies on — literal secrets must behave
    /// exactly as they did before the resolver was wired in.
    #[test]
    fn worker_literal_secret_resolves_to_itself() {
        let literal = "super-secret-control-key-value";
        let resolved =
            zeroship_core::config::resolve_secret(literal).expect("literal must resolve");
        assert_eq!(resolved, literal);
    }

    /// `is_secret_ref` distinguishes a reference from a literal. The
    /// check-config guard-skip in other binaries keys off this; here we lock the
    /// boolean so a literal is never mistaken for a reference (which would
    /// wrongly skip a strength guard) and a reference is always recognised
    /// (so a raw `env:`/`file:` string is never fed to a strength check during
    /// `--check-config`).
    #[test]
    fn worker_is_secret_ref_gates_literal_vs_reference() {
        // Literal: NOT a reference — a strength guard `!check || !is_ref` would
        // still RUN for a literal under check-config.
        assert!(!zeroship_core::config::is_secret_ref(
            "super-secret-control-key-value"
        ));
        // References: urn:zeroship:{env,file}: forms are recognised, so the
        // `!is_ref` half of the guard skip is true (guard skipped under check).
        assert!(zeroship_core::config::is_secret_ref(
            "urn:zeroship:env:CONTROL_KEY"
        ));
        assert!(zeroship_core::config::is_secret_ref(
            "urn:zeroship:file:/etc/zeroship/key"
        ));
    }

    /// A malformed reference is rejected by the format validator used on the
    /// `--check-config` path (`validate_secret_ref_or_exit` calls this and
    /// exits non-zero). A reserved `urn:`/`arn:` prefix that does not name a
    /// recognised scheme is malformed.
    #[test]
    fn worker_malformed_secret_ref_is_rejected() {
        assert!(zeroship_core::config::validate_secret_ref("urn:bogus:nope").is_err());
        // A well-formed reference and a plain literal both validate cleanly.
        assert!(zeroship_core::config::validate_secret_ref("urn:zeroship:env:WORKER_KEY").is_ok());
        assert!(zeroship_core::config::validate_secret_ref("a-plain-literal").is_ok());
    }

    // Serialise env-touching `[secrets]` tier tests; they set/remove a process
    // env var that `obtain_secret` resolves a `urn:zeroship:env:` reference from.
    static SECRETS_TIER_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// `[secrets]` file tier regression: when the CLI/env secret is empty, the
    /// `[secrets]` overlay reference (e.g. `control_key`/`worker_key`/`database_url`)
    /// is resolved and used. Pre-fix the worker called `resolve_secret_or_exit`
    /// with only the CLI value, so a `[secrets]` entry could never back-fill an
    /// empty CLI/env secret. Mirrors `file_secrets.<NAME>` wiring on the boot path.
    #[test]
    fn worker_secrets_file_tier_backfills_empty_cli() {
        let _guard = SECRETS_TIER_ENV_LOCK.lock().unwrap();
        std::env::set_var("WORKER_TEST_SECRETS_TIER_VAR", "from-secrets-file");

        // Empty CLI/env + a `[secrets]` reference → resolves to the referenced value.
        let resolved = zeroship_core::config::obtain_secret(
            "WORKER_KEY / --worker-key",
            "",
            Some("urn:zeroship:env:WORKER_TEST_SECRETS_TIER_VAR"),
            false,
        );
        assert_eq!(resolved, "from-secrets-file");

        std::env::remove_var("WORKER_TEST_SECRETS_TIER_VAR");
    }

    /// `[secrets]` file tier precedence: a CLI/env secret WINS over a `[secrets]`
    /// overlay entry. The file reference must not even be consulted (so its env
    /// var being unset is irrelevant). This locks `CLI/env > [secrets]`.
    #[test]
    fn worker_cli_secret_wins_over_secrets_file() {
        let _guard = SECRETS_TIER_ENV_LOCK.lock().unwrap();
        // Deliberately do NOT set the env the file ref points at: if precedence
        // were wrong and the file tier were consulted, resolution would exit(1).
        std::env::remove_var("WORKER_TEST_SECRETS_TIER_VAR");

        let resolved = zeroship_core::config::obtain_secret(
            "WORKER_KEY / --worker-key",
            "literal-cli-worker-key",
            Some("urn:zeroship:env:WORKER_TEST_SECRETS_TIER_VAR"),
            false,
        );
        assert_eq!(resolved, "literal-cli-worker-key");
    }

    /// Bare `--dev-insecure` (no value) enables insecure mode via the
    /// `default_missing_value = "true"`.
    #[test]
    fn worker_dev_insecure_bare_flag_enables() {
        let _guard = DEV_INSECURE_ENV_LOCK.lock().unwrap();
        std::env::remove_var("ZEROSHIP_DEV_INSECURE");

        let cli = WorkerCli::try_parse_from(["zeroship-worker", "--dev-insecure"])
            .expect("bare --dev-insecure should parse");
        assert_eq!(cli.dev_insecure, Some(true));

        // Absent flag + absent env resolves to false (secure default).
        let bare = WorkerCli::try_parse_from(["zeroship-worker"]).expect("bare parse");
        assert_eq!(bare.dev_insecure, None);
        assert!(!bare.dev_insecure.unwrap_or(false));
    }
}
