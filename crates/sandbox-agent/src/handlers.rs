//! HTTP handlers for the agent.
//!
//! Endpoints (every one except `/healthz` requires `Authorization:
//! Bearer <token>`):
//!
//!   GET  /healthz           — liveness, no auth (used for cold-start polling)
//!   POST /exec              — run shell command (JSON in/out)
//!   GET  /tree              — workspace file listing
//!   GET  /files/{*path}     — read file
//!   PUT  /files/{*path}     — write file (raw bytes)
//!   DELETE /files/{*path}   — delete file

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
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

// ─── auth middleware ─────────────────────────────────────────────

pub async fn require_token(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let header_val = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if !state.token.verify_header(header_val) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response();
    }
    next.run(req).await
}

// ─── /healthz ────────────────────────────────────────────────────

pub async fn healthz() -> Response {
    (StatusCode::OK, Json(json!({"status": "ok"}))).into_response()
}

// ─── /exec ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ExecBody {
    pub cmd: String,
    pub cwd: Option<String>,
    pub timeout_ms: Option<u64>,
}

pub async fn exec_cmd(
    State(state): State<AppState>,
    Json(body): Json<ExecBody>,
) -> Response {
    let cwd = body
        .cwd
        .as_deref()
        .unwrap_or_else(|| state.workspace.to_str().unwrap_or("/workspace"));
    let timeout = body.timeout_ms.unwrap_or(exec::DEFAULT_TIMEOUT_MS);
    match exec::run(&body.cmd, cwd, timeout).await {
        Ok(out) => Json(out).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

// ─── /tree ───────────────────────────────────────────────────────

pub async fn file_tree(State(state): State<AppState>) -> Response {
    match files::file_tree(&state.workspace) {
        Ok(entries) => Json(json!({"entries": entries})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

// ─── /files/* ────────────────────────────────────────────────────

pub async fn read_file(
    State(state): State<AppState>,
    Path(path): Path<String>,
) -> Response {
    match files::read_file(&state.workspace, &path) {
        Ok(bytes) => {
            let mut h = HeaderMap::new();
            h.insert(header::CONTENT_TYPE, content_type(&path).parse().unwrap());
            (StatusCode::OK, h, bytes).into_response()
        }
        Err(e) if e.contains("No such file") || e.contains("not found") => {
            err(StatusCode::NOT_FOUND, e)
        }
        Err(e) if e.contains("symlink") => err(StatusCode::FORBIDDEN, e),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

pub async fn write_file(
    State(state): State<AppState>,
    Path(path): Path<String>,
    body: Bytes,
) -> Response {
    let n = body.len();
    match files::write_file(&state.workspace, &path, &body) {
        Ok(()) => Json(json!({"written": path, "size": n})).into_response(),
        Err(e) if e.contains("symlink") => err(StatusCode::FORBIDDEN, e),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

pub async fn delete_file(
    State(state): State<AppState>,
    Path(path): Path<String>,
) -> Response {
    match files::delete_file(&state.workspace, &path) {
        Ok(true) => Json(json!({"deleted": path})).into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "file not found"),
        Err(e) if e.contains("symlink") => err(StatusCode::FORBIDDEN, e),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

// ─── helpers ─────────────────────────────────────────────────────

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({"error": msg.into()}))).into_response()
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
