//! `zeroship-gate` binary entry point. Thin shell over the
//! [`zeroship_gateway`] library: parse flags, build [`GateState`],
//! register routes, run.

use std::path::PathBuf;
use std::sync::Arc;

use ntex::web;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_gateway::{
    backchannel_logout, blob_cache, dpop_exchange, enforce, idempotency, oidc_rp, proxy, router,
    signing, sync, wrapper_token, GateConfig, GateState,
};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const DEV_STASH_SIGNING_KEY: &str = "dev-stash-key-please-rotate";

fn validate_gateway_stash_key(value: &str, insecure_dev: bool) -> Result<(), String> {
    if insecure_dev {
        return Ok(());
    }
    if value.is_empty() {
        return Err(
            "STASH_SIGNING_KEY is required outside INSECURE_DEV=true; set a strong (>=32 byte) value"
                .to_string(),
        );
    }
    if value == DEV_STASH_SIGNING_KEY {
        return Err(
            "STASH_SIGNING_KEY is the dev default; refusing to boot without INSECURE_DEV=true"
                .to_string(),
        );
    }
    if value.len() < 32 {
        return Err(format!(
            "STASH_SIGNING_KEY is too short ({} bytes); minimum 32 bytes",
            value.len()
        ));
    }
    Ok(())
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
        "",
    );
    let insecure_dev = arg_or_env(&args, "--insecure-dev", "INSECURE_DEV", "false")
        .eq_ignore_ascii_case("true");
    let signing_key_path = arg_or_env(
        &args,
        "--signing-key-file",
        "GATEWAY_SIGNING_KEY_FILE",
        "",
    );
    let public_url = arg_or_env(
        &args,
        "--gateway-public-url",
        "GATEWAY_PUBLIC_URL",
        "https://api.zeroship.ai",
    );

    if let Err(message) = validate_gateway_stash_key(&stash_signing_key, insecure_dev) {
        tracing::error!(error = %message, "gateway: refusing to start with unsafe stash signing key");
        std::process::exit(1);
    }
    let stash_signing_key = if stash_signing_key.is_empty() {
        DEV_STASH_SIGNING_KEY.to_string()
    } else {
        stash_signing_key
    };

    if worker_key.is_empty() {
        tracing::warn!(
            "WORKER_KEY not set — worker endpoints are unauthenticated"
        );
    }

    // Phase 8 U1 — load the gateway's wrapper-token signing key. The
    // flag is optional: when empty, the boot succeeds but DPoP-exchange
    // endpoints (added in U2/U3) will 503. We log a clear warning so
    // operators don't get a surprise during DPoP rollout.
    let signing_key: Option<Arc<ed25519_dalek::SigningKey>> = if signing_key_path.is_empty() {
        tracing::warn!(
            "GATEWAY_SIGNING_KEY_FILE not set — DPoP token-exchange endpoints will 503"
        );
        None
    } else {
        let key = signing::load_from_path(std::path::Path::new(&signing_key_path))
            .expect("gateway: load signing key");
        let kid = signing::jwk_thumbprint(&key);
        tracing::info!(
            path = %signing_key_path,
            kid = %kid,
            "gateway signing key loaded"
        );
        Some(Arc::new(key))
    };

    // Phase 8 U3 — wrapper-token issuer. One-to-one with `signing_key`:
    // both Some, or both None. Built once at boot so the per-request
    // /__zs/auth/dpop-exchange path doesn't pay for PKCS#8 encoding +
    // thumbprinting on every request.
    let wrapper_issuer: Option<Arc<wrapper_token::Issuer>> = signing_key.as_ref().map(|sk| {
        let issuer = wrapper_token::Issuer::new(sk.as_ref(), public_url.clone())
            .expect("wrapper_token::Issuer construction");
        Arc::new(issuer)
    });

    // Phase 8 U4 — wrapper-token verifier. Built from the PUBLIC half
    // of the same signing key in lockstep with `wrapper_issuer` (both
    // Some, or both None). The dispatch path consults this to detect
    // wrapper-bound DPoP requests; raw-hydra DPoP requests fall through
    // to the P7-U5 introspection path. Cheap to construct (no PKCS#8
    // encoding — `DecodingKey::from_ed_der` accepts the raw 32-byte
    // public key), so we just build it eagerly at boot.
    let wrapper_verifier: Option<Arc<wrapper_token::Verifier>> = signing_key.as_ref().map(|sk| {
        let public = sk.verifying_key();
        Arc::new(wrapper_token::Verifier::new(&public, public_url.clone()))
    });

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

    let dpop_jti_cache = db
        .as_ref()
        .map(|client| {
            let pg = zeroship_core::dpop::PgJtiCache::new(client.clone());
            zeroship_core::dpop::TieredJtiCache::with_pg(pg)
        })
        .unwrap_or_else(zeroship_core::dpop::TieredJtiCache::default);

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
            public_url,
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
        dpop_jti_cache: Arc::new(dpop_jti_cache),
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
        signing_key,
        wrapper_issuer,
        wrapper_verifier,
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
            // Phase 8 U3 — DPoP token exchange. Registered BEFORE
            // the subdomain catch-all so the path lands on the
            // dedicated handler rather than being dispatched as a
            // creator-app route. `cache-control: no-store` is set on
            // every response so intermediaries don't keep wrapper
            // tokens around.
            .service(
                web::resource("/__zs/auth/dpop-exchange")
                    .route(web::post().to(dpop_exchange::handle)),
            )
            // OIDC Back-Channel Logout 1.0 RP endpoint. Registered
            // at the gateway-host level (not per-app) because the
            // URI is stable across every `backchannel_logout_uri`
            // entry in `ops/auth-clients.example.toml`. Must be
            // mounted BEFORE the subdomain catch-all below — ntex's
            // path routing is registration-order-sensitive for
            // overlapping patterns.
            .configure(backchannel_logout::configure)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_stash_key_rejects_missing_in_non_dev() {
        let err = validate_gateway_stash_key("", false).unwrap_err();
        assert!(err.contains("required"), "{err}");
    }

    #[test]
    fn gateway_stash_key_rejects_dev_default_in_non_dev() {
        let err = validate_gateway_stash_key(DEV_STASH_SIGNING_KEY, false).unwrap_err();
        assert!(err.contains("dev default"), "{err}");
    }

    #[test]
    fn gateway_stash_key_rejects_short_in_non_dev() {
        let err = validate_gateway_stash_key("short", false).unwrap_err();
        assert!(err.contains("too short"), "{err}");
    }

    #[test]
    fn gateway_stash_key_accepts_strong_in_non_dev() {
        let key = "0123456789abcdef0123456789abcdef";
        assert!(validate_gateway_stash_key(key, false).is_ok());
    }

    #[test]
    fn gateway_stash_key_allows_dev_default_in_insecure_dev() {
        assert!(validate_gateway_stash_key(DEV_STASH_SIGNING_KEY, true).is_ok());
        assert!(validate_gateway_stash_key("", true).is_ok());
    }
}
