//! HTTP handlers for the sandbox service.
//!
//! Backend-agnostic — every op routes through
//! [`crate::backend::Backend`], so the same handlers serve docker
//! containers and k8s+libkrun Pods.

use std::sync::Arc;

use ntex::util::Bytes;
use ntex::web::{self, HttpRequest, HttpResponse};
use serde::Deserialize;
use uuid::Uuid;

use crate::{auth, AppState};

type State = web::types::State<Arc<AppState>>;

// ─── helpers ─────────────────────────────────────────────────────

fn unauthorized() -> HttpResponse {
    HttpResponse::Unauthorized().json(&serde_json::json!({"error": "unauthorized"}))
}

fn err(status: u16, msg: impl Into<String>) -> HttpResponse {
    let s = msg.into();
    let mut resp = match status {
        400 => HttpResponse::BadRequest(),
        404 => HttpResponse::NotFound(),
        409 => HttpResponse::Conflict(),
        500 => HttpResponse::InternalServerError(),
        _ => HttpResponse::InternalServerError(),
    };
    resp.json(&serde_json::json!({"error": s}))
}

fn parse_uuid(s: &str) -> Result<Uuid, HttpResponse> {
    s.parse::<Uuid>().map_err(|_| err(400, "invalid session id (not a uuid)"))
}

fn is_safe_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

fn infer_content_type(path: &str) -> &'static str {
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

// ─── POST /sessions ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateSessionBody {
    /// Identifies the human creator. Drives per-user PVC mounting
    /// in the K8s backend (caches survive across every sandbox the
    /// user opens) and "one active session per user" scheduling.
    /// Constrained to `[a-zA-Z0-9_-]{1,64}`.
    pub user_id: String,
    /// Stable per-project id. The session is keyed on
    /// (`user_id`, `project_id`); re-opening with the same pair
    /// returns the existing session if one is alive. A different
    /// project_id from the same user implies a different sandbox
    /// — the previous one will be stopped (per-user PVC is RWO).
    pub project_id: String,
}

pub async fn create_session(
    req: HttpRequest,
    state: State,
    body: web::types::Json<CreateSessionBody>,
) -> HttpResponse {
    if !auth::check(&req, &state) { return unauthorized(); }

    let user_id = body.user_id.trim().to_string();
    if user_id.is_empty()
        || user_id.len() > 64
        || !user_id.chars().all(is_safe_id_char)
    {
        return err(400, "invalid user_id (alphanumeric / dash / underscore, max 64 chars)");
    }

    let project_id = body.project_id.trim().to_string();
    if project_id.is_empty()
        || project_id.len() > 64
        || !project_id.chars().all(is_safe_id_char)
    {
        return err(400, "invalid project_id (alphanumeric / dash / underscore, max 64 chars)");
    }

    // Re-attach existing session for THIS USER on this project.
    // (Same project_id from a different user = a different
    // sandbox; they each have their own clone of the project.)
    if let Some(id) = state.sessions.find_by_user_project(&user_id, &project_id) {
        if let Some(info) = state.sessions.get(&id) {
            return HttpResponse::Ok().json(&info);
        }
    }

    let session_id = Uuid::new_v4();
    let info = match state.backend.create(session_id, &user_id, &project_id).await {
        Ok(i) => i,
        Err(e) => return err(500, format!("backend.create: {e}")),
    };
    let stored = state.sessions.insert(session_id, info);
    HttpResponse::Created().json(&stored)
}

// ─── GET /sessions ───────────────────────────────────────────────

pub async fn list_sessions(req: HttpRequest, state: State) -> HttpResponse {
    if !auth::check(&req, &state) { return unauthorized(); }
    HttpResponse::Ok().json(&state.sessions.list())
}

// ─── GET /sessions/:id ───────────────────────────────────────────

pub async fn get_session(
    req: HttpRequest,
    state: State,
    path: web::types::Path<String>,
) -> HttpResponse {
    if !auth::check(&req, &state) { return unauthorized(); }
    let id = match parse_uuid(&path) { Ok(u) => u, Err(r) => return r };
    match state.sessions.get(&id) {
        Some(info) => HttpResponse::Ok().json(&info),
        None => err(404, "session not found"),
    }
}

// ─── DELETE /sessions/:id ────────────────────────────────────────

