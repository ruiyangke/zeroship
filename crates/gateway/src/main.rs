//! `zeroship-gate` binary entry point. Thin shell over the
//! [`zeroship_gateway`] library: parse flags, build [`GateState`],
//! register routes, run.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use ntex::web;
use zeroship_core::config::{
    bootstrap_or_exit, parse_bool_flag, require_unless_dev, resolve_overlay_string,
    validate_stash_key, CheckConfigReport, CheckFormat, CheckValue, DEV_STASH_SIGNING_KEY,
};
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_gateway::{
    backchannel_logout, blob_cache, dpop_exchange, enforce, idempotency, oidc_rp, proxy, router,
    signing, sync, wrapper_token, GateConfig, GateState,
};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const DEFAULT_HYDRA_PUBLIC_URL: &str = "https://auth.zeroship.ai";
const DEV_GATEWAY_OIDC_SECRET: &str = "dev-secret-rotate-me-too";

/// zeroship gateway startup configuration.
#[derive(Parser)]
#[command(name = "zeroship-gate")]
struct GateCli {
    /// HTTP listen port.
    #[arg(long, env = "GATE_PORT", default_value_t = 80)]
    port: u16,

    /// Address to bind. Defaults to loopback; pass 0.0.0.0 to expose across a network.
    #[arg(long, env = "GATE_BIND", default_value = "127.0.0.1")]
    bind: String,

    /// Control-plane API base URL.
    #[arg(long = "control", env = "CONTROL_URL", default_value = "http://localhost:9090")]
    control: String,

    /// Admin/control API shared secret.
    #[arg(long = "control-key", env = "CONTROL_KEY", default_value = "", hide_env_values = true)]
    control_key: String,

    /// Comma-separated worker base URLs.
    #[arg(long = "workers", env = "WORKER_URLS", default_value = "http://localhost:8080")]
    workers: String,

    /// Route-table polling interval in seconds.
    #[arg(long = "poll-interval", env = "POLL_INTERVAL", default_value = "5")]
    poll_interval: u64,

    /// Shared secret for worker admin endpoints.
    #[arg(long = "worker-key", env = "WORKER_KEY", default_value = "", hide_env_values = true)]
    worker_key: String,

    /// Root directory for content-addressed deploy blobs.
    #[arg(long = "blob-store", env = "BLOB_STORE", default_value = "./bundles")]
    blob_store: String,

    /// In-memory blob cache budget in MiB.
    #[arg(long = "blob-cache-mem-mb", env = "BLOB_CACHE_MEM_MB", default_value = "256")]
    blob_cache_mem_mb: usize,

    /// On-disk blob cache budget in GiB.
    #[arg(long = "blob-cache-disk-gb", env = "BLOB_CACHE_DISK_GB", default_value = "20")]
    blob_cache_disk_gb: u64,

    /// Root directory for the on-disk blob cache.
    #[arg(
        long = "blob-cache-disk-root",
        env = "BLOB_CACHE_DISK_ROOT",
        default_value = "./blob-cache"
    )]
    blob_cache_disk_root: String,

    /// `PostgreSQL` DSN for gateway session validation.
    #[arg(long = "db", env = "DATABASE_URL", default_value = "", hide_env_values = true)]
    db: String,

    /// PEM/PKCS#8 signing key file for gateway-issued wrapper tokens.
    #[arg(
        long = "signing-key-file",
        env = "GATEWAY_SIGNING_KEY_FILE",
        default_value = ""
    )]
    gateway_signing_key_file: String,

    /// Public URL advertised as the gateway wrapper-token issuer.
    #[arg(
        long = "gateway-public-url",
        env = "GATEWAY_PUBLIC_URL",
        default_value = "https://api.zeroship.ai"
    )]
    gateway_public_url: String,

    /// Hydra public issuer/base URL.
    #[arg(long = "hydra-public-url", env = "HYDRA_PUBLIC_URL")]
    hydra_public_url: Option<String>,

    /// Upstream URL for the auth service UI and OAuth surfaces.
    #[arg(long = "auth-ui-url", env = "AUTH_UI_URL", default_value = "http://auth:9092")]
    auth_ui_url: String,

    /// Gateway OIDC client secret.
    #[arg(
        long = "gateway-oidc-secret",
        env = "GATEWAY_OIDC_SECRET",
        default_value = "",
        hide_env_values = true
    )]
    gateway_oidc_secret: String,

    /// HMAC key for short-lived OIDC stash cookies.
    #[arg(
        long = "stash-signing-key",
        env = "STASH_SIGNING_KEY",
        default_value = "",
        hide_env_values = true
    )]
    stash_signing_key: String,

    /// Allow explicitly insecure local development startup.
    /// CLI presence overrides the env var, so `--dev-insecure=false`
    /// disables a stray `ZEROSHIP_DEV_INSECURE=1`.
    #[arg(
        long = "dev-insecure",
        env = "ZEROSHIP_DEV_INSECURE",
        num_args = 0..=1,
        default_missing_value = "true",
        value_parser = parse_bool_flag
    )]
    dev_insecure: Option<bool>,

    /// Trust `X-Forwarded-For` from an upstream proxy. CLI presence
    /// overrides the env var.
    #[arg(
        long = "trust-proxy",
        env = "ZEROSHIP_TRUST_PROXY",
        num_args = 0..=1,
        default_missing_value = "true",
        value_parser = parse_bool_flag
    )]
    trust_proxy: Option<bool>,

    /// Optional shared config overlay path.
    #[arg(long = "config", env = "ZEROSHIP_CONFIG")]
    config_path: Option<PathBuf>,

    /// Disable auto-discovery of the well-known config overlay
    /// (`/etc/zeroship/zeroship.toml`); use compiled defaults instead.
    #[arg(long = "no-config")]
    no_config: bool,

    /// Validate config (CLI + overlay + guards) and print the resolved non-secret config, then exit without starting the server.
    #[arg(long = "check-config")]
    check_config: bool,

    /// Output format for `--check-config`: `text` (default) or `json`.
    #[arg(long = "check-config-format", default_value = "text")]
    check_config_format: String,

    /// Observability CLI/env overrides.
    #[command(flatten)]
    obs: zeroship_core::observability::ObservabilityFlags,
}

