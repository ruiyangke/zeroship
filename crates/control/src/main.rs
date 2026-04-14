//! zeroship-control — control plane binary.

mod api;
mod auth_handlers;
mod auth_service;
mod internal;
mod metering;
mod registry;

use std::sync::Arc;

use zeroship_core::vfs::{BundleStore, LocalFs};
use ntex::web;

use auth_service::AuthService;
use registry::Registry;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Shared application state injected into every handler.
#[allow(missing_debug_implementations)]
pub struct AppState {
    pub registry: Registry,
    pub auth: AuthService,
    pub vfs: Arc<dyn BundleStore + Send + Sync>,
    pub control_key: String,
    pub master_key: String,
}

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

    // JWT signing secret — defaults to a random value for development.
    let jwt_secret = arg_or_env(
        &args,
        "--auth-secret",
        "AUTH_SECRET",
        &uuid::Uuid::new_v4().to_string(),
    );

    let registry = Registry::new(&db_url)
        .await
        .expect("failed to connect to database");

    let auth = AuthService::new(&db_url, &jwt_secret)
        .await
        .expect("failed to initialise auth service");

    let vfs = Arc::new(
        LocalFs::new(&bundles_dir).expect("failed to initialise bundle store"),
    ) as Arc<dyn BundleStore + Send + Sync>;

    let state = Arc::new(AppState {
        registry,
        auth,
        vfs,
        control_key,
        master_key,
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
            // --- Auth ---
            .service(
                web::resource("/auth/register")
                    .route(web::post().to(auth_handlers::register)),
            )
            .service(
                web::resource("/auth/login")
                    .route(web::post().to(auth_handlers::login)),
            )
            .service(
                web::resource("/auth/userinfo")
                    .route(web::get().to(auth_handlers::userinfo)),
            )
            .service(
                web::resource("/auth/consent")
                    .route(web::post().to(auth_handlers::consent)),
            )
            .service(
                web::resource("/auth/logout")
                    .route(web::post().to(auth_handlers::logout)),
            )
            .service(
                web::resource("/auth/authorize")
                    .route(web::get().to(auth_handlers::authorize)),
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
