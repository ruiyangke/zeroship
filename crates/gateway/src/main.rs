#![recursion_limit = "256"]

//! `zeroship-gate` binary entry point. Thin shell over the
//! [`zeroship_gateway`] library: parse flags, build [`GateState`],
//! register routes, run.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use ntex::web;
use zeroship_core::config::{
    bootstrap_or_exit, require_nonempty, validate_stash_key, CheckConfigReport, CheckValue,
};
use zeroship_bundle::{build_blob_store, BlobStore, StoreUrl};
use zeroship_gateway::config::{GateSettings, GateSettingsSources};
use zeroship_gateway::{
    auth_token, backchannel_logout, blob_cache, browser_auth, enforce, idempotency, oidc_rp, proxy,
    router, session_token, signal_ingress, signing, sync, GateConfig, GateState,
};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// zeroship gateway startup configuration.
///
/// Only the credential-bearing fields remain here; every operational value is
/// generated in `zeroship_gateway::config`.
#[derive(Parser)]
#[command(name = "zeroship-gate")]
struct GateCli {
    /// Admin/control API shared secret.
    #[arg(long = "control-key", env = "CONTROL_KEY", default_value = "", hide_env_values = true)]
    control_key: String,

    /// Shared secret for worker admin endpoints.
    #[arg(long = "worker-key", env = "WORKER_KEY", default_value = "", hide_env_values = true)]
    worker_key: String,

    /// `PostgreSQL` DSN for gateway session validation.
    #[arg(long = "db", env = "DATABASE_URL", default_value = "", hide_env_values = true)]
    db: String,

    /// PEM/PKCS#8 signing key file for the gateway-signed session cookie.
    #[arg(
        long = "signing-key-file",
        env = "GATEWAY_SIGNING_KEY_FILE",
        default_value = ""
    )]
    gateway_signing_key_file: String,

    /// PEM/PKCS#8 PREVIOUS signing key file for the session-cookie rotation
    /// overlap (auth-sdk 8.5). Set ONLY during a key roll: the Verifier
    /// then accepts session cookies signed by EITHER the current or this
    /// previous key. The Issuer always signs with the current key only.
    /// Empty (default) means a single-key Verifier.
    #[arg(
        long = "prev-signing-key-file",
        env = "GATEWAY_PREV_SIGNING_KEY_FILE",
        default_value = ""
    )]
    gateway_prev_signing_key_file: String,

    /// File containing the shared platform broker master secret.
    ///
    /// Must contain the same raw bytes as auth's `AUTH_BROKER_SECRET_FILE`.
    /// The gateway derives per-app `oac_` client secrets from this material
    /// when brokering authorization-code, refresh, and revoke requests to the
    /// platform OP.
    #[arg(
        long = "gateway-broker-secret-file",
        env = "GATEWAY_BROKER_SECRET_FILE",
        default_value = ""
    )]
    gateway_broker_secret_file: String,

    /// HMAC key for short-lived OIDC stash cookies.
    #[arg(
        long = "stash-signing-key",
        env = "STASH_SIGNING_KEY",
        default_value = "",
        hide_env_values = true
    )]
    stash_signing_key: String,

    /// Dedicated PERMANENT pairwise-salt secret (value). The seed for every
    /// app's `pws_` per-app identity anchor (auth-sdk 6.2) - independent of
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

    /// Every operational value, generated from one declaration in
    /// `zeroship_gateway::config`.
    #[command(flatten)]
    settings: GateSettingsSources,
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
/// A configured-but-unreadable file is fatal; a misconfigured salt must fail
/// loudly rather than silently falling through to another input tier.
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