/// Parse the comma-separated `--workers`/`WORKER_URLS` list into a clean
/// vector, trimming whitespace and dropping empty entries. Parsed ONCE so
/// the check-config count and the runtime hash ring can never disagree
/// (M7) — previously check-config filtered empties while the runtime kept
/// them, so `a,,b` reported 2 workers but routed across 3 (one empty URL).
fn parse_worker_urls(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn main() -> std::io::Result<()> {
    let cli = GateCli::parse();
    let boot = bootstrap_or_exit(
        cli.config_path.as_deref(),
        !cli.no_config,
        &cli.obs,
        "info,zeroship_gateway=debug",
        "gateway",
    );
    let file = &boot.overlay.config;

    let insecure_dev = cli.dev_insecure.unwrap_or(false);
    let trust_proxy = cli.trust_proxy.unwrap_or(false);
    let hydra_public_url = resolve_overlay_string(
        cli.hydra_public_url,
        file.auth.hydra_public_url.clone(),
        Some(DEFAULT_HYDRA_PUBLIC_URL),
    );

    let port = cli.port;
    let bind_host = cli.bind;
    let control_url = cli.control;
    let control_key = cli.control_key;
    // M7 — parse the worker URL list ONCE (rejecting empty/whitespace
    // entries) and reuse it for both the check-config count and the
    // runtime hash ring, so the two can never disagree.
    let worker_urls = parse_worker_urls(&cli.workers);
    let poll_interval = cli.poll_interval;
    let worker_key = cli.worker_key;
    let blob_store_root = cli.blob_store;
    let blob_cache_mem_mb = cli.blob_cache_mem_mb;
    let blob_cache_disk_gb = cli.blob_cache_disk_gb;
    let blob_cache_disk_root = cli.blob_cache_disk_root;
    let auth_ui_url = cli.auth_ui_url;
    let pg_dsn = cli.db;
    let oidc_client_secret = cli.gateway_oidc_secret;
    let stash_signing_key = cli.stash_signing_key;
    let signing_key_path = cli.gateway_signing_key_file;
    let public_url = cli.gateway_public_url;

    if let Err(message) =
        require_unless_dev("CONTROL_KEY / --control-key", &control_key, insecure_dev)
    {
        tracing::error!(error = %message, "gateway: refusing to start without control key");
        std::process::exit(1);
    }

    if let Err(message) = require_unless_dev(
        "GATEWAY_OIDC_SECRET / --gateway-oidc-secret",
        &oidc_client_secret,
        insecure_dev,
    ) {
        tracing::error!(error = %message, "gateway: refusing to start without gateway OIDC secret");
        std::process::exit(1);
    }
    let oidc_client_secret = if oidc_client_secret.is_empty() {
        DEV_GATEWAY_OIDC_SECRET.to_string()
    } else {
        oidc_client_secret
    };

    if let Err(message) = validate_stash_key(&stash_signing_key, insecure_dev) {
        tracing::error!(error = %message, "gateway: refusing to start with unsafe stash signing key");
        std::process::exit(1);
    }
    let stash_signing_key = if stash_signing_key.is_empty() {
        DEV_STASH_SIGNING_KEY.to_string()
    } else {
        stash_signing_key
    };

    // S3 — symmetric WORKER_KEY enforcement. The worker refuses a
    // non-loopback bind without a key; the gateway is the caller of those
    // worker admin endpoints, so it must fail just as hard rather than
    // shipping `Authorization: Bearer ` (empty) into a cluster that
    // believes dispatch is authenticated.
    if let Err(message) =
        require_unless_dev("WORKER_KEY / --worker-key", &worker_key, insecure_dev)
    {
        tracing::error!(error = %message, "gateway: refusing to start without worker key");
        std::process::exit(1);
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

    if cli.check_config {
        let log_format = boot
            .log_format
            .map_or_else(|| "auto".to_string(), |f| f.to_string());
        let mut report = CheckConfigReport::new();
        report.field("port", CheckValue::Count(usize::from(port)));
        report.field("bind", CheckValue::Plain(bind_host.clone()));
        report.field(
            "config_source",
            CheckValue::Plain(boot.overlay.source.to_string()),
        );
        report.field("control_url", CheckValue::Plain(control_url));
        report.field(
            "hydra_public_url",
            CheckValue::Plain(hydra_public_url),
        );
        report.field("auth_ui_url", CheckValue::Plain(auth_ui_url));
        report.field("log_filter", CheckValue::Plain(boot.log_filter.clone()));
        report.field("log_format", CheckValue::Plain(log_format));
        report.field("insecure_dev", CheckValue::Flag(insecure_dev));
        report.field("trust_proxy", CheckValue::Flag(trust_proxy));
        report.field("blob_store", CheckValue::Plain(blob_store_root));
        report.field(
            "blob_cache_mem_mb",
            CheckValue::Count(blob_cache_mem_mb),
        );
        report.field(
            "blob_cache_disk_gb",
            CheckValue::Count(usize::try_from(blob_cache_disk_gb).unwrap_or(usize::MAX)),
        );
        report.field(
            "blob_cache_disk_root",
            CheckValue::Plain(blob_cache_disk_root),
        );
        report.field(
            "poll_interval_secs",
            CheckValue::Count(usize::try_from(poll_interval).unwrap_or(usize::MAX)),
        );
        report.field("workers_count", CheckValue::Count(worker_urls.len()));
        report.field("db_configured", CheckValue::Secret(!pg_dsn.is_empty()));
        report.field(
            "signing_key_configured",
            CheckValue::Secret(!signing_key_path.is_empty()),
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
        .name("zeroship-gate")
        .build(ntex::rt::DefaultRuntime)
        .block_on(async move {
    let blob_cache_bytes: usize = blob_cache_mem_mb.saturating_mul(1024 * 1024);
    let disk_cache_bytes: u64 = blob_cache_disk_gb.saturating_mul(1024 * 1024 * 1024);
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
        &auth_ui_url,
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
        .unwrap_or_default();

    let state = Arc::new(GateState {
        config: GateConfig {
            control_url,
            control_key,
            worker_urls,
            poll_interval_secs: poll_interval,
            worker_key,
            hydra_public_url,
            auth_ui_url,
            insecure_dev,
            trust_proxy,
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

    let bind_addr = format!("{bind_host}:{port}");
    if insecure_dev && bind_host != "127.0.0.1" && bind_host != "::1" && bind_host != "localhost" {
        tracing::warn!(
            bind = %bind_addr,
            "gateway: binding a non-loopback address under --dev-insecure on an untrusted \
             network is unsafe"
        );
    }
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
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    // S1: a stray `ZEROSHIP_DEV_INSECURE=1` in the environment MUST be
    // overridable from the CLI. `--dev-insecure=false` resolves to false.
    // NB: `GateCli` deliberately has no `Debug` (S2 — it holds raw secret
    // strings), so we can't `.expect()` the Ok arm; match instead.
    #[test]
    fn dev_insecure_cli_false_overrides_env_one() {
        // Single-threaded test: env set + cleared within this fn.
        // (Edition 2021 — `set_var`/`remove_var` are safe here.)
        std::env::set_var("ZEROSHIP_DEV_INSECURE", "1");
        let parsed = GateCli::try_parse_from(["zeroship-gate", "--dev-insecure=false"]);
        std::env::remove_var("ZEROSHIP_DEV_INSECURE");

        let Ok(cli) = parsed else {
            panic!("parse with explicit false should succeed");
        };
        let insecure_dev = cli.dev_insecure.unwrap_or(false);
        assert!(!insecure_dev, "CLI --dev-insecure=false must beat env=1");
    }

    #[test]
    fn dev_insecure_env_one_enables_when_cli_absent() {
        std::env::set_var("ZEROSHIP_DEV_INSECURE", "1");
        let parsed = GateCli::try_parse_from(["zeroship-gate"]);
        std::env::remove_var("ZEROSHIP_DEV_INSECURE");

        let Ok(cli) = parsed else {
            panic!("parse with env only should succeed");
        };
        assert_eq!(cli.dev_insecure, Some(true));
    }

    // S3: a missing WORKER_KEY is fatal outside dev, allowed inside dev.
    #[test]
    fn missing_worker_key_is_fatal_outside_dev() {
        assert!(require_unless_dev("WORKER_KEY / --worker-key", "", false).is_err());
        assert!(require_unless_dev("WORKER_KEY / --worker-key", "", true).is_ok());
        assert!(require_unless_dev("WORKER_KEY / --worker-key", "k", false).is_ok());
    }

    // M6: `--auth-secret` is a deleted legacy knob — clap must reject it
    // as an unknown argument, not silently accept it.
    #[test]
    fn auth_secret_flag_is_rejected() {
        let parsed = GateCli::try_parse_from(["zeroship-gate", "--auth-secret", "x"]);
        let err = match parsed {
            Ok(_) => panic!("--auth-secret must be rejected as an unknown argument"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    // M7: the worker URL list is parsed ONCE; empty/whitespace entries are
    // dropped so the check-config count and runtime hash ring agree.
    #[test]
    fn worker_urls_drops_empty_entries() {
        let parsed = parse_worker_urls("http://a:8080,,http://b:8080");
        assert_eq!(parsed, vec!["http://a:8080", "http://b:8080"]);
        assert_eq!(parsed.len(), 2);
    }

    #[test]
    fn worker_urls_trims_whitespace_entries() {
        let parsed = parse_worker_urls(" http://a:8080 ,  , http://b:8080 ");
        assert_eq!(parsed, vec!["http://a:8080", "http://b:8080"]);
    }

    #[test]
    fn gateway_stash_key_rejects_missing_in_non_dev() {
        let err = validate_stash_key("", false).unwrap_err();
        assert!(err.contains("required"), "{err}");
    }

    #[test]
    fn gateway_control_key_rejects_missing_in_non_dev() {
        let err =
            require_unless_dev("CONTROL_KEY / --control-key", "", false).unwrap_err();
        assert!(err.contains("CONTROL_KEY"), "{err}");
    }

    #[test]
    fn gateway_control_key_accepts_nonempty_in_non_dev() {
        assert!(require_unless_dev("CONTROL_KEY / --control-key", "secret", false).is_ok());
    }

    #[test]
    fn gateway_control_key_allows_missing_in_insecure_dev() {
        assert!(require_unless_dev("CONTROL_KEY / --control-key", "", true).is_ok());
    }

    #[test]
    fn gateway_oidc_secret_rejects_missing_in_non_dev() {
        let err = require_unless_dev(
            "GATEWAY_OIDC_SECRET / --gateway-oidc-secret",
            "",
            false,
        )
        .unwrap_err();
        assert!(err.contains("GATEWAY_OIDC_SECRET"), "{err}");
    }

    #[test]
    fn gateway_oidc_secret_allows_missing_in_insecure_dev() {
        assert!(
            require_unless_dev("GATEWAY_OIDC_SECRET / --gateway-oidc-secret", "", true).is_ok()
        );
    }

    #[test]
    fn gateway_oidc_secret_accepts_nonempty_in_non_dev() {
        assert!(
            require_unless_dev(
                "GATEWAY_OIDC_SECRET / --gateway-oidc-secret",
                "secret",
                false
            )
            .is_ok()
        );
    }

    #[test]
    fn gateway_stash_key_rejects_dev_default_in_non_dev() {
        let err = validate_stash_key(DEV_STASH_SIGNING_KEY, false).unwrap_err();
        assert!(err.contains("dev default"), "{err}");
    }

    #[test]
    fn gateway_stash_key_rejects_short_in_non_dev() {
        let err = validate_stash_key("short", false).unwrap_err();
        assert!(err.contains("too short"), "{err}");
    }

    #[test]
    fn gateway_stash_key_accepts_strong_in_non_dev() {
        let key = "0123456789abcdef0123456789abcdef";
        assert!(validate_stash_key(key, false).is_ok());
    }

    #[test]
    fn gateway_stash_key_allows_dev_default_in_insecure_dev() {
        assert!(validate_stash_key(DEV_STASH_SIGNING_KEY, true).is_ok());
        assert!(validate_stash_key("", true).is_ok());
    }
}
