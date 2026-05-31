//! `zeroship-gate` binary entry point. Thin shell over the
//! [`zeroship_gateway`] library: parse flags, build [`GateState`],
//! register routes, run.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use ntex::web;
use zeroship_core::config::{
    bootstrap_or_exit, parse_bool_flag, require_unless_dev, resolve_overlay_string,
    validate_stash_key, CheckConfigReport, CheckFormat, CheckValue, DEV_PAIRWISE_SALT,
    DEV_STASH_SIGNING_KEY,
};
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_gateway::{
    auth_token, backchannel_logout, blob_cache, browser_auth, enforce, idempotency, oidc_rp, proxy,
    router, session_token, signing, sync, GateConfig, GateState,
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

    /// Maximum number of pooled PostgreSQL connections the gateway
    /// keeps open for session/anchor/revocation work. Bounds concurrent
    /// DB fan-out so a Hydra brownout (or any stalled query) cannot pile
    /// up unbounded checkouts. Ignored when `--db` is empty.
    #[arg(long = "db-pool-size", env = "DB_POOL_SIZE", default_value_t = 16)]
    db_pool_size: usize,

    /// PEM/PKCS#8 signing key file for the gateway-signed session cookie.
    #[arg(
        long = "signing-key-file",
        env = "GATEWAY_SIGNING_KEY_FILE",
        default_value = ""
    )]
    gateway_signing_key_file: String,

    /// PEM/PKCS#8 PREVIOUS signing key file for the session-cookie rotation
    /// overlap (auth-sdk §8.5). Set ONLY during a key roll: the Verifier
    /// then accepts session cookies signed by EITHER the current or this
    /// previous key. The Issuer always signs with the current key only.
    /// Empty (default) ⇒ single-key Verifier.
    #[arg(
        long = "prev-signing-key-file",
        env = "GATEWAY_PREV_SIGNING_KEY_FILE",
        default_value = ""
    )]
    gateway_prev_signing_key_file: String,

    /// Public URL advertised as the gateway session-cookie issuer.
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

    /// Dedicated PERMANENT pairwise-salt secret (value). The seed for every
    /// app's `pws_` per-app identity anchor (auth-sdk §6.2) — independent of
    /// the rotatable stash key. MUST be identical on gateway + control and
    /// MUST NOT be rotated without a per-app `pws_` migration. Prefer
    /// `--pairwise-salt-file` in production so the value never appears in a
    /// process listing.
    #[arg(
        long = "pairwise-salt",
        env = "PAIRWISE_SALT",
        default_value = "",
        hide_env_values = true
    )]
    pairwise_salt: String,

    /// Path to a file holding the dedicated pairwise-salt secret. Takes
    /// precedence over `--pairwise-salt` / `PAIRWISE_SALT` when set.
    #[arg(long = "pairwise-salt-file", env = "PAIRWISE_SALT_FILE", default_value = "")]
    pairwise_salt_file: String,

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
    #[arg(long = "check-config-format", default_value = "text", value_parser = ["text", "json"])]
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

