//! `sandbox-agent` — PID 1 in zeroship sandbox microVMs.
//!
//! Boots ntex on the compio reactor, installs the SIGCHLD reaper
//! (pre-bind so blocking SIGCHLD process-wide is the first thing we
//! do), and serves the agent HTTP API on `:7777` until ntex's own
//! signal handling drains us on SIGTERM / SIGINT.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use ntex::web::{self, HttpResponse};

use zeroship_sandbox_agent::{
    handlers, reap, state_from_env, DEFAULT_PORT, DEFAULT_WORKSPACE,
};

#[ntex::main]
async fn main() -> ExitCode {
    if let Err(e) = run().await {
        eprintln!("[agent] fatal: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

async fn run() -> Result<(), String> {
    let workspace = PathBuf::from(
        std::env::var("SANDBOX_AGENT_WORKSPACE")
            .unwrap_or_else(|_| DEFAULT_WORKSPACE.to_string()),
    );
    let port: u16 = match std::env::var("SANDBOX_AGENT_PORT") {
        Ok(s) => s.parse().map_err(|e| format!("SANDBOX_AGENT_PORT: {e}"))?,
        Err(_) => DEFAULT_PORT,
    };

    std::fs::create_dir_all(&workspace)
        .map_err(|e| format!("create workspace {}: {e}", workspace.display()))?;

    // Reaper FIRST. It blocks SIGCHLD process-wide, so it must run
    // before ntex spawns workers (the block is inherited by every
    // thread from this point on).
    reap::install();

    let state = state_from_env(workspace.clone())?;
    let bind = format!("0.0.0.0:{port}");
    eprintln!("[agent] listening on http://{bind}, workspace={}", workspace.display());

    let state = Arc::new(state);
    web::server(async move || {
        web::App::new()
            .state((*state).clone())
            .service(web::resource("/healthz").route(web::get().to(handlers::healthz)))
            .service(web::resource("/exec").route(web::post().to(handlers::exec_cmd)))
            .service(web::resource("/tree").route(web::get().to(handlers::file_tree)))
            // {path}* is ntex's tail-match — captures across slashes.
            .service(
                web::resource("/files/{path}*")
                    .route(web::get().to(handlers::read_file))
                    .route(web::put().to(handlers::write_file))
                    .route(web::delete().to(handlers::delete_file)),
            )
            // Default 404 with JSON body (ntex returns text/plain otherwise).
            .default_service(web::route().to(|| async {
                HttpResponse::NotFound().json(&serde_json::json!({"error": "not found"}))
            }))
    })
    .bind(&bind)
    .map_err(|e| format!("bind {bind}: {e}"))?
    .run()
    .await
    .map_err(|e| format!("serve: {e}"))
}
