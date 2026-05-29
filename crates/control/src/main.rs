//! zeroship-control — control plane binary. Thin wrapper over
//! `zeroship_control` (the library crate).

use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine as _;
use clap::Parser;
use ntex::web;
use zeroship_core::config::{FileConfig, resolve_observability};
use zeroship_bundle::{BlobStore, BundleStore, LocalDiskBlobStore, LocalFs};
use zeroship_control::{
    admin_handlers, api, backchannel_logout, bootstrap_builder, env_handlers, internal,
    oauth_grants_handlers, oauth_handlers, oidc_rp, stripe_handlers, token_handlers, AppState,
    EnvStore, Quota, RateLimiter, Registry, StripeStore,
};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const DEV_HYDRA_PUBLIC_URL: &str = "http://localhost:4444";
const DEV_HYDRA_ADMIN_URL: &str = "http://localhost:4445";
const DEV_CONSOLE_OIDC_SECRET: &str = "dev-console-oidc-secret";
const DEV_STASH_SIGNING_KEY: &str = "dev-stash-key-please-rotate";

/// zeroship control-plane startup configuration.
#[derive(Debug, Parser)]
#[command(name = "zeroship-control")]
struct ControlCli {
    /// HTTP listen port.
    #[arg(long, env = "CONTROL_PORT", default_value = "9090")]
    port: String,

    /// PostgreSQL DSN for control-plane data.
    #[arg(long = "db", env = "DATABASE_URL", default_value = "postgres://localhost/zeroship")]
    db: String,

    /// Root directory for bundles and content-addressed deploy blobs.
    #[arg(long = "blob-store", env = "BLOB_STORE", default_value = "./bundles")]
    blob_store: String,

    /// Admin/control API shared secret.
    #[arg(long = "control-key", env = "CONTROL_KEY", default_value = "", hide_env_values = true)]
    control_key: String,

    /// Master key used for control-plane encrypted env/secrets.
    #[arg(long = "master-key", env = "MASTER_KEY", default_value = "", hide_env_values = true)]
    master_key: String,

    /// Comma-separated worker base URLs.
    #[arg(long = "workers", env = "WORKER_URLS", default_value = "http://localhost:8080")]
    workers: String,

    /// Shared secret for worker admin endpoints.
    #[arg(long = "worker-key", env = "WORKER_KEY", default_value = "", hide_env_values = true)]
    worker_key: String,

    /// PEM/PKCS#8 signing key file for PAT issuance.
    #[arg(long = "signing-key-file", env = "SIGNING_KEY_FILE", default_value = "")]
    signing_key_file: String,

