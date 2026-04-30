//! HTTP handlers for the agent.
//!
//! Endpoints (every one except `/healthz` requires `Authorization:
//! Bearer <token>` — checked inline at the top of each handler, same
//! pattern as `crates/sandbox/src/handlers.rs`):
//!
//!   GET  /healthz           — liveness, no auth (used for cold-start polling)
//!   POST /exec              — run shell command (JSON in/out)
//!   GET  /tree              — workspace file listing
//!   GET  /files/{path}*     — read file
//!   PUT  /files/{path}*     — write file (raw bytes)
//!   DELETE /files/{path}*   — delete file

use std::path::PathBuf;
use std::sync::Arc;

use ntex::util::Bytes;
use ntex::web::{self, HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::json;

use crate::auth::Token;
use crate::{exec, files};

/// State shared by every handler. Cheap to clone (`Arc` inside).
#[derive(Clone, Debug)]
pub struct AppState {
    pub token: Arc<Token>,
    pub workspace: PathBuf,
}

type State = web::types::State<AppState>;

// ─── helpers ─────────────────────────────────────────────────────

fn unauthorized() -> HttpResponse {
    HttpResponse::Unauthorized().json(&json!({"error": "unauthorized"}))
}

fn err(status: u16, msg: impl Into<String>) -> HttpResponse {
    let s = msg.into();
    let mut resp = match status {
        400 => HttpResponse::BadRequest(),
        403 => HttpResponse::Forbidden(),
        404 => HttpResponse::NotFound(),
        500 => HttpResponse::InternalServerError(),
        _ => HttpResponse::InternalServerError(),
    };
    resp.json(&json!({"error": s}))
}

/// Inline auth check. Reads `Authorization` header and constant-time
/// compares against the loaded token. False on any miss; the handler
/// uses that to short-circuit with 401.
fn check_token(req: &HttpRequest, state: &AppState) -> bool {
    let h = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok());
    state.token.verify_header(h)
}

fn content_type(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("");
    match ext.to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" | "ts" | "tsx" | "jsx" => "application/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "md" | "txt" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        _ => "application/octet-stream",
    }
}

// ─── /healthz ────────────────────────────────────────────────────

pub async fn healthz() -> HttpResponse {
    HttpResponse::Ok().json(&json!({"status": "ok"}))
}

// ─── /exec ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ExecBody {
    pub cmd: String,
    pub cwd: Option<String>,
    pub timeout_ms: Option<u64>,
}

pub async fn exec_cmd(
    req: HttpRequest,
    state: State,
    body: web::types::Json<ExecBody>,
) -> HttpResponse {
    if !check_token(&req, &state) { return unauthorized(); }

    let cwd = body
        .cwd
        .as_deref()
        .unwrap_or_else(|| state.workspace.to_str().unwrap_or("/workspace"));
    let timeout = body.timeout_ms.unwrap_or(exec::DEFAULT_TIMEOUT_MS);
    match exec::run(&body.cmd, cwd, timeout).await {
        Ok(out) => HttpResponse::Ok().json(&out),
        Err(e) => err(500, e),
    }
}

// ─── /tree ───────────────────────────────────────────────────────

pub async fn file_tree(req: HttpRequest, state: State) -> HttpResponse {
    if !check_token(&req, &state) { return unauthorized(); }

    match files::file_tree(&state.workspace) {
        Ok(entries) => HttpResponse::Ok().json(&json!({"entries": entries})),
        Err(e) => err(500, e),
    }
}

// ─── /files/{path}* ──────────────────────────────────────────────

pub async fn read_file(
    req: HttpRequest,
    state: State,
    path: web::types::Path<String>,
) -> HttpResponse {
    if !check_token(&req, &state) { return unauthorized(); }

    let p = path.into_inner();
    match files::read_file(&state.workspace, &p) {
        Ok(bytes) => HttpResponse::Ok()
            .content_type(content_type(&p))
            .body(bytes),
        Err(e) if e.contains("symlink") => err(403, e),
        Err(e) if e.contains("No such file") || e.contains("not found") => err(404, e),
        Err(e) => err(400, e),
    }
}

pub async fn write_file(
    req: HttpRequest,
    state: State,
    path: web::types::Path<String>,
    body: Bytes,
) -> HttpResponse {
    if !check_token(&req, &state) { return unauthorized(); }

    let p = path.into_inner();
    let n = body.len();
    match files::write_file(&state.workspace, &p, &body) {
        Ok(()) => HttpResponse::Ok().json(&json!({"written": p, "size": n})),
        Err(e) if e.contains("symlink") => err(403, e),
        Err(e) => err(400, e),
    }
}

pub async fn delete_file(
    req: HttpRequest,
    state: State,
    path: web::types::Path<String>,
) -> HttpResponse {
    if !check_token(&req, &state) { return unauthorized(); }

    let p = path.into_inner();
    match files::delete_file(&state.workspace, &p) {
        Ok(true) => HttpResponse::Ok().json(&json!({"deleted": p})),
        Ok(false) => err(404, "file not found"),
        Err(e) if e.contains("symlink") => err(403, e),
        Err(e) => err(400, e),
    }
}
