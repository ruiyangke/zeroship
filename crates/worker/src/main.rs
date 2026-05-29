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
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
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

    // CLI presence overrides env: `--dev-insecure=false` disables even a stray
    // `ZEROSHIP_DEV_INSECURE=1` (S1).
    let insecure_dev = cli.dev_insecure.unwrap_or(false);
    let port = cli.port;
    let workers_count = resolve_worker_threads(cli.worker_threads);
    let control_url = cli.control;
    // Secret-reference resolution (env:/file:/vault:/awssm: indirection). On the
    // real boot path we resolve to the live value (side effects: env/file read);
    // under --check-config we only validate the reference FORMAT, leaving the raw
    // ref string in place so no env/file/network read happens during a dry run.
    let control_key = if cli.check_config {
        zeroship_core::config::validate_secret_ref_or_exit(
            "CONTROL_KEY / --control-key",
            &cli.control_key,
        );
        cli.control_key
    } else {
        zeroship_core::config::resolve_secret_or_exit(
            "CONTROL_KEY / --control-key",
            &cli.control_key,
        )
    };
    let max_isolates = cli.max_isolates;
    let poll_interval = cli.poll_interval;
    let db_url = if cli.check_config {
        zeroship_core::config::validate_secret_ref_or_exit("DATABASE_URL / --db", &cli.db);
        cli.db
    } else {
        zeroship_core::config::resolve_secret_or_exit("DATABASE_URL / --db", &cli.db)
    };
    let worker_key = if cli.check_config {
        zeroship_core::config::validate_secret_ref_or_exit("WORKER_KEY / --worker-key", &cli.worker_key);
        cli.worker_key
    } else {
        zeroship_core::config::resolve_secret_or_exit("WORKER_KEY / --worker-key", &cli.worker_key)
    };
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
        report.field("socket_configured", CheckValue::Flag(!socket_path.is_empty()));
        report.field("db_configured", CheckValue::Flag(!db_url.is_empty()));

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
