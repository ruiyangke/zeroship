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
    s.parse::<Uuid>().map_err(|_| err(400, "invalid sandbox id (not a uuid)"))
}

/// Charset for user_id and project_id at the HTTP boundary.
///
/// **Tighter than DNS-1123 on purpose:** these IDs flow into k8s
/// resource names (PVC, Pod), label values, and YAML manifests.
/// k8s label values are restricted to `(([A-Za-z0-9][-A-Za-z0-9_.]*)?[A-Za-z0-9])?`
/// (max 63 chars) and resource names to lowercase DNS-1123. We
/// intersect both:
///
///   * lowercase a-z, digits 0-9, dash `-` only
///   * must start with [a-z0-9] (DNS-1123 + k8s-label both demand)
///   * length ≤ 50 (leaves headroom for prefixes like
///     `zsbx-userhome-<id>` to stay under 253-char DNS-1123)
///
/// Underscore is NOT allowed: previous versions accepted it and
/// `user_pvc_name` rewrote `_` → `-`, which collapsed `alice_1`
/// and `alice-1` to the same PVC — cross-user data bleed.
/// Uppercase is NOT allowed for the same reason: `Alice` and
/// `alice` would collapse. Today the validator + `user_pvc_name`
/// (lower-only, dash-only) are mutually self-consistent, so the
/// 1:1 between `user_id` and PVC name is restored.
fn is_safe_id_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'
}

/// Validate an HTTP-supplied id (user_id, project_id) at the
/// boundary. `is_safe_id_char` enforces the per-character rule;
/// this helper adds the boundary checks (non-empty, leading char,
/// length cap).
fn is_safe_id(id: &str) -> bool {
    if id.is_empty() || id.len() > 50 {
        return false;
    }
    let mut chars = id.chars();
    let first = match chars.next() {
        Some(c) => c,
        None => return false,
    };
    // First char must be alphanumeric (k8s + DNS-1123).
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    // Remaining chars: per-char rule.
    chars.all(is_safe_id_char)
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

// ─── GET /readyz ──────────────────────────────────────────────────
//
// 200 when the backend is healthy (last probe succeeded), 503
// otherwise. Unauthenticated — kubelet probes don't have the
// bearer token, and the response carries no sensitive info beyond
// "backend reachable / not reachable" which can be inferred from
// 5xx response patterns anyway.

pub async fn readyz(state: State) -> HttpResponse {
    if state.backend.is_healthy() {
        HttpResponse::Ok().json(&serde_json::json!({"status": "ready"}))
    } else {
        HttpResponse::ServiceUnavailable()
            .json(&serde_json::json!({"status": "backend-unhealthy"}))
    }
}

// ─── POST /sandboxes ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateSandboxBody {
    /// Identifies the human creator. Drives per-user PVC mounting
    /// in the K8s backend (caches survive across every sandbox the
    /// user opens) and "one active sandbox per user" scheduling.
    /// Constrained to `[a-zA-Z0-9_-]{1,64}`.
    pub user_id: String,
    /// Stable per-project id. The sandbox is keyed on
    /// (`user_id`, `project_id`); re-opening with the same pair
    /// returns the existing sandbox if one is alive. A different
    /// project_id from the same user implies a different sandbox
    /// — the previous one will be stopped (per-user PVC is RWO).
    pub project_id: String,
}

