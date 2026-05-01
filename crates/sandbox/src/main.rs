//! `zeroship-sandbox` — pluggable backends (docker, k8s) for AI builder
//! sandboxes.
//!
//! Each editor sandbox gets its own runtime — a Docker container or a
//! libkrun-microVM Pod, depending on `SANDBOX_BACKEND`. Files and
//! commands are driven through the [`zeroship_sandbox::backend::Backend`]
//! abstraction.
//!
//! See `docs/superpowers/specs/2026-04-27-zeroship-editor-design.md`
//! for the full architecture.

use ntex::web;
use zeroship_sandbox::{handlers, registry, AppState};
use zeroship_sandbox::config::SandboxConfig;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[ntex::main]
async fn main() -> std::io::Result<()> {
    let config = match SandboxConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[sandbox] config error: {e}");
            std::process::exit(1);
        }
    };

    eprintln!("[sandbox] backend:        {}", config.backend);
    if config.backend == "docker" {
        eprintln!("[sandbox] image:          {}", config.image);
        eprintln!("[sandbox] workspace root: {}", config.workspace_root.display());
    } else {
        eprintln!("[sandbox] k8s namespace:  {}", config.k8s.namespace);
        eprintln!("[sandbox] agent image:    {}", config.k8s.image);
        eprintln!("[sandbox] runtime class:  {}", config.k8s.runtime_class);
        eprintln!("[sandbox] port-forward:   {}", config.k8s.use_port_forward);
    }
    eprintln!("[sandbox] idle timeout:   {}s", config.idle_timeout_secs);

    if config.token.is_empty() {
        eprintln!("[sandbox] WARNING: SANDBOX_TOKEN not set — endpoints are unauthenticated");
    }

    // Build state (probes the backend at startup).
    let state = match AppState::from_config(config.clone()).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[sandbox] backend probe failed: {e}");
            std::process::exit(1);
        }
    };

    // Idle GC sweep — kills runtimes idle longer than `idle_timeout_secs`.
    registry::start_idle_gc(state.clone());

    let bind = format!("0.0.0.0:{}", config.port);
    eprintln!("[sandbox] http://{bind}");

    web::server(async move || {
        web::App::new()
            .state(state.clone())
            .service(
                web::resource("/health")
                    .route(web::get().to(|| async {
                        web::HttpResponse::Ok().body(r#"{"status":"ok"}"#)
                    })),
            )
            // Liveness — process is up. Always 200; load balancers
            // should restart on repeated 5xx, not on /readyz=503.
            .service(
                web::resource("/livez")
                    .route(web::get().to(|| async {
                        web::HttpResponse::Ok().body(r#"{"status":"ok"}"#)
                    })),
            )
            // Readiness — backend is reachable. Returns 503 when
            // the backend probe most recently failed (kubectl auth
            // expired, cluster unreachable, etc.). LBs / orchestrators
            // route around the controller until the backend recovers.
            .service(
                web::resource("/readyz").route(web::get().to(handlers::readyz)),
            )
            .service(
                web::resource("/sandboxes")
                    .route(web::post().to(handlers::create_sandbox))
                    .route(web::get().to(handlers::list_sandboxes)),
            )
            .service(
                web::resource("/sandboxes/{id}")
                    .route(web::get().to(handlers::get_sandbox))
                    .route(web::delete().to(handlers::stop_sandbox)),
            )
            .service(
                web::resource("/sandboxes/{id}/exec")
                    .route(web::post().to(handlers::exec)),
            )
            .service(
                web::resource("/sandboxes/{id}/file-tree")
                    .route(web::get().to(handlers::file_tree)),
            )
            .service(
                web::resource("/sandboxes/{id}/files/{path}*")
                    .route(web::get().to(handlers::read_file))
                    .route(web::put().to(handlers::write_file))
                    .route(web::delete().to(handlers::delete_file)),
            )
    })
    .bind(&bind)?
    .run()
    .await
}
