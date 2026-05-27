//! ntex routes wiring. Bootstrap of routes/handlers happens here; the
//! handlers themselves live in `src/ui/`, `src/identity/`, etc. (added
//! in later tasks/phases).

use ntex::web;

use crate::config::AuthConfig;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(healthz).service(readyz);
}

#[web::get("/healthz")]
async fn healthz() -> web::HttpResponse {
    web::HttpResponse::Ok().json(&serde_json::json!({ "ok": true }))
}

#[web::get("/readyz")]
async fn readyz() -> web::HttpResponse {
    // Phase 1 readiness is process-up. Phase 1 Task 17 wires PG + hydra reachability.
    web::HttpResponse::Ok().json(&serde_json::json!({ "ready": true }))
}

/// Bind and run the ntex HTTP server.
///
/// # Errors
///
/// Returns the underlying `std::io::Error` if binding fails or the
/// server loop exits with an error.
pub async fn run(cfg: AuthConfig) -> std::io::Result<()> {
    let addr = cfg.addr.clone();
    web::server(async move || web::App::new().configure(configure))
        .bind(&addr)?
        .run()
        .await
}