pub async fn create_sandbox(
    req: HttpRequest,
    state: State,
    body: web::types::Json<CreateSandboxBody>,
) -> HttpResponse {
    if !auth::check(&req, &state) { return unauthorized(); }

    let user_id = body.user_id.trim().to_string();
    if !is_safe_id(&user_id) {
        return err(
            400,
            "invalid user_id: must be [a-z0-9-]{1,50} starting with [a-z0-9]",
        );
    }

    let project_id = body.project_id.trim().to_string();
    if !is_safe_id(&project_id) {
        return err(
            400,
            "invalid project_id: must be [a-z0-9-]{1,50} starting with [a-z0-9]",
        );
    }

    // Re-attach existing sandbox for THIS USER on this project.
    // (Same project_id from a different user = a different
    // sandbox; they each have their own clone of the project.)
    if let Some(id) = state.sandboxes.find_by_user_project(&user_id, &project_id) {
        if let Some(info) = state.sandboxes.get(&id) {
            return HttpResponse::Ok().json(&info);
        }
    }

    let sandbox_id = Uuid::new_v4();
    let info = match state.backend.create(sandbox_id, &user_id, &project_id).await {
        Ok(i) => i,
        Err(e) => return err(500, format!("backend.create: {e}")),
    };
    let stored = state.sandboxes.insert(sandbox_id, info);
    HttpResponse::Created().json(&stored)
}

// ─── GET /sandboxes ───────────────────────────────────────────────
//
// Cross-tenant scope: the bearer token gates "who can call the
// API," but the API was designed to be called by ONE control
// plane on behalf of MANY end-users. So the per-request scope is
// determined by `?user_id=<id>` — without it we refuse rather
// than dump every user's PVC names + pod names + IDs to whoever
// holds the token. (The previous behavior was a silent cross-
// tenant info disclosure for any token holder.)

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// Required. Filters the response to sandboxes owned by this
    /// user. Returning all sandboxes globally would leak PVC names,
    /// project ids, and pod names of unrelated users.
    pub user_id: Option<String>,
}

pub async fn list_sandboxes(
    req: HttpRequest,
    state: State,
    query: web::types::Query<ListQuery>,
) -> HttpResponse {
    if !auth::check(&req, &state) { return unauthorized(); }
    let Some(user_id) = query.into_inner().user_id else {
        return err(
            400,
            "list requires ?user_id=<id> — cross-user listing is not exposed",
        );
    };
    if !is_safe_id(&user_id) {
        return err(400, "invalid user_id");
    }
    let filtered: Vec<_> = state
        .sandboxes
        .list()
        .into_iter()
        .filter(|s| s.user_id == user_id)
        .collect();
    HttpResponse::Ok().json(&filtered)
}

// ─── GET /sandboxes/:id ───────────────────────────────────────────
//
// Same model as the list endpoint — the caller must assert which
// user they're acting on behalf of via `?user_id=`. A wrong
// user_id gets 404 (not 403) so the API doesn't become a
// "does sandbox X exist?" oracle.

#[derive(Debug, Deserialize)]
pub struct GetQuery {
    pub user_id: Option<String>,
}

pub async fn get_sandbox(
    req: HttpRequest,
    state: State,
    path: web::types::Path<String>,
    query: web::types::Query<GetQuery>,
) -> HttpResponse {
    if !auth::check(&req, &state) { return unauthorized(); }
    let id = match parse_uuid(&path) { Ok(u) => u, Err(r) => return r };
    let Some(user_id) = query.into_inner().user_id else {
        return err(400, "get requires ?user_id=<id>");
    };
    if !is_safe_id(&user_id) {
        return err(400, "invalid user_id");
    }
    match state.sandboxes.get(&id) {
        Some(info) if info.user_id == user_id => HttpResponse::Ok().json(&info),
        // 404 for both "no such sandbox" and "wrong owner" — the
        // API must not reveal the difference.
        _ => err(404, "sandbox not found"),
    }
}

/// Verify the request's `?user_id=<id>` matches the sandbox's
/// owner. Returns the parsed sandbox id on success. On any
/// failure (bad uuid, missing/bad user_id, sandbox not found,
/// owner mismatch) returns 404 — same response regardless, so
/// the API doesn't become an existence oracle.
fn require_owner(
    req: &HttpRequest,
    state: &AppState,
    raw_id: &str,
) -> Result<Uuid, HttpResponse> {
    let id = parse_uuid(raw_id).map_err(|r| r)?;
    // Pull user_id from the query string. Hand-parse to avoid
    // pulling another extractor through every signature; the
    // string is short and the format is fixed.
    let user_id = req
        .uri()
        .query()
        .and_then(|q| {
            q.split('&').find_map(|kv| {
                let (k, v) = kv.split_once('=')?;
                if k == "user_id" {
                    Some(v.to_string())
                } else {
                    None
                }
            })
        })
        .unwrap_or_default();
    if !is_safe_id(&user_id) {
        return Err(err(404, "sandbox not found"));
    }
    match state.sandboxes.get(&id) {
        Some(info) if info.user_id == user_id => Ok(id),
        _ => Err(err(404, "sandbox not found")),
    }
}

