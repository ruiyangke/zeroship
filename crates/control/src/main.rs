//! zeroship-control — control plane binary. Thin wrapper over
//! `zeroship_control` (the library crate).

use std::path::PathBuf;
use std::sync::Arc;

use ntex::web;
use zeroship_bundle::{BlobStore, BundleStore, LocalDiskBlobStore, LocalFs};
use zeroship_control::{
    api, auth_handlers, auth_service, env_handlers, internal, oauth, oidc_rp, stripe_handlers,
    AppState, EnvStore, Quota, RateLimiter, Registry, StripeStore,
};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Parse a simple `--flag value` pair from the argument list.
fn arg_or_env(args: &[String], flag: &str, env_key: &str, default: &str) -> String {
    for pair in args.windows(2) {
        if pair[0] == flag {
            return pair[1].clone();
        }
    }
    env_or(env_key, default)
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    zeroship_core::observability::init_tracing("info,zeroship_control=debug");

    let args: Vec<String> = std::env::args().collect();

    let port = arg_or_env(&args, "--port", "CONTROL_PORT", "9090");
    let db_url = arg_or_env(&args, "--db", "DATABASE_URL", "postgres://localhost/zeroship");
    let bundles_dir = arg_or_env(&args, "--bundles", "BUNDLES_DIR", "./bundles");
    let control_key = arg_or_env(&args, "--control-key", "CONTROL_KEY", "");
    let master_key = arg_or_env(&args, "--master-key", "MASTER_KEY", "");
    let workers_str = arg_or_env(&args, "--workers", "WORKER_URLS", "http://localhost:8080");
    let worker_key = arg_or_env(&args, "--worker-key", "WORKER_KEY", "");
    let stripe_webhook_secret = arg_or_env(&args, "--stripe-webhook-secret", "STRIPE_WEBHOOK_SECRET", "");
    // Comma-separated list of previous master keys, tried as fallbacks
    // on decrypt failure during a rotation grace period.
    let legacy_master_keys_raw = arg_or_env(&args, "--legacy-master-keys", "LEGACY_MASTER_KEYS", "");
    // Opt-in: explicit "I know this is insecure" flag. Must be set to
    // run without control_key / master_key / stripe_webhook_secret.
    // Production refuses to boot without either the real secrets or
    // this sentinel.
    let insecure_dev =
        args.iter().any(|a| a == "--dev-insecure")
            || std::env::var("ZEROSHIP_DEV_INSECURE").map(|v| v == "1").unwrap_or(false);
    // Default: do NOT trust X-Forwarded-For. Operators behind a real
    // load balancer opt in explicitly via --trust-proxy; everyone else
    // gets the safe behavior (peer_addr only, no spoof surface).
    let trust_proxy =
        args.iter().any(|a| a == "--trust-proxy")
            || std::env::var("TRUST_PROXY").map(|v| v == "1").unwrap_or(false);

    let deploy_tmp_dir_str = arg_or_env(
        &args,
        "--deploy-tmp-dir",
        "DEPLOY_TMP_DIR",
        "", // empty -> fall back to std::env::temp_dir() below
    );
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
        if master_key.is_empty() { missing.push("--master-key / MASTER_KEY"); }
        if control_key.is_empty() { missing.push("--control-key / CONTROL_KEY"); }
        if !missing.is_empty() {
            tracing::error!(
                missing = %missing.join(", "),
                "control: refusing to start; required secrets missing. \
                 Pass --dev-insecure (or ZEROSHIP_DEV_INSECURE=1) to run \
                 without them — NEVER in production."
            );
            std::process::exit(1);
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

    let registry = Registry::new(&db_url)
        .await
        .expect("failed to connect to database");

    let vfs = Arc::new(
        LocalFs::new(&bundles_dir).expect("failed to initialise bundle store"),
    ) as Arc<dyn BundleStore + Send + Sync>;

    // BlobStore lives alongside the legacy BundleStore on the same
    // root. New `.zship` deploys land in `<bundles_dir>/blobs/` and
    // `<bundles_dir>/manifests/`; legacy `<bundles_dir>/<app_id>/...`
    // files stay where they are until the old BundleStore path is
    // retired.
    let blob_root = PathBuf::from(&bundles_dir);
    let blob_store: Arc<dyn BlobStore> = Arc::new(
        LocalDiskBlobStore::new(blob_root)
            .expect("failed to initialise blob store"),
    );

    let legacy_keys: Vec<&str> = legacy_master_keys_raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
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

    // JWT secret — used to sign session cookies. Must be stable across
    // restarts in prod (else everyone gets logged out). In dev a random
    // boot-time secret is fine.
    let jwt_secret = arg_or_env(&args, "--jwt-secret", "JWT_SECRET", "");
    let jwt_secret = if jwt_secret.is_empty() {
        if !insecure_dev {
            tracing::error!("control: refusing to start; --jwt-secret / JWT_SECRET required (or pass --dev-insecure)");
            std::process::exit(1);
        }
        // Stable fallback so cookies survive a quick restart in dev.
        "dev-jwt-secret-please-override-in-prod".to_string()
    } else {
        jwt_secret
    };

    let auth = auth_service::AuthService::new(&db_url, &jwt_secret)
        .await
        .expect("failed to init auth service");

    // Optional Google OAuth config — only set if all three env vars present.
    let google_client_id = env_or("GOOGLE_CLIENT_ID", "");
    let google_client_secret = env_or("GOOGLE_CLIENT_SECRET", "");
    let google_redirect = env_or("GOOGLE_REDIRECT_URI", "http://localhost:5173/auth/google/callback");
    let google_oauth = if !google_client_id.is_empty() && !google_client_secret.is_empty() {
        tracing::info!(redirect_uri = %google_redirect, "control: Google OAuth enabled");
        Some(oauth::GoogleConfig {
            client_id: google_client_id,
            client_secret: google_client_secret,
            redirect_uri: google_redirect,
        })
    } else {
        tracing::info!("control: Google OAuth disabled (set GOOGLE_CLIENT_ID + GOOGLE_CLIENT_SECRET)");
        None
    };

    // Phase 3 U7 — control plane OIDC RP for `console.zeroship.ai`.
    // Optional: when `--auth-public` or `--console-oidc-secret` is
    // empty, the new RP path is disabled and the legacy
    // `auth_handlers` chain remains the only auth surface. U8 retires
    // the legacy path and makes these mandatory.
    let auth_public = arg_or_env(&args, "--auth-public", "AUTH_PUBLIC", "");
    let console_oidc_secret =
        arg_or_env(&args, "--console-oidc-secret", "CONSOLE_OIDC_SECRET", "");
    let stash_signing_key = arg_or_env(
        &args,
        "--stash-signing-key",
        "STASH_SIGNING_KEY",
        "",
    );
    let auth_db_url = arg_or_env(&args, "--auth-db", "AUTH_DB_URL", "");

    let oidc_rp = if !auth_public.is_empty() && !console_oidc_secret.is_empty() {
        if stash_signing_key.is_empty() && !insecure_dev {
            tracing::error!(
                "control: refusing to enable console OIDC RP without --stash-signing-key (set --dev-insecure to override)"
            );
            std::process::exit(1);
        }
        let key = if stash_signing_key.is_empty() {
            // Dev-only fallback. Ephemeral keys are fine for the
            // 10-minute stash window during local development; in
            // prod the guard above already exited.
            b"dev-stash-key-please-rotate".to_vec()
        } else {
            stash_signing_key.into_bytes()
        };
        tracing::info!(
            auth_public = %auth_public,
            "control: console OIDC RP enabled"
        );
        Some(Arc::new(oidc_rp::ConsoleOidcRp::new(
            &auth_public,
            "console.zeroship.ai",
            console_oidc_secret,
            key,
        )))
    } else {
        tracing::info!(
            "control: console OIDC RP disabled (set --auth-public + --console-oidc-secret to enable)"
        );
        None
    };

    let auth_pg: Option<Arc<compio_postgres::Client>> = if auth_db_url.is_empty() {
        if oidc_rp.is_some() {
            tracing::warn!(
                "control: console OIDC RP is configured but --auth-db is empty; /auth/callback will 500"
            );
        }
        None
    } else {
        let (pg_client, pg_conn) = compio_postgres::connect(&auth_db_url, compio_postgres::NoTls)
            .await
            .expect("control: auth-pg connect");
        compio::runtime::spawn(async move {
            if let Err(e) = pg_conn.run().await {
                tracing::error!(error = %e, "control/auth-pg connection ended");
            }
        })
        .detach();
        Some(Arc::new(pg_client))
    };

    let state = Arc::new(AppState {
        registry,
        env_store,
        stripe_store,
        auth,
        google_oauth,
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
    });

    let bind_addr = format!("0.0.0.0:{port}");
    tracing::info!(bind = %bind_addr, "zeroship-control listening");

    web::server(async move || {
        web::App::new()
            .state(state.clone())
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
                    .route(web::get().to(env_handlers::list_vars))
                    .route(web::post().to(env_handlers::set_var)),
            )
            .service(
                web::resource("/api/apps/{id}/vars/{key}")
                    .route(web::delete().to(env_handlers::delete_var)),
            )
            .service(
                web::resource("/api/apps/{id}/secrets")
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
            // --- Auth (creator + end-user) ---
            .service(web::resource("/auth/register").route(web::post().to(auth_handlers::register)))
            .service(web::resource("/auth/login").route(web::post().to(auth_handlers::login)))
            .service(web::resource("/auth/logout").route(web::post().to(auth_handlers::logout)))
            .service(web::resource("/auth/userinfo").route(web::get().to(auth_handlers::userinfo)))
            .service(web::resource("/auth/consent").route(web::post().to(auth_handlers::consent)))
            .service(web::resource("/auth/authorize").route(web::get().to(auth_handlers::authorize)))
            .service(web::resource("/auth/google/start").route(web::get().to(auth_handlers::google_start)))
            .service(web::resource("/auth/google/callback").route(web::get().to(auth_handlers::google_callback)))
            // New OIDC RP callback for the `console.zeroship.ai` client
            // (P3-U7). Sibling of the legacy /auth/* handlers; U8
            // retires those and this becomes the canonical entry.
            .service(web::resource("/auth/callback").route(web::get().to(api::auth_callback)))
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
