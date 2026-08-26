//! ntex + compio benchmark server.
//!
//! Mirrors the raw httparse server's endpoints for A/B comparison:
//! - GET /health → {"status":"ok"}
//! - POST /rpc → JSON-RPC dispatch (ping only for benchmark)
//! - GET /* → HTTP handler (echo method + url as JSON)

use ntex::web::{self, App, HttpRequest, HttpResponse};
use serde::Deserialize;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Deserialize)]
struct RpcRequest {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    method: String,
    #[allow(dead_code)]
    params: Option<serde_json::Value>,
    id: Option<serde_json::Value>,
}

async fn health() -> HttpResponse {
    HttpResponse::Ok()
        .content_type("application/json")
        .body(r#"{"status":"ok"}"#)
}

async fn rpc(body: web::types::Json<RpcRequest>) -> HttpResponse {
    let result = match body.method.as_str() {
        "ping" => serde_json::json!("pong"),
        _ => serde_json::json!(null),
    };

    let response = serde_json::json!({
        "jsonrpc": "2.0",
        "result": result,
        "id": body.id.clone().unwrap_or(serde_json::json!(null)),
    });

    HttpResponse::Ok()
        .content_type("application/json")
        .body(serde_json::to_string(&response).unwrap())
}

async fn http_handler(req: HttpRequest) -> HttpResponse {
    let body = serde_json::json!({
        "method": req.method().to_string(),
        "url": req.uri().to_string(),
    });

    HttpResponse::Ok()
        .content_type("application/json")
        .body(serde_json::to_string(&body).unwrap())
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    let port: u16 = std::env::args()
        .find(|a| a.starts_with("--port="))
        .and_then(|a| a.strip_prefix("--port=").unwrap().parse().ok())
        .unwrap_or(5200);

    let workers: usize = std::env::args()
        .find(|a| a.starts_with("--workers="))
        .and_then(|a| a.strip_prefix("--workers=").unwrap().parse().ok())
        .unwrap_or(1);

    eprintln!("[ntex-compio] starting on port {port} with {workers} workers");

    web::server(async || {
        App::new()
            .service(
                web::resource("/health").route(web::get().to(health))
            )
            .service(
                web::resource("/rpc").route(web::post().to(rpc))
            )
            .default_service(web::route().to(http_handler))
    })
    .workers(workers)
    .bind(format!("0.0.0.0:{port}"))?
    .run()
    .await
}
