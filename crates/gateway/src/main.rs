mod auth;
mod blob_cache;
mod compiled;
mod dispatch;
mod enforce;
mod error;
mod idempotency;
mod oidc_rp;
mod proxy;
mod router;
mod sessions;
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
    /// Upstream URL for Ory Hydra's public OIDC endpoints. The gateway
    /// forwards `auth.zeroship.ai/{oauth2,.well-known}/*` (plus
    /// `/userinfo`) here. Compose-internal default points at the
    /// `hydra` service on its 4444 port.
    pub hydra_public: String,
    /// Upstream URL for `crates/auth` — the login/signup UI, OAuth2
    /// consent handlers, and webhook surfaces. Everything on the
    /// `auth.zeroship.ai` host that is NOT an OIDC protocol endpoint
    /// is forwarded here. Defaults to the compose-internal `auth`
    /// service; will be wired through once the service joins compose
    /// (Phase 3 Unit U11).
    pub auth_public: String,
    /// Dev-only flag. When true the gateway emits cookies without the
    /// `Secure` attribute so the localhost HTTP flow works in `pnpm dev`
    /// / docker-compose. Production MUST set this to false — the
    /// `__Host-` cookie prefix RFC 6265bis §4.1.3 requires `Secure`.
    pub insecure_dev: bool,
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
    /// OIDC relying-party engine for hosted creator apps. Drives the
    /// per-origin authorize-redirect → callback → session-mint flow
    /// (proposal §2.2 + §10.2). One RP instance services every
    /// `{app}.zeroship.ai` host — the per-app `redirect_uri` is the
    /// only thing that changes per request.
    pub oidc_rp: Arc<oidc_rp::OidcRp>,
    /// Postgres client used by the gateway's per-origin session store
    /// (`auth.gateway_sessions`). `Option` because the binary supports
    /// a dev "no-DB" mode (when `--db` is empty); test fixtures also
    /// rely on `None` to construct `GateState` without a live PG.
    pub db: Option<Arc<compio_postgres::Client>>,
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
    let hydra_public = arg_or_env(&args, "--hydra-public", "HYDRA_PUBLIC", "http://hydra:4444");
    let auth_public = arg_or_env(&args, "--auth-public", "AUTH_PUBLIC", "http://auth:9092");
    let pg_dsn = arg_or_env(&args, "--db", "DATABASE_URL", "");
    let oidc_client_secret = arg_or_env(
        &args,
        "--gateway-oidc-secret",
        "GATEWAY_OIDC_SECRET",
        "dev-secret-rotate-me-too",
    );
    let stash_signing_key = arg_or_env(
        &args,
        "--stash-signing-key",
        "STASH_SIGNING_KEY",
        "dev-stash-key-please-rotate",
    );
    let insecure_dev = arg_or_env(&args, "--insecure-dev", "INSECURE_DEV", "false")
        .eq_ignore_ascii_case("true");

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

    // Postgres client for the per-origin session store. The binary
    // accepts an empty DSN (`--db ""`) for dev / smoke modes that don't
    // exercise the OIDC RP path; downstream handlers gracefully return
    // 401 when `db` is None instead of panicking.
    let db: Option<Arc<compio_postgres::Client>> = if pg_dsn.is_empty() {
        tracing::warn!(
            "DATABASE_URL not set — gateway session validation disabled (all auth-gated requests will 401)"
        );
        None
    } else {
        let (pg_client, pg_conn) = compio_postgres::connect(&pg_dsn, compio_postgres::NoTls)
            .await
            .expect("gateway: pg connect");
        compio::runtime::spawn(async move {
            if let Err(e) = pg_conn.run().await {
                tracing::error!(error = %e, "gateway/pg connection ended");
            }
        })
        .detach();
        Some(Arc::new(pg_client))
    };

    // OIDC RP — services every `{app}.zeroship.ai` host. The
    // `client_id` matches the entry registered in
    // `ops/auth-clients.example.toml`; `redirect_uri` is per-app and
    // built at the dispatch site.
    let oidc_rp = Arc::new(oidc_rp::OidcRp::new(
        &auth_public,
        "gateway",
        oidc_client_secret,
        stash_signing_key.into_bytes(),
    ));

    let state = Arc::new(GateState {
        config: GateConfig {
            control_url,
            control_key,
            worker_urls,
            poll_interval_secs: poll_interval.parse().unwrap_or(5),
            auth_secret,
            worker_key,
            hydra_public,
            auth_public,
            insecure_dev,
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
        oidc_rp,
        db,
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