    /// Stripe webhook signing secret.
    #[arg(
        long = "stripe-webhook-secret",
        env = "STRIPE_WEBHOOK_SECRET",
        default_value = "",
        hide_env_values = true
    )]
    stripe_webhook_secret: String,

    /// Comma-separated previous master keys accepted during key rotation.
    #[arg(
        long = "legacy-master-keys",
        env = "LEGACY_MASTER_KEYS",
        default_value = "",
        hide_env_values = true
    )]
    legacy_master_keys: String,

    /// Allow explicitly insecure local development startup.
    #[arg(long = "dev-insecure", action = clap::ArgAction::SetTrue)]
    dev_insecure: bool,

    /// Environment half of `--dev-insecure`; only `1` is truthy.
    #[arg(skip = env_is_exact("ZEROSHIP_DEV_INSECURE", "1"))]
    dev_insecure_env: bool,

    /// Trust `X-Forwarded-For` from an upstream proxy.
    #[arg(long = "trust-proxy", action = clap::ArgAction::SetTrue)]
    trust_proxy: bool,

    /// Environment half of `--trust-proxy`; only `1` is truthy.
    #[arg(skip = env_is_exact("TRUST_PROXY", "1"))]
    trust_proxy_env: bool,

    /// Bootstrap the first-party builder OAuth client at startup.
    #[arg(long = "bootstrap-builder-client", action = clap::ArgAction::SetTrue)]
    bootstrap_builder_client: bool,

    /// Environment half of `--bootstrap-builder-client`; `1`/`true` are truthy.
    #[arg(skip = env_is_1_or_true("BOOTSTRAP_BUILDER_OAUTH_CLIENT"))]
    bootstrap_builder_client_env: bool,

    /// Redirect URI for the bootstrapped builder OAuth client.
    #[arg(
        long = "builder-redirect-uri",
        env = "BUILDER_REDIRECT_URI",
        default_value = bootstrap_builder::DEFAULT_BUILDER_REDIRECT_URI
    )]
    builder_redirect_uri: String,

    /// File path used to persist the bootstrapped builder client secret.
    #[arg(
        long = "builder-client-secret-file",
        env = "BUILDER_CLIENT_SECRET_FILE",
        default_value = bootstrap_builder::DEFAULT_BUILDER_CLIENT_SECRET_PATH
    )]
    builder_client_secret_file: PathBuf,

    /// Directory for in-flight deploy bodies; empty means the OS temp dir.
    #[arg(long = "deploy-tmp-dir", env = "DEPLOY_TMP_DIR", default_value = "")]
    deploy_tmp_dir: String,

    /// Optional shared config overlay path.
    #[arg(long = "config", env = "ZEROSHIP_CONFIG")]
    config_path: Option<PathBuf>,

    /// Observability CLI/env overrides.
    #[command(flatten)]
    obs: zeroship_core::config::ObservabilityFlags,

    /// Hydra admin API base URL.
    #[arg(long = "hydra-admin-url", env = "HYDRA_ADMIN_URL")]
    hydra_admin_url: Option<String>,

    /// Hydra public issuer/base URL used by the console OIDC RP.
    #[arg(long = "hydra-public-url", env = "HYDRA_PUBLIC_URL")]
    hydra_public_url: Option<String>,

    /// Console OIDC client secret.
    #[arg(
        long = "console-oidc-secret",
        env = "CONSOLE_OIDC_SECRET",
        default_value = "",
        hide_env_values = true
    )]
    console_oidc_secret: String,

    /// HMAC key for short-lived OIDC stash cookies.
    #[arg(
        long = "stash-signing-key",
        env = "STASH_SIGNING_KEY",
        default_value = "",
        hide_env_values = true
    )]
    stash_signing_key: String,

    /// PostgreSQL DSN for auth/console session tables.
    #[arg(long = "auth-db", env = "AUTH_DB_URL", default_value = "")]
    auth_db_url: String,

    /// Expected OAuth access-token audience for control bearer auth.
    #[arg(long = "oauth-audience", env = "OAUTH_AUDIENCE", default_value = "control.zeroship.ai")]
    oauth_audience: String,
}

impl ControlCli {
    fn insecure_dev(&self) -> bool {
        self.dev_insecure || self.dev_insecure_env
    }

    fn trust_proxy(&self) -> bool {
        self.trust_proxy || self.trust_proxy_env
    }

    fn bootstrap_builder_client(&self) -> bool {
        self.bootstrap_builder_client || self.bootstrap_builder_client_env
    }
}

fn env_is_exact(key: &str, expected: &str) -> bool {
    std::env::var(key).is_ok_and(|value| value == expected)
}

fn env_is_1_or_true(key: &str) -> bool {
    std::env::var(key).is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

fn resolve_file_overlay_string(
    cli_value: Option<String>,
    file_value: Option<String>,
    insecure_dev: bool,
    dev_default: &str,
) -> String {
    cli_value
        .or(file_value)
        .unwrap_or_else(|| {
            if insecure_dev {
                dev_default.to_string()
            } else {
                String::new()
            }
        })
}

fn decoded_master_key_len(value: &str) -> Option<usize> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    if trimmed.len() % 2 == 0 && trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        if let Ok(bytes) = hex::decode(trimmed) {
            if bytes.len() >= 32 {
                return Some(bytes.len());
            }
        }
    }

    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(trimmed)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(trimmed))
        .ok()
        .map(|bytes| bytes.len())
}