pub async fn stop_session(
    req: HttpRequest,
    state: State,
    path: web::types::Path<String>,
) -> HttpResponse {
    if !auth::check(&req, &state) { return unauthorized(); }
    let id = match parse_uuid(&path) { Ok(u) => u, Err(r) => return r };

    if state.sessions.get(&id).is_none() {
        return err(404, "session not found");
    }

    if let Err(e) = state.backend.stop(id).await {
        eprintln!("[sandbox] backend.stop({id}) failed: {e}");
        // Continue — we still want the registry entry gone.
    }
    state.sessions.remove(&id);

    HttpResponse::Ok().json(&serde_json::json!({"stopped": true, "session_id": id.to_string()}))
}

// ─── POST /sessions/:id/exec ─────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ExecBody {
    pub cmd: String,
    pub cwd: Option<String>,
    pub timeout_ms: Option<u64>,
}

pub async fn exec(
    req: HttpRequest,
    state: State,
    path: web::types::Path<String>,
    body: web::types::Json<ExecBody>,
) -> HttpResponse {
    if !auth::check(&req, &state) { return unauthorized(); }
    let id = match parse_uuid(&path) { Ok(u) => u, Err(r) => return r };

    if state.sessions.get(&id).is_none() {
        return err(404, "session not found");
    }

    let timeout_ms = body.timeout_ms.unwrap_or(60_000).min(600_000);
    let cwd = body.cwd.as_deref();

    match state.backend.exec(id, &body.cmd, cwd, Some(timeout_ms)).await {
        Ok(out) => HttpResponse::Ok().json(&serde_json::json!({
            "status": out.status,
            "stdout": out.stdout,
            "stderr": out.stderr,
            "timed_out": out.timed_out,
        })),
        Err(e) => err(500, format!("backend.exec: {e}")),
    }
}

// ─── GET /sessions/:id/file-tree ─────────────────────────────────

pub async fn file_tree(
    req: HttpRequest,
    state: State,
    path: web::types::Path<String>,
) -> HttpResponse {
    if !auth::check(&req, &state) { return unauthorized(); }
    let id = match parse_uuid(&path) { Ok(u) => u, Err(r) => return r };

    if state.sessions.get(&id).is_none() {
        return err(404, "session not found");
    }

    match state.backend.file_tree(id).await {
        Ok(entries) => HttpResponse::Ok().json(&serde_json::json!({"entries": entries})),
        Err(e) => err(500, format!("backend.file_tree: {e}")),
    }
}

// ─── GET /sessions/:id/files/{path} ──────────────────────────────

pub async fn read_file(
    req: HttpRequest,
    state: State,
    path: web::types::Path<(String, String)>,
) -> HttpResponse {
    if !auth::check(&req, &state) { return unauthorized(); }
    let (id_s, file_path) = path.into_inner();
    let id = match parse_uuid(&id_s) { Ok(u) => u, Err(r) => return r };

    if state.sessions.get(&id).is_none() {
        return err(404, "session not found");
    }

    match state.backend.read_file(id, &file_path).await {
        Ok(bytes) => HttpResponse::Ok()
            .content_type(infer_content_type(&file_path))
            .body(bytes),
        Err(e) if e.contains("No such file") || e.contains("file not found") || e.starts_with("read") => {
            err(404, e)
        }
        Err(e) => err(400, e),
    }
}

// ─── PUT /sessions/:id/files/{path} ──────────────────────────────

pub async fn write_file(
    req: HttpRequest,
    state: State,
    path: web::types::Path<(String, String)>,
    body: Bytes,
) -> HttpResponse {
    if !auth::check(&req, &state) { return unauthorized(); }
    let (id_s, file_path) = path.into_inner();
    let id = match parse_uuid(&id_s) { Ok(u) => u, Err(r) => return r };

    if state.sessions.get(&id).is_none() {
        return err(404, "session not found");
    }

    match state.backend.write_file(id, &file_path, &body).await {
        Ok(()) => HttpResponse::Ok().json(&serde_json::json!({
            "written": file_path,
            "size": body.len(),
        })),
        Err(e) => err(400, e),
    }
}

// ─── DELETE /sessions/:id/files/{path} ───────────────────────────

pub async fn delete_file(
    req: HttpRequest,
    state: State,
    path: web::types::Path<(String, String)>,
) -> HttpResponse {
    if !auth::check(&req, &state) { return unauthorized(); }
    let (id_s, file_path) = path.into_inner();
    let id = match parse_uuid(&id_s) { Ok(u) => u, Err(r) => return r };

    if state.sessions.get(&id).is_none() {
        return err(404, "session not found");
    }

    match state.backend.delete_file(id, &file_path).await {
        Ok(true) => HttpResponse::Ok().json(&serde_json::json!({"deleted": file_path})),
        Ok(false) => err(404, "file not found"),
        Err(e) => err(400, e),
    }
}
