//! `sandbox-agent` — PID 1 in zeroship sandbox microVMs.
//!
//! Boots ntex on the compio reactor, installs the SIGCHLD reaper
//! (pre-bind so blocking SIGCHLD process-wide is the first thing we
//! do), and serves the agent HTTP API on `:7777` until ntex's own
//! signal handling drains us on SIGTERM / SIGINT.

use std::path::PathBuf;
use std::process::ExitCode;

use ntex::web::middleware::DefaultHeaders;
use ntex::web::{self, HttpResponse};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use zeroship_sandbox_agent::{
    dropuser, handlers, reap, state_from_env, version, DEFAULT_PORT, DEFAULT_WORKSPACE,
};

#[ntex::main]
async fn main() -> ExitCode {
    // SIGCHLD MUST be blocked before any code that might fork(). The
    // reaper installs the process-wide block + a dedicated waitpid
    // thread, so put it ahead of everything (env reads, logging
    // setup, ntex startup). Idempotent + safe even when not PID 1.
    reap::install();
    init_tracing();
    if let Err(e) = run().await {
        error!(error = %e, "agent fatal");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

/// Init the JSON-formatted tracing subscriber. Filter by
/// `SANDBOX_AGENT_LOG` (defaults to `info`); accepts the standard
/// `RUST_LOG`-style directives — e.g.
/// `SANDBOX_AGENT_LOG=info,zeroship_sandbox_agent::audit=warn`.
fn init_tracing() {
    let filter = EnvFilter::try_from_env("SANDBOX_AGENT_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .json()
        .with_target(true)
        .with_current_span(false)
        .with_span_list(false)
        .init();
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

    // Chown the workspace to the drop user so /exec children
    // (which run as nobody:nogroup when we're PID 1 root) can
    // actually write into it. No-op when not running as root.
    dropuser::chown_workspace(&workspace);

    // (Reaper installed at the very top of `main` so SIGCHLD is
    // already blocked process-wide by the time we reach this point.)
    let state = state_from_env(workspace.clone())?;
    let bind = format!("0.0.0.0:{port}");
    info!(bind = %bind, workspace = %workspace.display(), "agent listening");

    // Hard cap on PUT /files request bodies, enforced at the ntex
    // payload extractor (rejects oversize *before* the handler runs,
    // so we never buffer 10 GiB into memory just to reject in
    // files::write_file). 1 MiB headroom over the file cap covers
    // serialization overhead.
    let body_limit = zeroship_sandbox_agent::files::MAX_BYTES + 1024 * 1024;

    let proto = version::PROTOCOL_VERSION.to_string();
    // AppState is already cheap-clone (`Arc<Token>`, `Arc<Workspace>`,
    // `Arc<AtomicBool>`), so we capture by move and clone per-worker
    // inside the factory. No outer Arc<AppState> needed.
    web::server(async move || {
        let state = state.clone();
        let proto = proto.clone();
        web::App::new()
            .state(state)
            // Bytes extractor used by PUT /files
            .state(web::types::PayloadConfig::default().limit(body_limit))
            // X-Sbx-Protocol on every response — controller checks
            // this to verify it's talking to a supported agent version.
            .middleware(DefaultHeaders::new().header("X-Sbx-Protocol", &proto))
            // Liveness / readiness / version (unauthenticated)
            .service(web::resource("/livez").route(web::get().to(handlers::livez)))
            .service(web::resource("/readyz").route(web::get().to(handlers::readyz)))
            .service(web::resource("/version").route(web::get().to(handlers::version_info)))
            // Prometheus scrape — unauthenticated; cluster NetworkPolicy
            // governs who can reach :7777 to scrape.
            .service(web::resource("/metrics").route(web::get().to(handlers::metrics)))
            // /healthz preserved as an alias for /livez (back-compat).
            .service(web::resource("/healthz").route(web::get().to(handlers::livez)))
            // Auth-gated endpoints
            .service(web::resource("/exec").route(web::post().to(handlers::exec_cmd)))
            .service(web::resource("/tree").route(web::get().to(handlers::file_tree)))
            .service(web::resource("/shutdown").route(web::post().to(handlers::shutdown)))
            .service(
                web::resource("/files/{path}*")
                    .route(web::get().to(handlers::read_file))
                    .route(web::put().to(handlers::write_file))
                    .route(web::delete().to(handlers::delete_file)),
            )
            .default_service(web::route().to(|| async {
                HttpResponse::NotFound().json(&serde_json::json!({"error": "not found"}))
            }))
    })
    .bind(&bind)
    .map_err(|e| format!("bind {bind}: {e}"))?
    // Hard cap on graceful drain: a long /exec can't hold the
    // shutdown forever. After this many seconds we abort
    // in-flight handlers and exit.
    .shutdown_timeout(ntex::time::Seconds(30))
    .run()
    .await
    .map_err(|e| format!("serve: {e}"))
}
