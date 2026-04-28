//! `zeroship-sandbox` — Docker-backed dev containers for the AI builder.
//!
//! Each editor session gets its own container with a persistent
//! workspace bind-mounted in. The agent (running inside the editor
//! app) calls this service over HTTP to read/write files and run
//! shell commands inside the container.
//!
//! See `docs/superpowers/specs/2026-04-27-zeroship-editor-design.md`
//! for the full architecture.

mod auth;
mod config;
mod docker;
mod files;
mod handlers;
mod session;

use std::sync::Arc;

use ntex::web;

use crate::config::SandboxConfig;
use crate::session::SessionRegistry;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Shared application state passed to every handler.
#[allow(missing_debug_implementations)]
pub struct AppState {
    pub config: SandboxConfig,
    pub sessions: SessionRegistry,
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    let config = match SandboxConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[sandbox] config error: {e}");
            std::process::exit(1);
        }
    };

    eprintln!("[sandbox] image: {}", config.image);
    eprintln!("[sandbox] workspace root: {}", config.workspace_root.display());
    eprintln!("[sandbox] idle timeout: {}s", config.idle_timeout_secs);

    if config.token.is_empty() {
        eprintln!("[sandbox] WARNING: SANDBOX_TOKEN not set — endpoints are unauthenticated");
    }

    // Probe Docker once at startup so we fail fast instead of on the first
    // session-create call. The user can fix permission issues before
    // anyone tries to use the service.
    if let Err(e) = docker::probe_docker().await {
        eprintln!("[sandbox] docker probe failed: {e}");
        eprintln!("[sandbox] is the docker daemon running and is the user in the docker group?");
        std::process::exit(1);
    }

    if config.auto_pull {
        eprintln!("[sandbox] pulling {}...", config.image);
        if let Err(e) = docker::pull_image(&config.image).await {
            eprintln!("[sandbox] pull failed (continuing — image may be local): {e}");
        }
    }

    // Ensure the workspace root exists.
    if let Err(e) = std::fs::create_dir_all(&config.workspace_root) {
        eprintln!("[sandbox] failed to create workspace root: {e}");
        std::process::exit(1);
    }

    let registry = SessionRegistry::new();
    let state = Arc::new(AppState {
        config: config.clone(),
        sessions: registry.clone(),
    });

    // Idle GC sweep — kills containers idle longer than `idle_timeout_secs`.
    session::start_idle_gc(state.clone());

    let bind = format!("0.0.0.0:{}", config.port);
    eprintln!("[sandbox] http://{bind}");

    web::server(async move || {
        web::App::new()
            .state(state.clone())
            .service(
                web::resource("/health")
                    .route(web::get().to(|| async { web::HttpResponse::Ok().body(r#"{"status":"ok"}"#) })),
            )
            .service(
                web::resource("/sessions")
                    .route(web::post().to(handlers::create_session))
                    .route(web::get().to(handlers::list_sessions)),
            )
            .service(
                web::resource("/sessions/{id}")
                    .route(web::get().to(handlers::get_session))
                    .route(web::delete().to(handlers::stop_session)),
            )
            .service(
                web::resource("/sessions/{id}/exec")
                    .route(web::post().to(handlers::exec)),
            )
            .service(
                web::resource("/sessions/{id}/file-tree")
                    .route(web::get().to(handlers::file_tree)),
            )
            .service(
                web::resource("/sessions/{id}/files/{path:.*}")
                    .route(web::get().to(handlers::read_file))
                    .route(web::put().to(handlers::write_file))
                    .route(web::delete().to(handlers::delete_file)),
            )
    })
    .bind(&bind)?
    .run()
    .await
}
