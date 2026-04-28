//! HTTP handlers for the sandbox service.
//!
//! All handlers share the same shape: bearer-token check, parse the
//! path/body, dispatch to the docker / files / session module, and
//! map results to JSON responses with the right status code.

use std::sync::Arc;

use ntex::util::Bytes;
use ntex::web::{self, HttpRequest, HttpResponse};
use serde::Deserialize;
use uuid::Uuid;

use crate::{auth, docker, files, session, AppState};

type State = web::types::State<Arc<AppState>>;

// ─── Auth wrapper ────────────────────────────────────────────────

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

// ─── POST /sessions ──────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateSessionBody {
    /// Stable per-project id. Re-using the same `project_id` returns
    /// the existing session if one is alive.
    pub project_id: String,
}

pub async fn create_session(
    req: HttpRequest,
    state: State,
    body: web::types::Json<CreateSessionBody>,
) -> HttpResponse {
    if !auth::check(&req, &state) { return unauthorized(); }

    let project_id = body.project_id.trim().to_string();
    if project_id.is_empty() || project_id.len() > 64 || !project_id.chars().all(is_safe_id_char) {
        return err(400, "invalid project_id (alphanumeric / dash / underscore, max 64 chars)");
    }

    // Re-attach existing session if alive.
    if let Some(id) = state.sessions.find_by_project(&project_id) {
        if let Some(info) = state.sessions.get(&id) {
            return HttpResponse::Ok().json(&info);
        }
    }

    let session_id = Uuid::new_v4();
    let container_name = format!("zsbx-{}", session_id.simple());

    // Per-project workspace (NOT per-session — survives container churn).
    let workspace = state.config.workspace_root.join(&project_id);
    if let Err(e) = std::fs::create_dir_all(&workspace) {
        return err(500, format!("create workspace: {e}"));
    }

    // Spawn the container.
    let container_id = match docker::run_container(
        &state.config.image,
        &container_name,
        &state.config.network,
        &workspace,
        state.config.memory_mb,
        state.config.cpus,
        &project_id,
        &session_id.to_string(),
    ).await {
        Ok(id) => id,
        Err(e) => return err(500, format!("docker run: {e}")),
    };

    // Get its IP on the sandbox network.
    let container_ip = docker::container_ip(&container_id, &state.config.network)
        .await
        .unwrap_or_default();

    let info = state.sessions.insert(
        session_id, project_id, container_id, container_name,
        container_ip, workspace,
    );

    HttpResponse::Created().json(&info)
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

    let info = match state.sessions.get(&id) {
        Some(i) => i,
        None => return err(404, "session not found"),
    };

    if let Err(e) = docker::stop_container(&info.container_name).await {
        eprintln!("[sandbox] stop {}: {e}", info.container_name);
        // continue — we still want the registry entry gone
    }
    state.sessions.remove(&id);

    HttpResponse::Ok().json(&serde_json::json!({"stopped": true, "session_id": id.to_string()}))
}

// ─── POST /sessions/:id/exec ─────────────────────────────────────

#[derive(Deserialize)]
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

    let info = match state.sessions.get(&id) {
        Some(i) => i,
        None => return err(404, "session not found"),
    };

    let timeout_ms = body.timeout_ms.unwrap_or(60_000).min(600_000); // cap 10 min
    let cwd = body.cwd.as_deref();

    match docker::exec_in_container(&info.container_name, &body.cmd, cwd, Some(timeout_ms)).await {
        Ok(out) => HttpResponse::Ok().json(&serde_json::json!({
            "status": out.status,
            "stdout": out.stdout,
            "stderr": out.stderr,
        })),
        Err(e) => err(500, format!("docker exec: {e}")),
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

    let info = match state.sessions.get(&id) {
        Some(i) => i,
        None => return err(404, "session not found"),
    };

    let workspace = std::path::PathBuf::from(&info.workspace_path);
    match files::file_tree(&workspace) {
        Ok(entries) => HttpResponse::Ok().json(&serde_json::json!({"entries": entries})),
        Err(e) => err(500, format!("walk: {e}")),
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

    let info = match state.sessions.get(&id) {
        Some(i) => i,
        None => return err(404, "session not found"),
    };

    let workspace = std::path::PathBuf::from(&info.workspace_path);
    match files::read_file(&workspace, &file_path) {
        Ok(bytes) => HttpResponse::Ok()
            .content_type(infer_content_type(&file_path))
            .body(bytes),
        Err(e) if e.contains("No such file") || e.starts_with("read") => err(404, e),
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

    let info = match state.sessions.get(&id) {
        Some(i) => i,
        None => return err(404, "session not found"),
    };

    let workspace = std::path::PathBuf::from(&info.workspace_path);
    match files::write_file(&workspace, &file_path, &body) {
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

    let info = match state.sessions.get(&id) {
        Some(i) => i,
        None => return err(404, "session not found"),
    };

    let workspace = std::path::PathBuf::from(&info.workspace_path);
    match files::delete_file(&workspace, &file_path) {
        Ok(true) => HttpResponse::Ok().json(&serde_json::json!({"deleted": file_path})),
        Ok(false) => err(404, "file not found"),
        Err(e) => err(400, e),
    }
}

// ─── helpers ─────────────────────────────────────────────────────

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

// Re-export for the unused-warning guard.
#[allow(dead_code)]
const _USES_SESSION: fn() = || {
    let _ = session::start_idle_gc;
};
