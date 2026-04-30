//! HTTP handlers for the agent.
//!
//! ## Endpoints
//!
//! **Unauthenticated** (no `Authorization` required — used by k8s
//! probes and the controller's pre-handshake feature detect):
//!
//!   - `GET /livez`    — always 200 if the process is alive
//!   - `GET /readyz`   — 200 normally, 503 while draining
//!   - `GET /healthz`  — alias for `/livez` (back-compat)
//!   - `GET /version`  — agent version + protocol + capabilities
//!
//! **Auth-gated** (require `Authorization: Bearer <token>` matching
//! the file-mounted token; checked inline at the top of each handler
//! via [`check_token`]):
//!
//!   - `POST /exec`              — run shell command (JSON in/out)
//!   - `GET  /tree`              — workspace file listing
//!   - `GET  /files/{path}*`     — read file
//!   - `PUT  /files/{path}*`     — write file (raw bytes)
//!   - `DELETE /files/{path}*`   — delete file
//!   - `POST /shutdown`          — flip drain flag (graceful drain)

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ntex::util::Bytes;
use ntex::web::{self, HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::json;

use crate::audit;
use crate::auth::Token;
use crate::exec;
use crate::files::Workspace;
use crate::version;

/// State shared by every handler. Cheap to clone (`Arc` inside).
#[derive(Clone, Debug)]
pub struct AppState {
    pub token: Arc<Token>,
    pub workspace: Arc<Workspace>,
    /// `true` when the agent is shutting down — `/readyz` returns 503
    /// so orchestrators stop sending traffic. Set by SIGTERM handler
    /// or `POST /shutdown`.
    pub draining: Arc<AtomicBool>,
    /// Unix timestamp at agent start; reported via `/version`.
    pub started_at_unix: u64,
}

impl AppState {
    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Relaxed)
    }
    pub fn mark_draining(&self) {
        self.draining.store(true, Ordering::Relaxed);
    }
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
/// compares against the loaded token. On miss, emits an audit event
/// (so failed-auth attempts spike visibly in the security pipeline)
/// and returns false; the handler uses that to short-circuit with 401.
fn check_token(req: &HttpRequest, state: &AppState) -> bool {
    let h = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok());
    let ok = state.token.verify_header(h);
    if !ok {
        let path = req.path();
        let method = req.method().as_str();
        audit::record(
            audit::events::AUTH_FAIL,
            &format!("method={method} path={path}"),
        );
    }
    ok
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

// ─── liveness / readiness / version ──────────────────────────────
//
// All three are unauthenticated:
//   - `/livez`  — "is the process alive?" Always 200 if responding.
//                  Used as k8s liveness probe.
//   - `/readyz` — "should I send traffic?" 200 normally; 503 while
//                  draining OR when the PID 1 reaper is not healthy.
//                  Used as k8s readiness probe.
//   - `/version`— version + capabilities, used by controllers for
//                  feature detection.
//
// `/healthz` is kept as an alias for `/livez` for back-compat.

pub async fn livez() -> HttpResponse {
    HttpResponse::Ok().json(&json!({"status": "ok"}))
}

pub async fn readyz(state: State) -> HttpResponse {
    if state.is_draining() {
        return HttpResponse::ServiceUnavailable()
            .json(&json!({"status": "draining"}));
    }
    if !crate::reap::is_healthy() {
        // The PID 1 reaper failed to install. Inside a libkrun VM
        // this means zombies pile up unbounded — so we report
        // not-ready rather than silently degrade.
        return HttpResponse::ServiceUnavailable()
            .json(&json!({"status": "reaper-down"}));
    }
    HttpResponse::Ok().json(&json!({"status": "ready"}))
}

pub async fn version_info(state: State) -> HttpResponse {
    HttpResponse::Ok().json(&json!({
        "agent_version": version::AGENT_VERSION,
        "git_commit": version::GIT_COMMIT,
        "protocol_version": version::PROTOCOL_VERSION,
        "capabilities": version::CAPABILITIES,
        "started_at_unix": state.started_at_unix,
    }))
}