/// Resolve the dedicated pairwise-salt secret. Precedence:
///   1. `--pairwise-salt-file` / `PAIRWISE_SALT_FILE` (read the file verbatim,
///      trimming a trailing newline) — keeps the value out of the process table,
///   2. else `obtain_secret` on `--pairwise-salt` / `PAIRWISE_SALT` (+ the
///      config-overlay reference).
///
/// A configured-but-unreadable file is fatal (a misconfigured prod salt must
/// fail loudly, not silently fall through to the dev default).
fn resolve_pairwise_salt(
    salt_file: &str,
    salt_value: &str,
    file_ref: Option<&str>,
    check_config: bool,
) -> String {
    if !salt_file.is_empty() {
        return std::fs::read_to_string(salt_file)
            .map(|s| s.trim_end_matches(['\n', '\r']).to_string())
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, path = %salt_file, "gateway: cannot read --pairwise-salt-file");
                std::process::exit(1);
            });
    }
    zeroship_core::config::obtain_secret(
        "PAIRWISE_SALT / --pairwise-salt",
        salt_value,
        file_ref,
        check_config,
    )
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
    // `[secrets]` file-tier overlay — bound ONCE before any secret resolution.
    // The gateway never partially moves `boot.overlay.config`, so a reference
    // is sufficient (no clone needed). Precedence per field: CLI/env > this
    // reference-only file tier > default, applied by `obtain_secret`.
    let file_secrets = &file.secrets;

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
    // Secret-bearing inputs are resolved through the secret-reference
    // resolver: a literal value passes through byte-identically, while a
    // `urn:zeroship:{env|file|...}:…` / `arn:…` reference is dereferenced
    // at real boot. During `--check-config` we only validate the reference
    // FORMAT (no env/file/network reads), keeping the local as the raw ref
    // string for the (non-secret) report.
    let control_key = zeroship_core::config::obtain_secret(
        "CONTROL_KEY / --control-key",
        &cli.control_key,
        file_secrets.control_key.as_deref(),
        cli.check_config,
    );
    // M7 — parse the worker URL list ONCE (rejecting empty/whitespace
    // entries) and reuse it for both the check-config count and the
    // runtime hash ring, so the two can never disagree.
    let worker_urls = parse_worker_urls(&cli.workers);
    let poll_interval = cli.poll_interval;
    let worker_key = zeroship_core::config::obtain_secret(
        "WORKER_KEY / --worker-key",
        &cli.worker_key,
        file_secrets.worker_key.as_deref(),
        cli.check_config,
    );
    let blob_store_root = cli.blob_store;
    let blob_cache_mem_mb = cli.blob_cache_mem_mb;
    let blob_cache_disk_gb = cli.blob_cache_disk_gb;
    let blob_cache_disk_root = cli.blob_cache_disk_root;
    let auth_ui_url = cli.auth_ui_url;
    let db_pool_size = cli.db_pool_size.max(1);
    // DSN carries the database password, so it is resolved like any other
    // secret (literals — including colon-laden DSNs — pass through unchanged).
    let pg_dsn = zeroship_core::config::obtain_secret(
        "DATABASE_URL / --db",
        &cli.db,
        file_secrets.database_url.as_deref(),
        cli.check_config,
    );
    let oidc_client_secret = zeroship_core::config::obtain_secret(
        "GATEWAY_OIDC_SECRET / --gateway-oidc-secret",
        &cli.gateway_oidc_secret,
        file_secrets.gateway_oidc_secret.as_deref(),
        cli.check_config,
    );
    let stash_signing_key = zeroship_core::config::obtain_secret(
        "STASH_SIGNING_KEY / --stash-signing-key",
        &cli.stash_signing_key,
        file_secrets.stash_signing_key.as_deref(),
        cli.check_config,
    );
    // Dedicated pairwise-salt secret. A `--pairwise-salt-file` path wins over
    // the inline `--pairwise-salt`/`PAIRWISE_SALT` value (and over the config
    // overlay reference), so prod can keep the value out of the process table.
    let pairwise_salt = resolve_pairwise_salt(
        &cli.pairwise_salt_file,
        &cli.pairwise_salt,
        file_secrets.pairwise_salt.as_deref(),
        cli.check_config,
    );
    // File-PATH field (names a file to read), NOT a secret value — left
    // unresolved; the signing key is loaded from this path below.
    let signing_key_path = cli.gateway_signing_key_file;
    let prev_signing_key_path = cli.gateway_prev_signing_key_file;
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

    // STRENGTH guard. At real boot `stash_signing_key` is the resolved value,
    // so the length/sentinel checks apply to the real material. During
    // `--check-config` the local is still the raw reference string (we only
    // format-validated it above); a reference's text is not the secret, so
    // running a strength check on it would wrongly fail — skip it then.
    if !cli.check_config || !zeroship_core::config::is_secret_ref(&stash_signing_key) {
        if let Err(message) = validate_stash_key(&stash_signing_key, insecure_dev) {
            tracing::error!(error = %message, "gateway: refusing to start with unsafe stash signing key");
            std::process::exit(1);
        }
    }
    let stash_signing_key = if stash_signing_key.is_empty() {
        DEV_STASH_SIGNING_KEY.to_string()
    } else {
        stash_signing_key
    };

    // STRENGTH guard for the dedicated pairwise-salt secret. Same posture as
    // the stash key: skip the strength check when `--check-config` still holds a
    // raw secret reference (its text is not the secret). Outside dev a missing /
    // weak / dev-default salt aborts boot — the per-app `pws_` anchor must be a
    // strong, stable, operator-set secret.
    if !cli.check_config || !zeroship_core::config::is_secret_ref(&pairwise_salt) {
        if let Err(message) =
            zeroship_core::config::validate_pairwise_salt(&pairwise_salt, insecure_dev)
        {
            tracing::error!(error = %message, "gateway: refusing to start with unsafe pairwise salt");
            std::process::exit(1);
        }
    }
    // Keep the operator-supplied `pairwise_salt` String intact (the check-config
    // report reads it pre-dev-default); derive the effective secret separately.
    let pairwise_salt_secret = if pairwise_salt.is_empty() {
        DEV_PAIRWISE_SALT.to_string()
    } else {
        pairwise_salt.clone()
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

    // Load the gateway's session-cookie signing key. The flag is optional:
    // when empty, the boot succeeds but the signed session cookie cannot be
    // issued/verified, so the cookie auth arm fails closed. We log a clear
    // warning so operators don't get a surprise.
    let signing_key: Option<Arc<ed25519_dalek::SigningKey>> = if signing_key_path.is_empty() {
        tracing::warn!(
            "GATEWAY_SIGNING_KEY_FILE not set — signed session cookies disabled (cookie auth fails closed)"
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

    // auth-sdk Slice 1b-browser — load the PREVIOUS session-cookie signing key
    // for the rotation overlap (§8.5). Set ONLY during a key roll. When present,
    // the session Verifier is built via `Verifier::with_previous` (accepts
    // session cookies signed by EITHER key). Ignored (with a warning) when no
    // current key is configured, since there is nothing to overlap with.
    let prev_signing_key: Option<Arc<ed25519_dalek::SigningKey>> =
        if prev_signing_key_path.is_empty() {
            None
        } else if signing_key.is_none() {
            tracing::warn!(
                "GATEWAY_PREV_SIGNING_KEY_FILE set but no current signing key — ignoring \
                 (a previous key needs a current key to overlap with)"
            );
            None
        } else {
            let key = signing::load_from_path(std::path::Path::new(&prev_signing_key_path))
                .expect("gateway: load previous signing key");
            let kid = signing::jwk_thumbprint(&key);
            tracing::info!(
                path = %prev_signing_key_path,
                kid = %kid,
                "gateway PREVIOUS signing key loaded (rotation overlap active)"
            );
            Some(Arc::new(key))
        };

    // BFF redesign slice R1b — the SIGNED STATELESS session cookie. Built from
    // the ed25519 signing key (+ previous key for the rotation overlap),
    // stamping the distinct `zeroship-sess+jwt` typ.
    // Both `Some`, or both `None` (one-to-one with `signing_key`): with no key
    // the gateway cannot sign/verify the session cookie, so the cookie arm fails
    // closed. The Issuer always signs with the CURRENT key; the Verifier folds
    // in the previous key during an overlap so a cookie minted just before a
    // roll still verifies for its ~15 min life.
    let session_issuer: Option<Arc<session_token::Issuer>> = signing_key.as_ref().map(|sk| {
        let issuer = session_token::Issuer::new(sk.as_ref(), public_url.clone())
            .expect("session_token::Issuer construction");
        Arc::new(issuer)
    });
    let session_verifier: Option<Arc<session_token::Verifier>> = signing_key.as_ref().map(|sk| {
        let current = sk.verifying_key();
        let verifier = match prev_signing_key.as_ref() {
            Some(prev) => session_token::Verifier::with_previous(
                &current,
                &prev.verifying_key(),
                public_url.clone(),
            ),
            None => session_token::Verifier::new(&current, public_url.clone()),
        };
        Arc::new(verifier)
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
        // Report whether the OPERATOR explicitly supplied a salt (pre-dev-
        // default), matching control — so a dev run with no salt reads "(unset)"
        // rather than masking the missing config behind the dev default.
        report.field(
            "pairwise_salt_configured",
            CheckValue::Secret(!pairwise_salt.is_empty()),
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

    // Postgres connection config for the per-origin session store and the
    // anchor/revocation read/write paths. The binary accepts an empty
    // DSN (`--db ""`) for dev / smoke modes that don't exercise the OIDC
    // RP path; downstream handlers gracefully return 401 when `db` is
    // None instead of panicking.
    //
    // `GateState.db` carries only the `Send + Sync` connection params: the
    // compio-postgres `Pool` is `!Send`, so the real pool is built lazily
    // **per ntex worker thread** in a thread-local (see `crate::db`).
    // Every per-request DB touch checks out a pooled connection for ONE
    // operation and releases it on drop, so no single shared connection
    // serializes gateway DB work.
    //
    // `dpop_jti_cache` keeps its own dedicated single connection: the
    // `PgJtiCache` type (in zeroship-core) owns an `Arc<Client>` (which
    // *is* `Send + Sync`), and its DPoP replay-insert path is unchanged
    // by this slice — so it stays exactly as it was before the pool
    // migration.
    let (db, dpop_jti_cache): (
        Option<zeroship_gateway::db::DbConfig>,
        zeroship_core::dpop::TieredJtiCache,
    ) = if pg_dsn.is_empty() {
        tracing::warn!(
            "DATABASE_URL not set — gateway session validation disabled (all auth-gated requests will 401)"
        );
        (None, zeroship_core::dpop::TieredJtiCache::default())
    } else {
        let db_cfg = zeroship_gateway::db::DbConfig::new(pg_dsn.clone(), db_pool_size);
        tracing::info!(
            db_pool_size = db_cfg.pool_size(),
            "gateway pg connection pool configured (per-worker)"
        );

        // Dedicated single connection for the DPoP jti replay cache.
        let (jti_client, jti_conn) = compio_postgres::connect(&pg_dsn, compio_postgres::NoTls)
            .await
            .expect("gateway: pg connect (dpop jti cache)");
        compio::runtime::spawn(async move {
            if let Err(e) = jti_conn.run().await {
                tracing::error!(error = %e, "gateway/pg dpop-jti connection ended");
            }
        })
        .detach();
        let pg = zeroship_core::dpop::PgJtiCache::new(Arc::new(jti_client));
        let dpop_jti_cache = zeroship_core::dpop::TieredJtiCache::with_pg(pg);

        (Some(db_cfg), dpop_jti_cache)
    };

    // OIDC RP — services every `{app}.zeroship.ai` host. The
    // `client_id` matches the entry registered in
    // `ops/auth-clients.example.toml`; `redirect_uri` is per-app and
    // built at the dispatch site.
    let stash_signing_key_bytes = stash_signing_key.into_bytes();

    // auth-sdk Slice 1b-anchors — AES-256-GCM key for the server-held refresh
    // family at rest in `auth.app_session_anchors.refresh_token_enc` (§8.1).
    // Derived from the (server-only) stash signing key via
    // `core::crypto::derive_key` so no new CLI flag is needed and the
    // refresh family never sits in PG in plaintext. Domain-separated by the
    // derive prefix; rotating the stash key rotates this key too (acceptable
    // pre-launch — a roll just forces re-login, which the anchor design
    // already tolerates via Hydra invalid_grant → login_required).
    let anchor_enc_key = {
        let seed = format!(
            "anchor-refresh-enc:{}",
            String::from_utf8_lossy(&stash_signing_key_bytes)
        );
        zeroship_core::crypto::derive_key(&seed)
    };

    // auth-sdk §6.2 — platform-wide pairwise salt for the per-app `pws_…`
    // subject projection. Derived via the SHARED helper so the gateway and the
    // control plane (which revokes the per-app token family on a dashboard
    // "disconnect app", Batch A fix 4) produce byte-identical `pws_…` subjects.
    // Seeded from the DEDICATED `PAIRWISE_SALT` secret (NOT the rotatable stash
    // key): `pws_` is the PERMANENT per-app identity anchor that apps store as a
    // user FK, so its seed must be independent of operational-key rotation. The
    // SAME `PAIRWISE_SALT` value must be configured on gateway + control.
    // Domain-separated from `anchor_enc_key` by the helper's distinct prefix.
    let pairwise_salt = zeroship_core::auth::derive_pairwise_salt(pairwise_salt_secret.as_bytes());

    let oidc_rp = Arc::new(oidc_rp::OidcRp::new(
        &auth_ui_url,
        "gateway",
        oidc_client_secret,
        stash_signing_key_bytes,
    ));

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
        revocation_cache: Arc::new(zeroship_core::wrapper_revocation::RevocationCache::new()),
        signing_key,
        prev_signing_key,
        session_issuer,
        session_verifier,
        anchor_enc_key,
        pairwise_salt,
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
            // auth-sdk BFF redesign slice R1b — the ONE identity-session
            // resource. `/token` is GONE (merged here); both methods live on
            // `/__zeroship/auth/session`:
            //   - POST = code→token exchange + create anchor + ISSUE the signed
            //     session cookie + return `{ user, expires_at }`.
            //   - GET[?mint=1] = decode the live signed cookie, or (expired /
            //     `mint=1`) re-sign a fresh cookie from the server-held anchor.
            // Registered BEFORE the subdomain catch-all. Same-origin-only (no
            // CORS); `?mint=1` + POST additionally require `X-ZS-Auth`.
            .service(
                web::resource("/__zeroship/auth/session")
                    .route(web::post().to(auth_token::session_post))
                    .route(web::get().to(auth_token::session)),
            )
            // auth-sdk Slice 1b-browser — the browser-facing auth HTTP
            // surface. Same mounting discipline (BEFORE the subdomain
            // catch-all). `/authorize` 302s to Hydra (the one cross-site
            // hop); `/popup-callback` serves the same-origin relay page;
            // `/signout` revokes + clears (fixes the live bug).
            .service(
                web::resource("/__zeroship/auth/authorize")
                    .route(web::get().to(browser_auth::authorize)),
            )
            .service(
                web::resource("/__zeroship/auth/popup-callback")
                    .route(web::get().to(browser_auth::popup_callback)),
            )
            .service(
                web::resource("/__zeroship/auth/signout")
                    .route(web::post().to(browser_auth::signout)),
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

    // --- secret-reference resolver wiring (server-config) ---

    // (a) A literal secret passes through the resolver byte-identically.
    // This is the guarantee that wiring the resolver does not change
    // behavior for the existing literal-secret deployments.
    #[test]
    fn literal_secret_resolves_to_itself() {
        let literal = "super-secret-control-key-value";
        assert_eq!(
            zeroship_core::config::resolve_secret(literal).expect("literal resolves"),
            literal,
        );
        // A colon-laden DSN literal must NOT be mistaken for a reference.
        let dsn = "postgres://user:pass@host:5432/db";
        assert_eq!(
            zeroship_core::config::resolve_secret(dsn).expect("dsn resolves"),
            dsn,
        );
    }

    // (b) The exact boolean the stash-key strength guard is gated on. In
    // check-config, a REFERENCE skips the strength guard (the local is the
    // raw ref string, not the material) while a LITERAL still runs it.
    // Mirrors `!cli.check_config || !is_secret_ref(&stash_signing_key)`.
    #[test]
    fn check_config_ref_skips_strength_guard_literal_still_runs() {
        let check_config = true;
        // A short literal in check-config: guard must RUN (and would reject it).
        let literal = "short";
        let runs_guard = !check_config || !zeroship_core::config::is_secret_ref(literal);
        assert!(runs_guard, "literal in check-config must run the strength guard");
        assert!(
            validate_stash_key(literal, false).is_err(),
            "the short literal would fail the guard once it runs"
        );

        // A reference in check-config: guard must be SKIPPED (the raw ref
        // string `urn:…` is not the secret material and would wrongly fail
        // the length check).
        let reference = "urn:zeroship:env:STASH_SIGNING_KEY";
        let runs_guard = !check_config || !zeroship_core::config::is_secret_ref(reference);
        assert!(!runs_guard, "reference in check-config must skip the strength guard");

        // Outside check-config the guard always runs, ref or not.
        let check_config = false;
        let runs_guard = !check_config || !zeroship_core::config::is_secret_ref(reference);
        assert!(runs_guard, "outside check-config the guard always runs");
    }

    // (c) A malformed reference is rejected by the format validator used on
    // the check-config path (validate_secret_ref_or_exit calls this).
    #[test]
    fn malformed_secret_ref_is_rejected() {
        assert!(
            zeroship_core::config::validate_secret_ref("urn:zeroship:nope:x").is_err(),
            "an unrecognized urn: scheme must be rejected"
        );
        assert!(
            zeroship_core::config::validate_secret_ref("urn:zeroship:env:").is_err(),
            "a recognized scheme with an empty body must be rejected"
        );
        // A well-formed reference and a plain literal both pass format check.
        zeroship_core::config::validate_secret_ref("urn:zeroship:env:MY_VAR")
            .expect("well-formed ref is format-valid");
        zeroship_core::config::validate_secret_ref("a-plain-literal")
            .expect("a literal is format-valid");
    }

    // (d) `[secrets]` file tier — the gateway maps control_key/worker_key/
    // database_url/gateway_oidc_secret/stash_signing_key through
    // `obtain_secret`. When the CLI/env value is empty, a `[secrets]` file
    // reference is used; when both are present, the CLI/env value WINS.
    // Asserted directly against the public `obtain_secret` (the exact helper
    // each gateway field now calls) so the precedence contract is pinned
    // without standing up a full process boot.
    #[test]
    fn secrets_file_tier_used_when_cli_empty() {
        // Empty CLI + a `[secrets]` env-reference => the reference resolves.
        let var = format!("ZEROSHIP_GW_SECRETS_TIER_{}", std::process::id());
        std::env::set_var(&var, "resolved-from-secrets-file");
        let reference = format!("urn:zeroship:env:{var}");
        let out = zeroship_core::config::obtain_secret(
            "MASTER_KEY",
            "",
            Some(&reference),
            false,
        );
        std::env::remove_var(&var);
        assert_eq!(
            out, "resolved-from-secrets-file",
            "an empty CLI value must fall through to the [secrets] file reference"
        );
    }

    #[test]
    fn cli_env_secret_beats_secrets_file_entry() {
        // A non-empty CLI/env value WINS over any `[secrets]` file reference:
        // the file reference is never even resolved (note the env var below is
        // intentionally never set — if precedence were wrong, resolving the
        // ref would fail/exit instead of returning the literal).
        let out = zeroship_core::config::obtain_secret(
            "MASTER_KEY",
            "literal-from-cli",
            Some("urn:zeroship:env:ZEROSHIP_GW_SECRETS_TIER_NEVER_SET"),
            false,
        );
        assert_eq!(
            out, "literal-from-cli",
            "a CLI/env secret must win over the [secrets] file entry"
        );
    }
}
