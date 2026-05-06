mod auth;
mod blob_cache;
mod compiled;
mod dispatch;
mod enforce;
mod idempotency;
mod proxy;
mod router;
mod sync;
mod user_auth;

use std::path::PathBuf;
use std::sync::Arc;

use ntex::web;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[allow(missing_debug_implementations)]
pub struct GateConfig {
    pub control_url: String,
    pub control_key: String,
    pub worker_urls: Vec<String>,
    pub poll_interval_secs: u64,
    pub auth_secret: String,
    /// Shared secret between gateway and workers. Used to bearer-auth the
    /// `/dispatch` endpoints and HMAC-sign the `ZeroShip-User` header so
    /// workers can verify forwarded identity was not forged by an attacker
    /// with direct network access. Empty disables both checks (dev only).
    pub worker_key: String,
}

#[allow(missing_debug_implementations)]
pub struct GateState {
    pub config: GateConfig,
    pub routes: sync::RouteCache,
    pub hash_ring: proxy::HashRing,
    pub rate_limiters: enforce::RateLimitRegistry,
    /// Per-rule rate limits declared in `Action::Worker.rate_limit`.
    /// Layered on top of the global per-app `rate_limiters` — runs
    /// FIRST in the request path so a rule that's already saturated
    /// short-circuits without the global bucket lookup.
    pub per_rule_rate_limits: enforce::PerRuleRateLimitRegistry,
    pub concurrency: enforce::ConcurrencyRegistry,
    /// Content-addressed blob store. The gateway fetches asset bytes
    /// here directly instead of round-tripping through the control
    /// plane.
    pub blob_store: Arc<dyn BlobStore>,
    /// In-memory LRU cache in front of `blob_store`.
    pub blob_cache: blob_cache::BlobCache,
    /// On-disk LRU cache underneath `blob_cache`. Large blobs that
    /// do not fit in memory land here, and `serve_static_hit` mmaps
    /// them on serve so the userspace → kernel copy goes away.
    pub disk_cache: blob_cache::DiskBlobCache,
    /// KV-backed dedupe table for idempotent RPC mutations. The
    /// gateway consults this before forwarding `idempotent: true`
    /// mutations to the worker; on a hit it returns the stored
    /// response without touching V8.
    pub idempotency_store: std::sync::Arc<dyn idempotency::IdempotencyStore>,
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    zeroship_core::observability::init_tracing("info,zeroship_gateway=debug");

    let args: Vec<String> = std::env::args().collect();
    let port = arg_or_env(&args, "--port", "GATE_PORT", "80");
    let control_url = arg_or_env(&args, "--control", "CONTROL_URL", "http://localhost:9090");
    let control_key = arg_or_env(&args, "--control-key", "CONTROL_KEY", "");
    let workers_str = arg_or_env(&args, "--workers", "WORKER_URLS", "http://localhost:8080");
    let poll_interval = arg_or_env(&args, "--poll-interval", "POLL_INTERVAL", "5");
    let auth_secret = arg_or_env(&args, "--auth-secret", "AUTH_SECRET", "");
    let worker_key = arg_or_env(&args, "--worker-key", "WORKER_KEY", "");
    let blob_store_root = arg_or_env(&args, "--blob-store", "BLOB_STORE", "./bundles");
    let blob_cache_mem_mb = arg_or_env(&args, "--blob-cache-mem-mb", "BLOB_CACHE_MEM_MB", "256");
    let blob_cache_disk_gb = arg_or_env(&args, "--blob-cache-disk-gb", "BLOB_CACHE_DISK_GB", "20");
    let blob_cache_disk_root = arg_or_env(
        &args,
        "--blob-cache-disk-root",
        "BLOB_CACHE_DISK_ROOT",
        "./blob-cache",
    );

    if worker_key.is_empty() {
        tracing::warn!(
            "WORKER_KEY not set — worker endpoints are unauthenticated"
        );
    }

    let blob_cache_bytes: usize = blob_cache_mem_mb
        .parse::<usize>()
        .unwrap_or(256)
        .saturating_mul(1024 * 1024);
    let disk_cache_bytes: u64 = blob_cache_disk_gb
        .parse::<u64>()
        .unwrap_or(20)
        .saturating_mul(1024 * 1024 * 1024);
    let blob_store: Arc<dyn BlobStore> = Arc::new(
        LocalDiskBlobStore::new(PathBuf::from(&blob_store_root))
            .expect("failed to initialise blob store"),
    );
    let disk_cache = blob_cache::DiskBlobCache::new(
        PathBuf::from(&blob_cache_disk_root),
        disk_cache_bytes,
    )
    .expect("failed to initialise disk blob cache");
    tracing::info!(
        blob_store_root = %blob_store_root,
        blob_cache_mem_mb = %blob_cache_mem_mb,
        blob_cache_disk_root = %blob_cache_disk_root,
        blob_cache_disk_gb = %blob_cache_disk_gb,
        "gateway blob store + cache configured"
    );

    let worker_urls: Vec<String> = workers_str
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();

    let num_workers = worker_urls.len();
    // Bounded load: each worker handles at most 125% of average load
    // With 10K apps and 10 workers, avg = 1K apps → max = 1.25K
    // For request concurrency, use a generous static bound
    let max_per_worker = 500u32;

    tracing::info!(
        workers = num_workers,
        vnodes = 150,
        max_per_worker,
        "gateway routing configured (CHWBL)"
    );

    let hash_ring = proxy::HashRing::new(worker_urls.clone(), max_per_worker);

    let state = Arc::new(GateState {
        config: GateConfig {
            control_url,
            control_key,
            worker_urls,
            poll_interval_secs: poll_interval.parse().unwrap_or(5),
            auth_secret,
            worker_key,
        },
        routes: sync::RouteCache::new(),
        hash_ring,
        rate_limiters: enforce::RateLimitRegistry::new(1000, 2000),
        per_rule_rate_limits: enforce::PerRuleRateLimitRegistry::new(),
        concurrency: enforce::ConcurrencyRegistry::new(100),
        blob_store,
        blob_cache: blob_cache::BlobCache::new(blob_cache_bytes),
        disk_cache,
        idempotency_store: Arc::new(idempotency::InMemoryIdempotencyStore::new()),
    });

    sync::start_sync(state.clone());

    let bind_addr = format!("0.0.0.0:{port}");
    tracing::info!(bind = %bind_addr, "gateway listening");

    web::server(async move || {
        web::App::new()
            .state(state.clone())
            .service(
                // ntex's `{path:.*}` only matches a single segment;
                // `{tail}*` is the tail-match syntax that handles
                // nested asset paths like `assets/index-abc.js`.
                web::resource("/apps/{app_name}/{tail}*")
                    .route(web::route().to(router::handle)),
            )
            .service(web::resource("/health").route(web::get().to(|| async {
                web::HttpResponse::Ok().body(r#"{"status":"ok"}"#)
            })))
            // Subdomain catch-all — must be last (lowest priority)
            .service(
                web::resource("/{tail}*")
                    .route(web::route().to(router::handle_subdomain)),
            )
    })
    .bind(&bind_addr)?
    .run()
    .await
}

fn arg_or_env(args: &[String], flag: &str, env_key: &str, default: &str) -> String {
    for pair in args.windows(2) {
        if pair[0] == flag {
            return pair[1].clone();
        }
    }
    std::env::var(env_key).unwrap_or_else(|_| default.to_string())
}
