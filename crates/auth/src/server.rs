//! ntex routes wiring. Bootstrap of routes/handlers happens here; the
//! handlers themselves live in `src/ui/`, `src/identity/`, etc.

use std::sync::Arc;

use ntex::web;

use crate::config::AuthConfig;
use crate::hydra_client::HydraAdmin;
use crate::ui;

/// Bundled stylesheet served at `/static/style.css`. Compiled into the
/// binary at build time so the runtime has no filesystem dependency.
const STATIC_CSS: &str = include_str!("../static/style.css");

/// Register every route the auth server exposes.
///
/// State (`Arc<HydraAdmin>`-equivalent, `Arc<AuthConfig>`, `Arc<Client>`)
/// is registered on the `App` in [`run`]; this function only wires URL
/// paths to handlers.
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(healthz)
        .service(readyz)
        .service(style)
        .service(
            web::resource("/login")
                .route(web::get().to(ui::login::get))
                .route(web::post().to(ui::login::post)),
        )
        .service(
            web::resource("/signup")
                .route(web::get().to(ui::signup::get))
                .route(web::post().to(ui::signup::post)),
        )
        .service(
            web::resource("/consent").route(web::get().to(ui::consent::get)),
        );
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

#[web::get("/static/style.css")]
async fn style() -> web::HttpResponse {
    let mut r = web::HttpResponse::Ok();
    r.content_type("text/css; charset=utf-8");
    r.body(STATIC_CSS)
}

/// Bind and run the ntex HTTP server.
///
/// Threads three shared-state slots through ntex's `App::state`:
///
/// - `HydraAdmin` — hydra admin API client (cheap to clone; holds an
///   internal `cyper::Client`).
/// - `Arc<AuthConfig>` — the parsed config; used by handlers for the
///   `insecure_dev` cookie flag and hydra URLs.
/// - `Arc<compio_postgres::Client>` — the PG client; `Client` is not
///   itself `Clone`, so it must be wrapped before being shared across
///   worker tasks.
///
/// # Errors
///
/// Returns the underlying `std::io::Error` if binding fails or the
/// server loop exits with an error.
pub async fn run(
    cfg: AuthConfig,
    admin: HydraAdmin,
    db: compio_postgres::Client,
) -> std::io::Result<()> {
    let addr = cfg.addr.clone();
    let cfg = Arc::new(cfg);
    let db = Arc::new(db);

    web::server(async move || {
        web::App::new()
            .state(admin.clone())
            .state(cfg.clone())
            .state(db.clone())
            .configure(configure)
    })
    .bind(&addr)?
    .run()
    .await
}
