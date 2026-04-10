//! appbase-control — control plane binary.

mod api;
mod internal;
mod metering;
mod registry;

use std::sync::Arc;

use appbase_common::vfs::{BundleStore, LocalFs};
use ntex::web;

use registry::Registry;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Shared application state injected into every handler.
#[allow(missing_debug_implementations)]
pub struct AppState {
    pub registry: Registry,
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
    let db_url = arg_or_env(&args, "--db", "DATABASE_URL", "postgres://localhost/appbase");
    let bundles_dir = arg_or_env(&args, "--bundles", "BUNDLES_DIR", "./bundles");
    let control_key = arg_or_env(&args, "--control-key", "CONTROL_KEY", "");
    let master_key = arg_or_env(&args, "--master-key", "MASTER_KEY", "");

    let registry = Registry::new(&db_url)
        .await
        .expect("failed to connect to database");

    let vfs = Arc::new(
        LocalFs::new(&bundles_dir).expect("failed to initialise bundle store"),
    ) as Arc<dyn BundleStore + Send + Sync>;

    let state = Arc::new(AppState {
        registry,
        vfs,
        control_key,
        master_key,
    });

    let bind_addr = format!("0.0.0.0:{port}");
    eprintln!("appbase-control listening on {bind_addr}");

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
                web::resource("/internal/usage")
                    .route(web::post().to(internal::report_usage)),
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