fn load_gateway_broker_secret_file(path: &str) -> oidc_rp::BrokerSecret {
    if path.is_empty() {
        tracing::error!(
            "gateway: refusing to start without GATEWAY_BROKER_SECRET_FILE / --gateway-broker-secret-file"
        );
        std::process::exit(1);
    }
    let bytes = std::fs::read(path).unwrap_or_else(|e| {
        tracing::error!(
            error = %e,
            path = %path,
            "gateway: cannot read GATEWAY_BROKER_SECRET_FILE"
        );
        std::process::exit(1);
    });
    oidc_rp::BrokerSecret::from_bytes(bytes).unwrap_or_else(|message| {
        let message = message.replace(
            "AUTH_BROKER_SECRET_FILE",
            "GATEWAY_BROKER_SECRET_FILE / --gateway-broker-secret-file",
        );
        tracing::error!(
            error = %message,
            "gateway: refusing to start with unsafe broker master secret"
        );
        std::process::exit(1);
    })
}

fn main() -> std::io::Result<()> {
    let cli = GateCli::parse();
    let (settings, boot) = bootstrap_or_exit::<GateSettings>(
        cli.settings,
        zeroship_gateway::config::DEFAULT_LOG_FILTER,
        "gateway",
    );
    let check_config = *settings.check_config.get();
    let file = &boot.overlay.config;
    // `[secrets]` file-tier overlay — bound ONCE before any secret resolution.
    // The gateway never partially moves `boot.overlay.config`, so a reference
    // is sufficient (no clone needed). Precedence per field: CLI/env > this
    // reference-only file tier > default, applied by `obtain_secret`.
    let file_secrets = &file.secrets;

    let origin_scheme = *settings.origin_scheme.get();
    let trusted_origins = settings.trusted_origins.get().clone();
    let trust_proxy = *settings.trust_proxy.get();
    let port = *settings.port.get();
    let bind_host = settings.bind.get().clone();
    let control_url = settings.control_url.get().clone();
    // Refuse a scheme this transport cannot honour, the same way `--blob-store`
    // below refuses a store URL it cannot parse. Without this an `https://`
    // control URL is silently downgraded to plaintext on port 80 and the
    // control key goes out in the clear.
    if let Err(e) = zeroship_gateway::sync::validate_control_url(&control_url) {
        eprintln!("gateway: invalid --control-url: {e}");
        std::process::exit(2);
    }
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
        check_config,
    );
    // M7 — parse the worker URL list ONCE (rejecting empty/whitespace
    // entries) and reuse it for both the check-config count and the
    // runtime hash ring, so the two can never disagree.
    let worker_urls = parse_worker_urls(settings.worker_urls.get());
    let poll_interval = *settings.poll_interval.get();
    let worker_key = zeroship_core::config::obtain_secret(
        "WORKER_KEY / --worker-key",
        &cli.worker_key,
        file_secrets.worker_key.as_deref(),
        check_config,
    );
    let blob_store_root = settings.blob_store.get().clone();
    // Classify the `--blob-store` value: `s3://…` → remote S3, bare path →
    // local disk (dev default). An `s3://` URL is validated now so a
    // misconfiguration fails fast at startup / check-config.
    let store_url = match StoreUrl::parse(&blob_store_root) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("gateway: invalid --blob-store: {e}");
            std::process::exit(2);
        }
    };
    let blob_store_is_remote = store_url.is_remote();
    let blob_cache_mem_mb = *settings.blob_cache_mem_mb.get();
    let blob_cache_disk_gb = *settings.blob_cache_disk_gb.get();
    let blob_cache_disk_root = settings.blob_cache_disk_root.get().clone();
    let auth_ui_url = settings.auth_ui_url.get().clone();
    let db_pool_size = (*settings.db_pool_size.get()).max(1);
    // DSN carries the database password, so it is resolved like any other
    // secret (literals — including colon-laden DSNs — pass through unchanged).
    let pg_dsn = zeroship_core::config::obtain_secret(
        "DATABASE_URL / --db",
        &cli.db,
        file_secrets.database_url.as_deref(),
        check_config,
    );
    let stash_signing_key = zeroship_core::config::obtain_secret(
        "STASH_SIGNING_KEY / --stash-signing-key",
        &cli.stash_signing_key,
        file_secrets.stash_signing_key.as_deref(),
        check_config,
    );
    // Dedicated pairwise-salt secret. A `--pairwise-salt-file` path wins over
    // the inline `--pairwise-salt`/`PAIRWISE_SALT` value (and over the config
    // overlay reference), so prod can keep the value out of the process table.
    let pairwise_salt = resolve_pairwise_salt(
        &cli.pairwise_salt_file,
        &cli.pairwise_salt,
        file_secrets.pairwise_salt.as_deref(),
        check_config,
    );
    // File-PATH field (names a file to read), NOT a secret value — left
    // unresolved; the signing key is loaded from this path below.
    let signing_key_path = cli.gateway_signing_key_file;
    let prev_signing_key_path = cli.gateway_prev_signing_key_file;
    let broker_secret_path = cli.gateway_broker_secret_file;
    let public_url = settings.public_url.get().clone();

    if let Err(message) = require_nonempty("CONTROL_KEY / --control-key", &control_key) {
        tracing::error!(error = %message, "gateway: refusing to start without control key");
        std::process::exit(1);
    }

    let broker_secret = load_gateway_broker_secret_file(&broker_secret_path);

    // STRENGTH guard. At real boot `stash_signing_key` is the resolved value,
    // so the length/sentinel checks apply to the real material. During
    // `--check-config` the local is still the raw reference string (we only
    // format-validated it above); a reference's text is not the secret, so
    // running a strength check on it would wrongly fail — skip it then.
    if !check_config || !zeroship_core::config::is_secret_ref(&stash_signing_key) {
        if let Err(message) = validate_stash_key(&stash_signing_key) {
            tracing::error!(error = %message, "gateway: refusing to start with unsafe stash signing key");
            std::process::exit(1);
        }
    }

    // STRENGTH guard for the dedicated pairwise-salt secret. Same posture as
    // the stash key: skip the strength check when `--check-config` still holds a
    // raw secret reference (its text is not the secret). A missing or weak salt
    // aborts boot: the per-app `pws_` anchor must be a strong, stable,
    // operator-set secret.
    if !check_config || !zeroship_core::config::is_secret_ref(&pairwise_salt) {
        if let Err(message) = zeroship_core::config::validate_pairwise_salt(&pairwise_salt) {
            tracing::error!(error = %message, "gateway: refusing to start with unsafe pairwise salt");
            std::process::exit(1);
        }
    }
    let pairwise_salt_secret = pairwise_salt.clone();

    // S3 — symmetric WORKER_KEY enforcement. The worker refuses a
    // non-loopback bind without a key; the gateway is the caller of those
    // worker admin endpoints, so it must fail just as hard rather than
    // shipping `Authorization: Bearer ` (empty) into a cluster that
    // believes dispatch is authenticated.
    if let Err(message) = require_nonempty("WORKER_KEY / --worker-key", &worker_key) {
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

    // Load the PREVIOUS session-cookie signing key for the rotation overlap.
    // Set ONLY during a key roll. When present,
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

    if check_config {
        let log_format = boot.log_format.to_string();
        let mut report = CheckConfigReport::new();
        report.field("port", CheckValue::Count(usize::from(port)));
        report.field("bind", CheckValue::Plain(bind_host.clone()));
        report.field(
            "config_source",
            CheckValue::Plain(boot.overlay.source.to_string()),
        );
        report.field("control_url", CheckValue::Plain(control_url));
        report.field("auth_ui_url", CheckValue::Plain(auth_ui_url));
        report.field("origin_scheme", CheckValue::Plain(origin_scheme.to_string()));
        report.field(
            "trusted_origins_count",
            CheckValue::Count(trusted_origins.len()),
        );
        report.field("log_filter", CheckValue::Plain(boot.log_filter.clone()));
        report.field("log_format", CheckValue::Plain(log_format));
        report.field("trust_proxy", CheckValue::Flag(trust_proxy));
        report.field("blob_store", CheckValue::Plain(blob_store_root));
        report.field("blob_store_remote", CheckValue::Flag(blob_store_is_remote));
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
        report.field(
            "gateway_broker_secret_file_configured",
            CheckValue::Secret(!broker_secret_path.is_empty()),
        );
        // Report whether the operator supplied a salt without revealing it.
        report.field(
            "pairwise_salt_configured",
            CheckValue::Secret(!pairwise_salt.is_empty()),
        );
        report.emit(*settings.check_config_format.get());
        return Ok(());
    }

    ntex::rt::System::build()
        .name("zeroship-gate")
        .build(ntex::rt::DefaultRuntime)
        .block_on(async move {
    let blob_cache_bytes: usize = blob_cache_mem_mb.saturating_mul(1024 * 1024);
    let disk_cache_bytes: u64 = blob_cache_disk_gb.saturating_mul(1024 * 1024 * 1024);
    // Read here rather than inside `zeroship-bundle`, so the record names the
    // gateway as the reader. Only a remote store needs credentials.
    let s3_runtime = store_url.is_remote().then(|| {
        zeroship_core::resolve_s3_runtime!(zeroship_gateway::config::GateSettingsConsumer)
            .expect("failed to resolve S3 credentials for the blob store")
    });
    let blob_store: Arc<dyn BlobStore> = build_blob_store(&store_url, s3_runtime.as_ref())
        .expect("failed to initialise blob store");
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
    let db: Option<zeroship_gateway::db::DbConfig> = if pg_dsn.is_empty() {
        tracing::warn!(
            "DATABASE_URL not set — gateway session validation disabled (all auth-gated requests will 401)"
        );
        None
    } else {
        let db_cfg = zeroship_gateway::db::DbConfig::new(pg_dsn.clone(), db_pool_size);
        tracing::info!(
            db_pool_size = db_cfg.pool_size(),
            "gateway pg connection pool configured (per-worker)"
        );
        Some(db_cfg)
    };

    // OIDC RP — services every `{app}.zeroship.ai` host. The
    // `client_id` must match the client this gateway host is registered
    // as with the OP; `redirect_uri` is per-app and built at the
    // dispatch site.
    let stash_signing_key_bytes = stash_signing_key.into_bytes();

    // AES-256-GCM key for the server-held refresh family at rest in
    // `zeroship.app_session_anchors.refresh_token_enc`.
    // Derived from the (server-only) stash signing key via
    // `core::crypto::derive_key` so no new CLI flag is needed and the
    // refresh family never sits in PG in plaintext. Domain-separated by the
    // derive prefix; rotating the stash key rotates this key too (acceptable
    // pre-launch — a roll just forces re-login, which the anchor design
    // already tolerates via OP invalid_grant → login_required).
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
        broker_secret,
        stash_signing_key_bytes,
    ));

    // ── Metering infrastructure (coverage #27) ───────────────────────────
    // The gateway is a SECOND usage producer. ONE process-wide meter, shared
    // into `GateState` (so the response path records `gateway_egress_bytes` for
    // static/redirect/error bodies the worker never sees) AND drained by the
    // usage outbox spawned just below. Mirrors the worker: same `Meter`, same
    // shared `build_usage_outbox` producer → the billing stream. The
    // `gate-…-<uuid>` source is unique per boot so gateway and worker producer
    // ids never collide; their events simply SUM in `usage_aggregates`.
    // `gate_base` is the part that must stay STABLE across restarts, because
    // the WAL is named for it. The uuid below is the part that must CHANGE per
    // boot, because two live producers must not share a client id. They were
    // one string until the WAL turned out to be keyed on the changing half.
    let gate_base = zeroship_core::declared_env!(external, "HOSTNAME", zeroship_gateway::config::GateSettingsConsumer)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("gate-{port}"));
    let gate_meter_source = format!("gate-{gate_base}-{}", uuid::Uuid::new_v4());
    let gate_wal = zeroship_metering::wal_identity("gate", &gate_base);
    let meter = Arc::new(zeroship_metering::Meter::with_source(gate_meter_source.clone()));

    let state = Arc::new(GateState {
        config: GateConfig {
            control_url,
            control_key,
            worker_urls,
            poll_interval_secs: poll_interval,
            worker_key,
            auth_ui_url,
            origin_scheme,
            trusted_origins,
            trust_proxy,
            public_url,
        },
        routes: sync::RouteCache::new(),
        hash_ring,
        // TODO(S5 throughput backstop, billing-provider-platform design v7
        // Pillar 4/5): make this plan-aware so FREE-tier apps get a tighter
        // default per-app cap. The current registry is global; wiring the
        // route's plan into limiter defaults belongs in the gateway config
        // slice, not in the usage-aggregate recompute writer.
        rate_limiters: enforce::RateLimitRegistry::new(1000, 2000),
        per_rule_rate_limits: enforce::PerRuleRateLimitRegistry::new(),
        concurrency: enforce::ConcurrencyRegistry::new(100),
        blob_store,
        blob_cache: blob_cache::BlobCache::new(blob_cache_bytes),
        disk_cache,
        idempotency_store: Arc::new(idempotency::InMemoryIdempotencyStore::new()),
        oidc_rp,
        db,
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
        revocation_cache: Arc::new(zeroship_authz::wrapper_revocation::RevocationCache::new()),
        signing_key,
        prev_signing_key,
        session_issuer,
        session_verifier,
        anchor_enc_key,
        pairwise_salt,
        meter: Arc::clone(&meter),
    });

    sync::start_sync(state.clone());

    let bind_addr = format!("{bind_host}:{port}");

    // Spawn the gateway usage-event outbox (coverage #27). Drains the gateway's
    // meter every ~10s and publishes UsageEvents to the billing stream via the
    // SAME shared producer wiring as the worker. Disabled (drain-and-drop) when
    // REDPANDA_BROKERS is unset. Detached — never on the proxy hot path.
    // Env wins, the `[metering]` file overlay back-fills (config-file driven).
    let fm = &boot.overlay.config.metering;
    let gate_stream_settings = zeroship_metering::UsageStreamSettings::from_env().or(
        zeroship_metering::UsageStreamSettings {
            brokers: fm.redpanda_brokers.clone(),
            topic: fm.usage_events_topic.clone(),
            group_id: fm.producer_group_id.clone(),
            wal_path: fm.outbox_wal_path.clone(),
        },
    );
    match zeroship_metering::build_usage_outbox(
        &gate_meter_source,
        &gate_wal,
        &gate_stream_settings,
    ) {
        Ok(Some((outbox, outbox_config))) => {
            let topic = outbox.topic().to_string();
            zeroship_metering::spawn_outbox_task(Arc::clone(&meter), outbox, outbox_config);
            tracing::info!(producer = %gate_meter_source, topic = %topic, "gateway usage-event outbox started");
        }
        Ok(None) => {
            zeroship_metering::spawn_disabled_drain_task(
                Arc::clone(&meter),
                zeroship_metering::DEFAULT_OUTBOX_INTERVAL,
                "REDPANDA_BROKERS is not set".to_string(),
            );
        }
        // FATAL, matching the worker. Brokers are configured, so the operator
        // intends this gateway to bill; the common cause on a stable WAL path
        // is a co-located producer holding the single-writer redb lock. The
        // old arm degraded to a drain-and-drop task, which loses every event
        // for the life of the process - permanent total loss substituted for
        // intermittent partial loss.
        Err(error) => {
            tracing::error!(
                producer = %gate_meter_source,
                wal = %gate_wal.as_str(),
                error = %error,
                "gateway usage outbox could not be built; refusing to boot \
                 rather than dropping billable usage"
            );
            return Err(std::io::Error::other(format!(
                "gateway usage outbox could not be built (wal={}): {error}",
                gate_wal.as_str()
            )));
        }
    }
    tracing::info!(bind = %bind_addr, "gateway listening");

    web::server(async move || {
        web::App::new()
            .state(state.clone())
            .service(
                // ntex's `{path:.*}` only matches a single segment;
                // `{tail}*` is the tail-match syntax that handles
                // nested asset paths like `assets/index-abc.js`.
                // The creator-app body cap, shared with the worker. Without an
                // explicit PayloadConfig ntex applies its own 256 KiB default,
                // which would cap every creator app at a sixteenth of the
                // documented limit and answer with a bare framework 400 that
                // names neither the limit nor the tier that imposed it.
                web::resource("/apps/{app_name}/{tail}*")
                    .state(web::types::PayloadConfig::new(
                        zeroship_core::dispatch_frame::MAX_REQUEST_BODY_BYTES,
                    ))
                    .route(web::route().to(router::handle)),
            )
            .service(web::resource("/health").route(web::get().to(|| async {
                web::HttpResponse::Ok().body(r#"{"status":"ok"}"#)
            })))
            .service(
                web::resource("/__zeroship/internal/workflow-advance")
                    .route(web::post().to(router::workflow_advance_internal)),
            )
            .service(
                web::resource("/__zeroship/v1/signal")
                    .state(web::types::PayloadConfig::new(
                        signal_ingress::SIGNAL_INGRESS_BODY_BYTES,
                    ))
                    .route(web::post().to(signal_ingress::public_signal_ingress)),
            )
            .service(
                web::resource("/__zeroship/signals/v1")
                    .state(web::types::PayloadConfig::new(
                        signal_ingress::SIGNAL_INGRESS_BODY_BYTES,
                    ))
                    .route(web::post().to(signal_ingress::public_signal_ingress)),
            )
            // The ONE identity-session
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
            // The browser-facing auth HTTP
            // surface. Same mounting discipline (BEFORE the subdomain
            // catch-all). `/authorize` 302s to OP (the one cross-site
            // hop); `/popup-callback` serves the same-origin relay page;
            // `/signout` revokes + clears the session state.
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
            // URI is stable across every registered client's
            // `backchannel_logout_uri`. Must be
            // mounted BEFORE the subdomain catch-all below — ntex's
            // path routing is registration-order-sensitive for
            // overlapping patterns.
            .configure(backchannel_logout::configure)
            // Subdomain catch-all — must be last (lowest priority)
            .service(
                web::resource("/{tail}*")
                    .state(web::types::PayloadConfig::new(
                        zeroship_core::dispatch_frame::MAX_REQUEST_BODY_BYTES,
                    ))
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
    use zeroship_core::config::{GeneratedConfig, OriginScheme, TrustedOrigin};

    static CLI_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn restore_env_var(key: &str, old: Option<std::ffi::OsString>) {
        match old {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn topology_cli_overrides_environment_and_environment_overrides_file() {
        let _guard = CLI_ENV_LOCK.lock().expect("env lock");
        let old_scheme = zeroship_core::test_env_os!("ZEROSHIP_ORIGIN_SCHEME");
        let old_origins = zeroship_core::test_env_os!("ZEROSHIP_TRUSTED_ORIGINS");
        std::env::set_var("ZEROSHIP_ORIGIN_SCHEME", "http");
        std::env::set_var(
            "ZEROSHIP_TRUSTED_ORIGINS",
            "https://env.example,http://localhost:3000",
        );

        // The environment reaches the same carrier the flag does, and the
        // generated resolver then prefers the carrier over the overlay. The
        // hand-written resolve_origin_scheme/resolve_trusted_origins helpers
        // this test used to call are deleted; the precedence they encoded is
        // now the resolver's, asserted here against a REAL overlay.
        let overlay: toml::Value = toml::from_str(
            "origin_scheme = \"https\"\ntrusted_origins = [\"https://file.example\"]\n",
        )
        .expect("fixture overlay");
        let env = GateCli::try_parse_from(["zeroship-gate"]).expect("parse env topology");
        let resolved = GateSettings::resolve_config(env.settings, Some(&overlay))
            .expect("settings resolve");
        assert_eq!(resolved.origin_scheme.get(), &OriginScheme::Http);
        assert_eq!(
            resolved
                .trusted_origins
                .get()
                .iter()
                .map(TrustedOrigin::as_str)
                .collect::<Vec<_>>(),
            vec!["https://env.example", "http://localhost:3000"]
        );

        let cli = GateCli::try_parse_from([
            "zeroship-gate",
            "--origin-scheme",
            "https",
            "--trusted-origins",
            "https://cli.example",
        ])
        .expect("parse CLI topology");
        let flagged = GateSettings::resolve_config(cli.settings, Some(&overlay))
            .expect("settings resolve");
        assert_eq!(flagged.origin_scheme.get(), &OriginScheme::Https);
        assert_eq!(
            flagged.trusted_origins.get()[0].as_str(),
            "https://cli.example"
        );

        restore_env_var("ZEROSHIP_ORIGIN_SCHEME", old_scheme);
        restore_env_var("ZEROSHIP_TRUSTED_ORIGINS", old_origins);
    }

    #[test]
    fn deleted_security_relaxation_flag_is_rejected() {
        let parsed = GateCli::try_parse_from(["zeroship-gate", "--dev-insecure"]);
        let err = match parsed {
            Ok(_) => panic!("deleted --dev-insecure flag must be rejected"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn obsolete_security_relaxation_environment_variable_is_ignored() {
        let _guard = CLI_ENV_LOCK.lock().expect("env lock");
        let old = zeroship_core::declared_env_os!(
            dev,
            "ZEROSHIP_DEV_INSECURE",
            zeroship_gateway::config::GateSettingsConsumer
        );
        std::env::set_var("ZEROSHIP_DEV_INSECURE", "1");
        let parsed = GateCli::try_parse_from(["zeroship-gate"]);
        restore_env_var("ZEROSHIP_DEV_INSECURE", old);

        let Ok(cli) = parsed else {
            panic!("an obsolete environment variable must not affect parsing");
        };
        assert_eq!(cli.settings.origin_scheme, None);
        assert_eq!(cli.settings.trust_proxy, None);
    }

    #[test]
    fn gateway_worker_key_is_required() {
        assert!(require_nonempty("WORKER_KEY / --worker-key", "").is_err());
        assert!(require_nonempty("WORKER_KEY / --worker-key", "key").is_ok());
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
    fn gateway_stash_key_rejects_missing() {
        let err = validate_stash_key("").unwrap_err();
        assert!(err.contains("required"), "{err}");
    }

    #[test]
    fn gateway_control_key_rejects_missing() {
        let err = require_nonempty("CONTROL_KEY / --control-key", "").unwrap_err();
        assert!(err.contains("CONTROL_KEY"), "{err}");
    }

    #[test]
    fn gateway_control_key_accepts_nonempty() {
        assert!(require_nonempty("CONTROL_KEY / --control-key", "secret").is_ok());
    }

    #[test]
    fn gateway_broker_secret_rejects_short_material() {
        let err = oidc_rp::BrokerSecret::from_bytes(b"short".to_vec()).unwrap_err();
        assert!(err.contains("minimum is 32 bytes"), "{err}");
    }

    #[test]
    fn gateway_broker_secret_rejects_dev_sentinel() {
        let err = oidc_rp::BrokerSecret::from_bytes(
            zeroship_core::auth::DEV_BROKER_MASTER_SECRET.to_vec(),
        )
        .unwrap_err();
        assert!(err.contains("dev sentinel"), "{err}");
    }

    #[test]
    fn gateway_broker_secret_accepts_strong_material() {
        oidc_rp::BrokerSecret::from_bytes(
            b"gateway-broker-secret-test-master-32-bytes".to_vec(),
        )
        .expect("strong broker master");
    }

    #[test]
    fn gateway_stash_key_rejects_short() {
        let err = validate_stash_key("short").unwrap_err();
        assert!(err.contains("too short"), "{err}");
    }

    #[test]
    fn gateway_stash_key_accepts_strong() {
        let key = "0123456789abcdef0123456789abcdef";
        assert!(validate_stash_key(key).is_ok());
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
    // Mirrors `!check_config || !is_secret_ref(&stash_signing_key)`.
    #[test]
    fn check_config_ref_skips_strength_guard_literal_still_runs() {
        let check_config = true;
        // A short literal in check-config: guard must RUN (and would reject it).
        let literal = "short";
        let runs_guard = !check_config || !zeroship_core::config::is_secret_ref(literal);
        assert!(runs_guard, "literal in check-config must run the strength guard");
        assert!(
            validate_stash_key(literal).is_err(),
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
    // database_url/stash_signing_key through
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