// ─── DELETE /sandboxes/:id ────────────────────────────────────────

pub async fn stop_sandbox(
    req: HttpRequest,
    state: State,
    path: web::types::Path<String>,
) -> HttpResponse {
    if !auth::check(&req, &state) { return unauthorized(); }
    let id = match require_owner(&req, &state, &path) { Ok(u) => u, Err(r) => return r };

    if let Err(e) = state.backend.stop(id).await {
        // **Don't** swallow: surface so the operator sees the
        // failure. We still remove from the registry — leaving a
        // stale entry would never resolve, and the runtime
        // (Pod/container) is the controller's responsibility to
        // chase down via cluster-side cleanup.
        state.sandboxes.remove(&id);
        return err(500, format!("backend.stop: {e}"));
    }
    state.sandboxes.remove(&id);

    HttpResponse::Ok().json(&serde_json::json!({"stopped": true, "sandbox_id": id.to_string()}))
}

// ─── POST /sandboxes/:id/exec ─────────────────────────────────────

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
    let id = match require_owner(&req, &state, &path) { Ok(u) => u, Err(r) => return r };

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

// ─── GET /sandboxes/:id/file-tree ─────────────────────────────────

pub async fn file_tree(
    req: HttpRequest,
    state: State,
    path: web::types::Path<String>,
) -> HttpResponse {
    if !auth::check(&req, &state) { return unauthorized(); }
    let id = match require_owner(&req, &state, &path) { Ok(u) => u, Err(r) => return r };

    match state.backend.file_tree(id).await {
        Ok(entries) => HttpResponse::Ok().json(&serde_json::json!({"entries": entries})),
        Err(e) => err(500, format!("backend.file_tree: {e}")),
    }
}

// ─── GET /sandboxes/:id/files/{path} ──────────────────────────────

pub async fn read_file(
    req: HttpRequest,
    state: State,
    path: web::types::Path<(String, String)>,
) -> HttpResponse {
    if !auth::check(&req, &state) { return unauthorized(); }
    let (id_s, file_path) = path.into_inner();
    let id = match require_owner(&req, &state, &id_s) { Ok(u) => u, Err(r) => return r };

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

// ─── PUT /sandboxes/:id/files/{path} ──────────────────────────────

pub async fn write_file(
    req: HttpRequest,
    state: State,
    path: web::types::Path<(String, String)>,
    body: Bytes,
) -> HttpResponse {
    if !auth::check(&req, &state) { return unauthorized(); }
    let (id_s, file_path) = path.into_inner();
    let id = match require_owner(&req, &state, &id_s) { Ok(u) => u, Err(r) => return r };

    match state.backend.write_file(id, &file_path, &body).await {
        Ok(()) => HttpResponse::Ok().json(&serde_json::json!({
            "written": file_path,
            "size": body.len(),
        })),
        Err(e) => err(400, e),
    }
}

// ─── DELETE /sandboxes/:id/files/{path} ───────────────────────────

pub async fn delete_file(
    req: HttpRequest,
    state: State,
    path: web::types::Path<(String, String)>,
) -> HttpResponse {
    if !auth::check(&req, &state) { return unauthorized(); }
    let (id_s, file_path) = path.into_inner();
    let id = match require_owner(&req, &state, &id_s) { Ok(u) => u, Err(r) => return r };

    match state.backend.delete_file(id, &file_path).await {
        Ok(true) => HttpResponse::Ok().json(&serde_json::json!({"deleted": file_path})),
        Ok(false) => err(404, "file not found"),
        Err(e) => err(400, e),
    }
}
