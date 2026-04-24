//! zeroship-control — control plane binary. Thin wrapper over
//! `zeroship_control` (the library crate).

use std::sync::Arc;

use ntex::web;
use zeroship_core::vfs::{BundleStore, LocalFs};
use zeroship_control::{api, env_handlers, internal, stripe_handlers, AppState, EnvStore, Registry, StripeStore};

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
    let args: Vec<String> = std::env::args().collect();

    let port = arg_or_env(&args, "--port", "CONTROL_PORT", "9090");
    let db_url = arg_or_env(&args, "--db", "DATABASE_URL", "postgres://localhost/zeroship");
    let bundles_dir = arg_or_env(&args, "--bundles", "BUNDLES_DIR", "./bundles");
    let control_key = arg_or_env(&args, "--control-key", "CONTROL_KEY", "");
    let master_key = arg_or_env(&args, "--master-key", "MASTER_KEY", "");
    let stripe_webhook_secret = arg_or_env(&args, "--stripe-webhook-secret", "STRIPE_WEBHOOK_SECRET", "");
    // Opt-in: explicit "I know this is insecure" flag. Must be set to
    // run without control_key / master_key / stripe_webhook_secret.
    // Production refuses to boot without either the real secrets or
    // this sentinel.
    let insecure_dev =
        args.iter().any(|a| a == "--dev-insecure")
            || std::env::var("ZEROSHIP_DEV_INSECURE").map(|v| v == "1").unwrap_or(false);

    if !insecure_dev {
        let mut missing = Vec::new();
        if master_key.is_empty() { missing.push("--master-key / MASTER_KEY"); }
        if control_key.is_empty() { missing.push("--control-key / CONTROL_KEY"); }
        // stripe_webhook_secret is optional in principle (control-plane
        // may run without Stripe), but if it's empty the webhook
        // handler rejects every request — so the operator is on notice
        // via the startup log.
        if !missing.is_empty() {
            eprintln!(
                "[control] refusing to start: required secrets missing: {}\n\
                 Pass --dev-insecure (or ZEROSHIP_DEV_INSECURE=1) to run\n\
                 without them — NEVER in production.",
                missing.join(", "),
            );
            std::process::exit(1);
        }
    }
    if insecure_dev {
        eprintln!("[control] WARNING: --dev-insecure set; admin + internal auth disabled.");
    }

    let registry = Registry::new(&db_url)
        .await
        .expect("failed to connect to database");

    let vfs = Arc::new(
        LocalFs::new(&bundles_dir).expect("failed to initialise bundle store"),
    ) as Arc<dyn BundleStore + Send + Sync>;

    let env_store = EnvStore::new(registry.clone(), &master_key, insecure_dev)
        .expect("env store init");
    let stripe_store = StripeStore::new(registry.clone());

    let state = Arc::new(AppState {
        registry,
        env_store,
        stripe_store,
        vfs,
        control_key,
        master_key,
        stripe_webhook_secret,
        insecure_dev,
    });

    let bind_addr = format!("0.0.0.0:{port}");
    eprintln!("zeroship-control listening on {bind_addr}");

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
                web::resource("/api/apps/{id}/deploy")
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
                web::resource("/api/apps/{id}/assets/{path:.*}")
                    .route(web::put().to(api::upload_asset)),
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
                web::resource("/internal/bundles/{app_id}")
                    .route(web::get().to(internal::get_bundle)),
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
                web::resource("/internal/assets/{app_id}/{path:.*}")
                    .route(web::get().to(internal::get_asset)),
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
