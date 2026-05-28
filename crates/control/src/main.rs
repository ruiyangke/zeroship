//! zeroship-control — control plane binary. Thin wrapper over
//! `zeroship_control` (the library crate).

use std::path::PathBuf;
use std::sync::Arc;

use ntex::web;
use zeroship_bundle::{BlobStore, BundleStore, LocalDiskBlobStore, LocalFs};
use zeroship_control::{
    admin_handlers, api, backchannel_logout, env_handlers, internal, oidc_rp, stripe_handlers,
    oauth_handlers, token_handlers, AppState, EnvStore, Quota, RateLimiter, Registry, StripeStore,
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
    let signing_key_file = arg_or_env(&args, "--signing-key-file", "SIGNING_KEY_FILE", "");
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
        if signing_key_file.is_empty() { missing.push("--signing-key-file / SIGNING_KEY_FILE"); }
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

    // Phase 3 U7/U8 — control plane OIDC RP for `console.zeroship.ai`.
    // Mandatory post-U8: the legacy `auth_handlers` / `auth_service`
    // chain has been retired, so the OIDC RP is the only console-auth
    // surface. Control also needs hydra-admin for OAuth bearer
    // introspection. Refuses to boot unless these pieces are configured
    // (`--dev-insecure` permits localhost defaults only).
    let auth_public = arg_or_env(&args, "--auth-public", "AUTH_PUBLIC", "");
    let hydra_admin_url = arg_or_env(
        &args,
        "--hydra-admin",
        "AUTH_HYDRA_ADMIN",
        "http://127.0.0.1:4445",
    );
    let console_oidc_secret =
        arg_or_env(&args, "--console-oidc-secret", "CONSOLE_OIDC_SECRET", "");
    let stash_signing_key = arg_or_env(
        &args,
        "--stash-signing-key",
        "STASH_SIGNING_KEY",
        "",
    );
    let auth_db_url = arg_or_env(&args, "--auth-db", "AUTH_DB_URL", "");
    let hydra_admin_url = {
        let value = arg_or_env(&args, "--hydra-admin-url", "HYDRA_ADMIN_URL", "");
        if value.is_empty() {
            env_or("AUTH_HYDRA_ADMIN", "")
        } else {
            value
        }
    };

    if !insecure_dev {
        let mut missing = Vec::new();
        if auth_public.is_empty() {
            missing.push("--auth-public / AUTH_PUBLIC");
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
    }

    let stash_key_bytes = if stash_signing_key.is_empty() {
        // Dev-only fallback. Ephemeral keys are fine for the 10-minute
        // stash window during local development; in prod the guard
        // above already exited.
        b"dev-stash-key-please-rotate".to_vec()
    } else {
        stash_signing_key.into_bytes()
    };
    let auth_public_value = if auth_public.is_empty() {
        // Dev-only fallback so a `--dev-insecure` boot succeeds without
        // a configured hydra. Production exited above.
        "http://localhost:4444".to_string()
    } else {
        auth_public.clone()
    };
    let hydra_admin_url_value = if hydra_admin_url.is_empty() {
        // Dev-only fallback: Hydra's default admin listener in local
        // docker-compose. Production exited above.
        "http://localhost:4445".to_string()
    } else {
        hydra_admin_url
    };
    let console_oidc_secret_value = if console_oidc_secret.is_empty() {
        "dev-console-oidc-secret".to_string()
    } else {
        console_oidc_secret
    };
    tracing::info!(
        auth_public = %auth_public_value,
        "control: console OIDC RP enabled"
    );
    let oidc_rp = Arc::new(oidc_rp::ConsoleOidcRp::new(
        &auth_public_value,
        "console.zeroship.ai",
        console_oidc_secret_value,
        stash_key_bytes,
    ));
    let hydra_introspector = Arc::new(zeroship_core::hydra::HydraIntrospector::new(
        &hydra_admin_url_value,
    ));

    let auth_pg: Arc<compio_postgres::Client> = {
        let resolved = if auth_db_url.is_empty() {
            // Dev fallback: reuse the control DB URL so /auth/callback
            // works against a single local Postgres without operator
            // ceremony. Production refused to start without --auth-db
            // above.
            db_url.clone()
        } else {
            auth_db_url
        };
        let (pg_client, pg_conn) = compio_postgres::connect(&resolved, compio_postgres::NoTls)
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
        hydra_admin_url,
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
            // --- Auth (creator console) ---
            // OIDC RP callback for the `console.zeroship.ai` client.
            // The legacy `/auth/{login,register,logout,userinfo,consent,
            // authorize,google/*}` handlers were retired in P3-U8 along
            // with `auth_service` / `auth_handlers`; this is now the
            // only console-auth surface.
            .service(web::resource("/auth/callback").route(web::get().to(api::auth_callback)))
            .configure(token_handlers::configure)
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