// ─── /shutdown — controller-initiated drain ──────────────────────
//
// POST /shutdown (auth-gated) flips the agent into draining mode.
// Subsequent `/readyz` returns 503 so the orchestrator (k8s preStop
// hook, controller, etc) stops sending traffic. Existing in-flight
// requests run to completion. The agent does NOT exit — that's the
// orchestrator's job; we just stop being ready.
//
// This is the "graceful drain without process exit" hook for
// preStop-style lifecycle management.

pub async fn shutdown(req: HttpRequest, state: State) -> HttpResponse {
    if !check_token(&req, &state) { return unauthorized(); }
    state.mark_draining();
    tracing::info!("shutdown requested via /shutdown — readyz will now report 503");
    HttpResponse::Ok().json(&json!({"draining": true}))
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
        .unwrap_or_else(|| state.workspace.path().to_str().unwrap_or("/workspace"));
    let timeout = body.timeout_ms.unwrap_or(exec::DEFAULT_TIMEOUT_MS);
    match exec::run(&body.cmd, cwd, timeout).await {
        Ok(out) => {
            if out.timed_out {
                audit::record(audit::events::EXEC_TIMEOUT, &format!("timeout_ms={timeout}"));
            }
            if out.stdout_truncated || out.stderr_truncated {
                audit::record(
                    audit::events::EXEC_TRUNCATED,
                    &format!(
                        "stdout_truncated={} stderr_truncated={}",
                        out.stdout_truncated, out.stderr_truncated
                    ),
                );
            }
            HttpResponse::Ok().json(&out)
        }
        Err(e) => err(500, e),
    }
}

// ─── /tree ───────────────────────────────────────────────────────

pub async fn file_tree(req: HttpRequest, state: State) -> HttpResponse {
    if !check_token(&req, &state) { return unauthorized(); }

    match state.workspace.file_tree() {
        Ok(tree) => HttpResponse::Ok().json(&tree),
        Err(e) => err(500, e),
    }
}

// ─── /files/{path}* ──────────────────────────────────────────────

/// Map a `files::Workspace` error string to an HTTP response and
/// emit the matching audit event. Centralizes the string-matching so
/// a wording change in `files::map_open_err` only needs an update
/// here, and the audit-event mapping can never silently drift across
/// the three file handlers.
///
/// Also captures: oversize → 400 + `fs.size_reject`, NotFound → 404
/// (no audit; ENOENT is normal user behavior, not an attack signal).
fn fs_error_response(op: &'static str, path: &str, e: String, write_size: Option<usize>) -> HttpResponse {
    if e.contains("symlink") {
        audit::record(audit::events::FS_SYMLINK_REJECT, &format!("op={op} path={path}"));
        return err(403, e);
    }
    if e.contains("escapes") {
        audit::record(audit::events::FS_ESCAPE_REJECT, &format!("op={op} path={path}"));
        return err(403, e);
    }
    if e.contains("too large") {
        let size = write_size.unwrap_or(0);
        audit::record(
            audit::events::FS_SIZE_REJECT,
            &format!("op={op} path={path} size={size}"),
        );
        return err(400, e);
    }
    if e.contains("No such file") {
        return err(404, e);
    }
    err(400, e)
}

pub async fn read_file(
    req: HttpRequest,
    state: State,
    path: web::types::Path<String>,
) -> HttpResponse {
    if !check_token(&req, &state) { return unauthorized(); }

    let p = path.into_inner();
    match state.workspace.read_file(&p) {
        Ok(bytes) => HttpResponse::Ok()
            .content_type(content_type(&p))
            .body(bytes),
        Err(e) => fs_error_response("read", &p, e, None),
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
    match state.workspace.write_file(&p, &body) {
        Ok(()) => HttpResponse::Ok().json(&json!({"written": p, "size": n})),
        Err(e) => fs_error_response("write", &p, e, Some(n)),
    }
}

pub async fn delete_file(
    req: HttpRequest,
    state: State,
    path: web::types::Path<String>,
) -> HttpResponse {
    if !check_token(&req, &state) { return unauthorized(); }

    let p = path.into_inner();
    match state.workspace.delete_file(&p) {
        Ok(true) => HttpResponse::Ok().json(&json!({"deleted": p})),
        Ok(false) => err(404, format!("delete {p}: No such file or directory")),
        Err(e) => fs_error_response("delete", &p, e, None),
    }
}