fn validate_master_key_material(
    label: &str,
    value: &str,
    insecure_dev: bool,
) -> Result<(), String> {
    if insecure_dev {
        return Ok(());
    }
    match decoded_master_key_len(value) {
        Some(n) if n >= 32 => Ok(()),
        Some(n) => Err(format!(
            "{label} decodes to {n} bytes; minimum is 32 random bytes"
        )),
        None => Err(format!(
            "{label} must be hex or base64url encoded and decode to at least 32 random bytes"
        )),
    }
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    let cli = ControlCli::parse();
    let file = match FileConfig::load(cli.config_path.as_deref()) {
        Ok(file) => file,
        Err(err) => {
            eprintln!("control: failed to load config file: {err}");
            std::process::exit(1);
        }
    };
    let (filter, format) = resolve_observability(
        &cli.obs,
        &file.observability,
        "info,zeroship_control=debug",
    );
    zeroship_core::observability::init_tracing_with(&filter, format.as_deref());

    let insecure_dev = cli.insecure_dev();
    let trust_proxy = cli.trust_proxy();
    let bootstrap_builder_client = cli.bootstrap_builder_client();

    let hydra_admin_url = resolve_file_overlay_string(
        cli.hydra_admin_url,
        file.auth.hydra_admin_url.clone(),
        insecure_dev,
        DEV_HYDRA_ADMIN_URL,
    );
    let hydra_public_url = resolve_file_overlay_string(
        cli.hydra_public_url,
        file.auth.hydra_public_url.clone(),
        insecure_dev,
        DEV_HYDRA_PUBLIC_URL,
    );
    let trusted_oauth_clients = zeroship_control::resolve_trusted_oauth_clients(&file.auth);

    let port = cli.port;
    let db_url = cli.db;
    let blob_store_root = cli.blob_store;
    let control_key = cli.control_key;
    let master_key = cli.master_key;
    let workers_str = cli.workers;
    let worker_key = cli.worker_key;
    let signing_key_file = cli.signing_key_file;
    let stripe_webhook_secret = cli.stripe_webhook_secret;
    // Comma-separated list of previous master keys, tried as fallbacks
    // on decrypt failure during a rotation grace period.
    let legacy_master_keys_raw = cli.legacy_master_keys;
    let legacy_keys: Vec<&str> = legacy_master_keys_raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let builder_redirect_uri = cli.builder_redirect_uri;
    let builder_client_secret_path = cli.builder_client_secret_file;
    let deploy_tmp_dir_str = cli.deploy_tmp_dir;

    let deploy_tmp_dir: std::path::PathBuf = if deploy_tmp_dir_str.is_empty() {
        std::env::temp_dir()
    } else {
        std::path::PathBuf::from(&deploy_tmp_dir_str)
    };

    // Validate at startup so operators don't discover a misconfigured
    // path on first deploy. We check existence + writability by trying
    // to create the directory tree (idempotent if it already exists)
    // and then writing + removing a probe file.
    if let Err(e) = std::fs::create_dir_all(&deploy_tmp_dir) {
        tracing::error!(
            path = %deploy_tmp_dir.display(),
            error = %e,
            "control: deploy_tmp_dir not creatable, refusing to start",
        );
        std::process::exit(1);
    }
    let probe = deploy_tmp_dir.join(format!(
        ".zeroship-probe-{}",
        uuid::Uuid::new_v4().simple()
    ));
    if let Err(e) = std::fs::write(&probe, b"") {
        tracing::error!(
            path = %deploy_tmp_dir.display(),
            error = %e,
            "control: deploy_tmp_dir not writable, refusing to start",
        );
        std::process::exit(1);
    }
    let _ = std::fs::remove_file(&probe);
    tracing::info!(path = %deploy_tmp_dir.display(), "control: deploy_tmp_dir configured");

    if !insecure_dev {
        let mut missing = Vec::new();
        if master_key.is_empty() {
            missing.push("--master-key / MASTER_KEY");
        }
        if control_key.is_empty() {
            missing.push("--control-key / CONTROL_KEY");
        }
        if signing_key_file.is_empty() {
            missing.push("--signing-key-file / SIGNING_KEY_FILE");
        }
        if !missing.is_empty() {
            tracing::error!(
                missing = %missing.join(", "),
                "control: refusing to start; required secrets missing. \
                 Pass --dev-insecure (or ZEROSHIP_DEV_INSECURE=1) to run \
                 without them — NEVER in production."
            );
            std::process::exit(1);
        }
        if let Err(message) = validate_master_key_material("MASTER_KEY", &master_key, insecure_dev)
        {
            tracing::error!(error = %message, "control: refusing to start with weak MASTER_KEY");
            std::process::exit(1);
        }
        for (idx, legacy_key) in legacy_keys.iter().enumerate() {
            let label = format!("LEGACY_MASTER_KEYS[{idx}]");
            if let Err(message) = validate_master_key_material(&label, legacy_key, insecure_dev) {
                tracing::error!(
                    error = %message,
                    "control: refusing to start with weak legacy master key"
                );
                std::process::exit(1);
            }
        }
        if stripe_webhook_secret.is_empty() {
            // Not fatal — operators may run a control plane without
            // Stripe entirely. But every webhook delivery will reject
            // with 500, so log loudly at startup so a misconfigured
            // deploy isn't noticed only via Stripe-side retries.
            tracing::warn!(
                "control: stripe_webhook_secret unset — /internal/webhooks/stripe will reject every request. \
                 Set --stripe-webhook-secret if you need Stripe integration."
            );
        }
    }
    if insecure_dev {
        tracing::warn!("control: --dev-insecure set; admin + internal auth disabled");
    }

    let pat_issuer = if signing_key_file.is_empty() {
        tracing::warn!("control: using dev-only PAT signing key");
        Arc::new(token_handlers::PatIssuer::dev_insecure())
    } else {
        let signing_key = token_handlers::load_signing_key_from_path(
            std::path::Path::new(&signing_key_file),
        )
        .map_err(|err| {
            tracing::error!(error = %err, "control: failed to load PAT signing key");
            std::io::Error::new(std::io::ErrorKind::InvalidInput, err)
        })?;
        Arc::new(token_handlers::PatIssuer::new(&signing_key).map_err(|err| {
            tracing::error!(error = %err, "control: failed to initialize PAT issuer");
            std::io::Error::new(std::io::ErrorKind::InvalidInput, err)
        })?)
    };

    let registry = Registry::new(&db_url)
        .await
        .expect("failed to connect to database");

    let vfs = Arc::new(
        LocalFs::new(&blob_store_root).expect("failed to initialise bundle store"),
    ) as Arc<dyn BundleStore + Send + Sync>;

    // BlobStore lives alongside the legacy BundleStore on the same
    // root. New `.zship` deploys land in `<blob_store_root>/blobs/` and
    // `<blob_store_root>/manifests/`; legacy `<blob_store_root>/<app_id>/...`
    // files stay where they are until the old BundleStore path is
    // retired.
    let blob_root = PathBuf::from(&blob_store_root);
    let blob_store: Arc<dyn BlobStore> = Arc::new(
        LocalDiskBlobStore::new(blob_root)
            .expect("failed to initialise blob store"),
    );

    if !legacy_keys.is_empty() {
        tracing::info!(
            legacy_keys = legacy_keys.len(),
            "control: EnvStore booted with legacy master keys (rotation grace period)"
        );
    }
    let env_store = EnvStore::new_with_previous(
        registry.clone(),
        &master_key,
        &legacy_keys,
        insecure_dev,
    )
    .expect("env store init");
    let stripe_store = StripeStore::new(registry.clone());

    // Phase 3 U7/U8 — control plane OIDC RP for `console.zeroship.ai`.
    // Mandatory post-U8: the legacy `auth_handlers` / `auth_service`
    // chain has been retired, so the OIDC RP is the only console-auth
    // surface. Control also needs hydra-admin for OAuth bearer
    // introspection. Refuses to boot unless these pieces are configured
    // (`--dev-insecure` permits localhost defaults only).
    let console_oidc_secret = cli.console_oidc_secret;
    let stash_signing_key = cli.stash_signing_key;
    let auth_db_url = cli.auth_db_url;
    let expected_oauth_audience = cli.oauth_audience;

    if !insecure_dev {
        let mut missing = Vec::new();
        if hydra_public_url.is_empty() {
            missing.push("--hydra-public-url / HYDRA_PUBLIC_URL");
        }
        if console_oidc_secret.is_empty() {
            missing.push("--console-oidc-secret / CONSOLE_OIDC_SECRET");
        }
        if stash_signing_key.is_empty() {
            missing.push("--stash-signing-key / STASH_SIGNING_KEY");
        }
        if auth_db_url.is_empty() {
            missing.push("--auth-db / AUTH_DB_URL");
        }
        if hydra_admin_url.is_empty() {
            missing.push("--hydra-admin-url / HYDRA_ADMIN_URL");
        }
        if !missing.is_empty() {
            tracing::error!(
                missing = %missing.join(", "),
                "control: refusing to start; OIDC RP and OAuth introspection require these flags. \
                 Pass --dev-insecure to run with localhost defaults."
            );
            std::process::exit(1);
        }

        // D5: control adopts the shared stash-key strength check (gateway
        // already had it). Empty is reported by the missing-secrets block
        // above; this additionally rejects a present-but-weak key (<32
        // bytes), which also catches the dev sentinel (27 bytes) in prod.
        if let Err(message) =
            zeroship_core::config::validate_stash_key(&stash_signing_key, insecure_dev)
        {
            tracing::error!(error = %message, "control: refusing to start with weak STASH_SIGNING_KEY");
            std::process::exit(1);
        }
    }

    let stash_key_bytes = if stash_signing_key.is_empty() {
        // Dev-only fallback. Ephemeral keys are fine for the 10-minute
        // stash window during local development; in prod the guard
        // above already exited.
        DEV_STASH_SIGNING_KEY.as_bytes().to_vec()
    } else {
        stash_signing_key.into_bytes()
    };
    let console_oidc_secret_value = if console_oidc_secret.is_empty() {
        DEV_CONSOLE_OIDC_SECRET.to_string()
    } else {
        console_oidc_secret
    };
    tracing::info!(
        hydra_public_url = %hydra_public_url,
        "control: console OIDC RP enabled"
    );
    let oidc_rp = Arc::new(oidc_rp::ConsoleOidcRp::new(
        &hydra_public_url,
        "console.zeroship.ai",
        console_oidc_secret_value,
        stash_key_bytes,
    ));
    let hydra_introspector = Arc::new(zeroship_core::hydra::HydraIntrospector::new(
        &hydra_admin_url,
    ));

    let auth_db_url_resolved = if auth_db_url.is_empty() {
        // Dev fallback: reuse the control DB URL so /auth/callback
        // works against a single local Postgres without operator
        // ceremony. Production refused to start without --auth-db
        // above.
        db_url.clone()
    } else {
        auth_db_url
    };
    let auth_pg: Arc<compio_postgres::Client> = {
        let (pg_client, pg_conn) = compio_postgres::connect(&auth_db_url_resolved, compio_postgres::NoTls)
            .await
            .expect("control: auth-pg connect");
        compio::runtime::spawn(async move {
            if let Err(e) = pg_conn.run().await {
                tracing::error!(error = %e, "control/auth-pg connection ended");
            }
        })
        .detach();
        Arc::new(pg_client)
    };

    if bootstrap_builder_client {
        zeroship_auth::store::migrations::migrate(&auth_pg)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "control: auth/control migrations failed");
                std::io::Error::other(err.to_string())
            })?;
        let cfg = bootstrap_builder::BuilderClientBootstrapConfig {
            enabled: true,
            hydra_admin_url: hydra_admin_url.clone(),
            redirect_uri: builder_redirect_uri,
            client_secret_path: builder_client_secret_path,
            skip_consent: trusted_oauth_clients
                .contains(bootstrap_builder::BUILDER_CLIENT_ID),
        };
        bootstrap_builder::bootstrap_builder_oauth_client(&auth_pg, &cfg)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "control: builder OAuth client bootstrap failed");
                std::io::Error::other(err.to_string())
            })?;
    }

    let state = Arc::new(AppState {
        registry,
        env_store,
        stripe_store,
        vfs,
        blob_store,
        control_key: zeroship_control::SecretString::new(control_key),
        master_key: zeroship_control::SecretString::new(master_key),
        stripe_webhook_secret: zeroship_control::SecretString::new(stripe_webhook_secret),
        worker_urls: workers_str
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
        worker_key: zeroship_control::SecretString::new(worker_key),
        admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(30, 60))),
        webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(50, 600))),
        insecure_dev,
        trust_proxy,
        deploy_tmp_dir,
        oidc_rp,
        auth_pg,
        auth_db_url: auth_db_url_resolved,
        hydra_admin_url,
        trusted_oauth_clients,
        expected_oauth_audience,
        static_policies: zeroship_authz::load_platform_policies()
            .expect("control: bundled authz policies parse"),
        pat_issuer,
        hydra_introspector,
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
    });

    let bind_addr = format!("0.0.0.0:{port}");
    tracing::info!(bind = %bind_addr, "zeroship-control listening");

    web::server(async move || {
        web::App::new()
            .state(state.clone())
            .configure(admin_handlers::configure)
            .configure(oauth_handlers::configure)
            // --- Admin API ---
            .service(
                web::resource("/api/apps")
                    .route(web::post().to(api::create_app))
                    .route(web::get().to(api::list_apps)),
            )
            .service(
                web::resource("/api/apps/{id}")
                    .route(web::get().to(api::get_app))
                    .route(web::delete().to(api::delete_app)),
            )
            .service(
                // 256MB cap matches `MAX_COMPRESSED_BYTES` in deploy.rs.
                // ntex's PayloadConfig only enforces a single upper
                // bound on the request body — the decompressed cap is
                // enforced separately as we read the tar stream.
                web::resource("/api/apps/{id}/deploy")
                    .state(web::types::PayloadConfig::new(
                        zeroship_control::deploy::MAX_COMPRESSED_BYTES,
                    ))
                    .route(web::post().to(api::deploy)),
            )
            .service(
                web::resource("/api/apps/{id}/plan")
                    .route(web::put().to(api::set_plan)),
            )
            .service(
                web::resource("/api/apps/{id}/usage")
                    .route(web::get().to(api::get_usage)),
            )
            .service(
                web::resource("/api/apps/{id}/logs")
                    .route(web::get().to(api::get_app_logs)),
            )
            .service(
                web::resource("/api/apps/{id}/vars")
                    .state(web::types::PayloadConfig::new(
                        env_handlers::ENV_MUTATION_PAYLOAD_BYTES,
                    ))
                    .route(web::get().to(env_handlers::list_vars))
                    .route(web::post().to(env_handlers::set_var)),
            )
            .service(
                web::resource("/api/apps/{id}/vars/{key}")
                    .route(web::delete().to(env_handlers::delete_var)),
            )
            .service(
                web::resource("/api/apps/{id}/secrets")
                    .state(web::types::PayloadConfig::new(
                        env_handlers::ENV_MUTATION_PAYLOAD_BYTES,
                    ))
                    .route(web::get().to(env_handlers::list_secrets))
                    .route(web::post().to(env_handlers::set_secret)),
            )
            .service(
                web::resource("/api/apps/{id}/secrets/{key}")
                    .route(web::delete().to(env_handlers::delete_secret)),
            )
            .service(
                web::resource("/api/apps/{id}/env/expose")
                    .route(web::get().to(env_handlers::list_expose))
                    .route(web::put().to(env_handlers::set_expose)),
            )
            .service(
                web::resource("/api/apps/{id}/audit")
                    .route(web::get().to(env_handlers::list_audit)),
            )
            // --- Auth (creator console) ---
            // OIDC RP callback for the `console.zeroship.ai` client.
            // The legacy `/auth/{login,register,logout,userinfo,consent,
            // authorize,google/*}` handlers were retired in P3-U8 along
            // with `auth_service` / `auth_handlers`; this is now the
            // only console-auth surface.
            .service(web::resource("/auth/callback").route(web::get().to(api::auth_callback)))
            .configure(token_handlers::configure)
            .configure(oauth_grants_handlers::configure)
            // OIDC Back-Channel Logout 1.0 RP endpoint. Hydra POSTs
            // here on user sign-out; we verify the logout_token and
            // revoke the user's console sessions. The URI must match
            // `backchannel_logout_uri` on the `console.zeroship.ai`
            // client in `ops/auth-clients.example.toml`. Mounted via
            // `.configure(...)` to mirror the gateway pattern.
            .configure(backchannel_logout::configure)
            // --- Stripe Connect ---
            .service(
                web::resource("/api/creators/{id}/stripe/onboard")
                    .route(web::post().to(stripe_handlers::onboard)),
            )
            .service(
                web::resource("/api/creators/{id}/stripe/callback")
                    .route(web::post().to(stripe_handlers::callback)),
            )
            .service(
                web::resource("/api/creators/{id}/stripe")
                    .route(web::delete().to(stripe_handlers::unlink)),
            )
            .service(
                web::resource("/api/creators/{id}/earnings")
                    .route(web::get().to(stripe_handlers::earnings)),
            )
            // --- Internal API ---
            .service(
                web::resource("/internal/versions")
                    .route(web::get().to(internal::get_versions)),
            )
            .service(
                web::resource("/internal/apps/{app_id}")
                    .route(web::get().to(internal::get_app_version)),
            )
            .service(
                web::resource("/internal/apps/{app_id}/env")
                    .route(web::get().to(internal::get_app_env)),
            )
            .service(
                web::resource("/internal/routes")
                    .route(web::get().to(internal::get_routes)),
            )
            .service(
                web::resource("/internal/usage")
                    .route(web::post().to(internal::report_usage)),
            )
            .service(
                web::resource("/internal/webhooks/stripe")
                    .route(web::post().to(stripe_handlers::webhook)),
            )
            // --- Health ---
            .service(
                web::resource("/health")
                    .route(web::get().to(internal::health)),
            )
    })
    .bind(&bind_addr)?
    .run()
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_blob_store_flag_uses_unified_name() {
        let cli =
            ControlCli::try_parse_from(["zeroship-control", "--blob-store", "/tmp/blob-root"])
                .expect("blob-store flag should parse");

        assert_eq!(cli.blob_store, "/tmp/blob-root");
    }

    #[test]
    fn control_rejects_removed_bundles_flag() {
        let err = ControlCli::try_parse_from([
            "zeroship-control",
            "--bundles",
            "/tmp/bundle-root",
        ])
        .unwrap_err();

        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn master_key_rejects_dictionary_string_in_non_dev() {
        let err = validate_master_key_material(
            "MASTER_KEY",
            "correct horse battery staple",
            false,
        )
        .unwrap_err();
        assert!(err.contains("hex or base64url"), "{err}");
    }

    #[test]
    fn master_key_rejects_short_decoded_material_in_non_dev() {
        let err = validate_master_key_material("MASTER_KEY", "YWJj", false).unwrap_err();
        assert!(err.contains("3 bytes"), "{err}");
    }

    #[test]
    fn master_key_accepts_32_byte_hex_in_non_dev() {
        let key = "00".repeat(32);
        assert!(validate_master_key_material("MASTER_KEY", &key, false).is_ok());
    }

    #[test]
    fn master_key_accepts_32_byte_base64url_in_non_dev() {
        let key = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([7u8; 32]);
        assert!(validate_master_key_material("MASTER_KEY", &key, false).is_ok());
    }

    #[test]
    fn master_key_allows_dev_shortcut_in_insecure_dev() {
        assert!(validate_master_key_material("MASTER_KEY", "password", true).is_ok());
    }
}
