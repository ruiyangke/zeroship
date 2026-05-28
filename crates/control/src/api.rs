//! Admin API handlers — app CRUD, deploy, plan, usage.

use std::path::Path as StdPath;
use std::sync::Arc;
use std::time::Duration;

use futures::Stream;
use ntex::web;
use ntex::web::types::{Json, Path, State};
use serde::Deserialize;
use uuid::Uuid;
use zeroship_auth::audit::{self as auth_audit, AuditEvent};
use zeroship_authz::{Action, Resource};

use crate::authz_guard::AuthzGuard;
use crate::deploy::{self, IngestError};
use crate::registry::RegistryError;
use crate::AppState;

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CreateAppBody {
    pub name: String,
    #[serde(default = "default_plan")]
    pub plan_id: String,
}

fn default_plan() -> String {
    "free".to_string()
}

#[derive(Deserialize)]
pub struct SetPlanBody {
    pub plan_id: String,
}

// ---------------------------------------------------------------------------
// Error → HttpResponse
// ---------------------------------------------------------------------------

fn error_response(e: RegistryError) -> web::HttpResponse {
    match e {
        RegistryError::NotFound(msg) => {
            web::HttpResponse::NotFound().json(&serde_json::json!({ "error": msg }))
        }
        RegistryError::AlreadyExists(msg) => {
            web::HttpResponse::Conflict().json(&serde_json::json!({ "error": msg }))
        }
        RegistryError::InvalidInput(msg) => {
            web::HttpResponse::BadRequest().json(&serde_json::json!({ "error": msg }))
        }
        RegistryError::Database(msg) => web::HttpResponse::InternalServerError()
            .json(&serde_json::json!({ "error": msg })),
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn create_app(
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
    body: Json<CreateAppBody>,
) -> web::HttpResponse {
    if let Err(resp) = authz.require(Action::AppsWrite, Resource::Any, &state).await {
        return resp;
    }
    match state.registry.create_app(&body.name, &body.plan_id).await {
        Ok(record) => {
            // Include api_key in the create response (it's skipped from normal serialization)
            let mut json = serde_json::to_value(&record).unwrap();
            json["api_key"] = serde_json::Value::String(record.api_key.clone());
            web::HttpResponse::Created().json(&json)
        }
        Err(e) => error_response(e),
    }
}

pub async fn list_apps(
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Err(resp) = authz.require(Action::AppsRead, Resource::Any, &state).await {
        return resp;
    }
    match state.registry.list_apps().await {
        Ok(apps) => web::HttpResponse::Ok().json(&apps),
        Err(e) => error_response(e),
    }
}

pub async fn get_app(
    id: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    let uid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error":"invalid uuid"}))
        }
    };
    if let Err(resp) = authz
        .require(Action::AppsRead, Resource::App { id: uid.to_string() }, &state)
        .await
    {
        return resp;
    }
    match state.registry.get_app(&uid).await {
        Ok(Some(record)) => web::HttpResponse::Ok().json(&record),
        Ok(None) => {
            web::HttpResponse::NotFound().json(&serde_json::json!({"error":"app not found"}))
        }
        Err(e) => error_response(e),
    }
}

pub async fn delete_app(
    id: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    let uid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error":"invalid uuid"}))
        }
    };
    if let Err(resp) = authz
        .require(Action::AppsDelete, Resource::App { id: uid.to_string() }, &state)
        .await
    {
        return resp;
    }
    // Delete from VFS first (ignore NotFound — bundle may not exist yet).
    let app_id_str = uid.to_string();
    if let Err(e) = state.vfs.delete(&app_id_str) {
        match e {
            zeroship_bundle::VfsError::NotFound(_) => { /* ok */ }
            other => {
                return web::HttpResponse::InternalServerError()
                    .json(&serde_json::json!({"error": other.to_string()}));
            }
        }
    }
    match state.registry.delete_app(&uid).await {
        Ok(true) => web::HttpResponse::Ok().json(&serde_json::json!({"deleted": true})),
        Ok(false) => {
            web::HttpResponse::NotFound().json(&serde_json::json!({"error":"app not found"}))
        }
        Err(e) => error_response(e),
    }
}

/// Streaming `.zship` ingest. Replaces the legacy raw-bundle path —
/// deploy bundles now arrive as zstd-compressed tar archives carrying
/// `manifest.json` + `blobs/<sha256>` entries. See
/// `docs/reference/zship.md` for the wire format and ingestion
/// algorithm.
pub async fn deploy(
    req: web::HttpRequest,
    id: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
    mut body: web::types::Payload,
) -> web::HttpResponse {
    // Authz + uuid + content-type rejections happen BEFORE any body byte
    // is consumed, so rejected callers cannot tie up tmp file slots.
    let uid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error":"invalid uuid"}))
        }
    };
    if let Err(resp) = authz
        .require(Action::AppsDeploy, Resource::App { id: uid.to_string() }, &state)
        .await
    {
        return resp;
    }

    // Hard cut: only `application/x-zship` is accepted. The legacy
    // raw `.appbundle` and `application/javascript` paths are gone.
    let content_type = req
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !is_zship_content_type(content_type) {
        return web::HttpResponse::UnsupportedMediaType().json(&serde_json::json!({
            "error": "unsupported content type",
            "detail": "expected application/x-zship",
        }));
    }

    // Stream the request body to a tmp file under the configured
    // deploy tmp dir. Tmp files live for the duration of the deploy
    // and are removed after ingest (success or error). Path includes
    // a uuid so concurrent deploys don't trample each other. The
    // helper itself enforces `MAX_COMPRESSED_BYTES` while writing —
    // see `stream_body_to_tmp_file`. ntex's `Payload` implements
    // `Stream<Item = Result<Bytes, PayloadError>>` directly, so the
    // generic helper accepts it without an adapter.
    let tmp_path = state
        .deploy_tmp_dir
        .join(format!("zeroship-deploy-{}.zship", uuid::Uuid::new_v4().simple()));

    match stream_body_to_tmp_file(
        &mut body,
        &tmp_path,
        zeroship_bundle::MAX_COMPRESSED_BYTES as u64,
    )
    .await
    {
        Ok(_written) => { /* fall through to mmap + ingest */ }
        Err(StreamToTmpError::TooLarge { cap, observed }) => {
            return web::HttpResponse::PayloadTooLarge().json(&serde_json::json!({
                "error": "deploy too large",
                "cap_bytes": cap,
                "observed_bytes": observed,
            }));
        }
        Err(StreamToTmpError::PayloadError(detail)) => {
            return web::HttpResponse::BadRequest().json(&serde_json::json!({
                "error": "payload error",
                "detail": detail,
            }));
        }
        Err(e) => {
            tracing::error!(error = %e, path = %tmp_path.display(), "deploy: streaming to tmp failed");
            return web::HttpResponse::InternalServerError().json(&serde_json::json!({
                "error": "deploy temp storage unavailable",
            }));
        }
    }

    // mmap + ingest. The std::fs::File::open is sync but cheap (no I/O
    // beyond opening a fd); Mmap::map sets up VM mappings without
    // reading bytes. tar/zstd then page-fault through the slice, which
    // the kernel services from page cache.
    let file = match std::fs::File::open(&tmp_path) {
        Ok(f) => f,
        Err(e) => {
            tracing::error!(error = %e, path = %tmp_path.display(), "deploy: tmp re-open failed");
            let _ = compio::fs::remove_file(&tmp_path).await;
            return web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({"error":"deploy temp readback failed"}));
        }
    };
    // SAFETY: tmp file is owned by this handler, written exclusively by
    // us (create_new), fsynced before mapping, and not modified by any
    // other process for the lifetime of `mmap`.
    #[allow(unsafe_code)]
    let mmap = match unsafe { memmap2::Mmap::map(&file) } {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(error = %e, path = %tmp_path.display(), "deploy: mmap failed");
            drop(file);
            let _ = compio::fs::remove_file(&tmp_path).await;
            return web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({"error":"deploy temp mmap failed"}));
        }
    };

    let result = deploy::ingest(&state.blob_store, &uid, &mmap[..]).await;

    // Drop mmap + file before unlinking. On Linux unlink-while-mapped
    // is fine, but explicit drop avoids edge cases on other platforms.
    drop(mmap);
    drop(file);
    let _ = compio::fs::remove_file(&tmp_path).await;

    match result {
        Ok(success) => {
            // Atomic UPDATE: deploy_hash + manifest_json land together
            // so the gateway never sees half-applied state.
            match state
                .registry
                .set_deploy_with_manifest(&uid, &success.deploy_hash, &success.manifest_json)
                .await
            {
                Ok(true) => web::HttpResponse::Ok().json(&serde_json::json!({
                    "deploy_hash": success.deploy_hash,
                    "blobs_uploaded": success.blobs_uploaded,
                    "blobs_deduped": success.blobs_deduped,
                })),
                Ok(false) => web::HttpResponse::NotFound()
                    .json(&serde_json::json!({"error": "app not found"})),
                Err(e) => error_response(e),
            }
        }
        Err(e) => ingest_error_to_response(e),
    }
}

/// Permissive content-type check. We accept the canonical
/// `application/x-zship` plus parameterised variants like
/// `application/x-zship; charset=utf-8` (some clients add charset
/// even on binary uploads).
fn is_zship_content_type(value: &str) -> bool {
    let primary = value.split(';').next().unwrap_or("").trim();
    primary.eq_ignore_ascii_case("application/x-zship")
}

/// Map the structured ingest error to an HTTP response.
fn ingest_error_to_response(e: IngestError) -> web::HttpResponse {
    match e {
        IngestError::BadRequest { error, detail } => web::HttpResponse::BadRequest()
            .json(&serde_json::json!({"error": error, "detail": detail})),
        IngestError::TooLarge { cap_bytes, observed_bytes } => {
            web::HttpResponse::PayloadTooLarge().json(&serde_json::json!({
                "error": "deploy too large",
                "cap_bytes": cap_bytes,
                "observed_bytes": observed_bytes,
            }))
        }
        IngestError::UnsupportedMediaType => web::HttpResponse::UnsupportedMediaType()
            .json(&serde_json::json!({
                "error": "unsupported content type",
                "detail": "expected application/x-zship",
            })),
        IngestError::BlobStoreUnavailable(detail) => web::HttpResponse::ServiceUnavailable()
            .json(&serde_json::json!({
                "error": "blob store unavailable",
                "detail": detail,
            })),
        IngestError::Internal(detail) => web::HttpResponse::InternalServerError()
            .json(&serde_json::json!({"error": "internal", "detail": detail})),
    }
}

pub async fn set_plan(
    id: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
    body: Json<SetPlanBody>,
) -> web::HttpResponse {
    let uid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error": "invalid uuid"}))
        }
    };
    if let Err(resp) = authz
        .require(Action::BillingWrite, Resource::App { id: uid.to_string() }, &state)
        .await
    {
        return resp;
    }
    match state.registry.set_plan(&uid, &body.plan_id).await {
        Ok(true) => web::HttpResponse::Ok().json(&serde_json::json!({"updated": true})),
        Ok(false) => {
            web::HttpResponse::NotFound().json(&serde_json::json!({"error":"app not found"}))
        }
        Err(e) => error_response(e),
    }
}

pub async fn get_usage(
    id: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    let uid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error":"invalid uuid"}))
        }
    };
    if let Err(resp) = authz
        .require(Action::BillingRead, Resource::App { id: uid.to_string() }, &state)
        .await
    {
        return resp;
    }
    match state.registry.get_usage(&uid).await {
        Ok(usage) => web::HttpResponse::Ok().json(&usage),
        Err(e) => error_response(e),
    }
}

pub async fn get_app_logs(
    id: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    let uid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({"error":"invalid uuid"}))
        }
    };
    if let Err(resp) = authz
        .require(Action::DeploymentsRead, Resource::App { id: uid.to_string() }, &state)
        .await
    {
        return resp;
    }

    let mut lines = Vec::new();
    let mut errors = Vec::new();
    for worker_url in &state.worker_urls {
        match fetch_worker_logs(worker_url, state.worker_key.expose_secret(), &uid).await {
            Ok(mut worker_lines) => lines.append(&mut worker_lines),
            Err(e) => {
                tracing::warn!(
                    worker_url = %worker_url,
                    app_id = %uid,
                    error = %e,
                    "control: worker log fetch failed",
                );
                errors.push(format!("{worker_url}: {e}"));
            }
        }
    }

    if lines.is_empty() && !errors.is_empty() && errors.len() == state.worker_urls.len() {
        return web::HttpResponse::BadGateway().json(&serde_json::json!({
            "error": "worker logs unavailable",
            "details": errors,
        }));
    }

    web::HttpResponse::Ok().json(&lines)
}

async fn fetch_worker_logs(
    worker_url: &str,
    worker_key: &str,
    app_id: &Uuid,
) -> Result<Vec<String>, String> {
    let url = format!("{}/logs/{app_id}", worker_url.trim_end_matches('/'));
    let client = cyper::Client::new();
    let mut builder = client
        .get(&url)
        .map_err(|e| format!("invalid worker URL: {e}"))?;
    if !worker_key.is_empty() {
        builder = builder
            .header("authorization", &format!("Bearer {worker_key}"))
            .map_err(|e| format!("invalid auth header: {e}"))?;
    }

    let response = compio::time::timeout(Duration::from_secs(2), builder.send())
        .await
        .map_err(|_| "request timeout".to_string())?
        .map_err(|e| format!("request failed: {e}"))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("read body: {e}"))?;
    if !status.is_success() {
        let body = String::from_utf8_lossy(&bytes);
        return Err(format!(
            "HTTP {} {}: {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or(""),
            body,
        ));
    }

    serde_json::from_slice::<Vec<String>>(&bytes)
        .map_err(|e| format!("parse logs JSON: {e}"))
}

// ---------------------------------------------------------------------------
// /auth/callback — control plane OIDC RP callback (P3-U7 / U8)
// ---------------------------------------------------------------------------
//
// `console.zeroship.ai` is registered with hydra as a first-party OIDC
// client (`skip_consent=true`). The control plane is the relying party:
// it owns the authorize redirect, the stash cookie, and the callback
// code exchange. This is the canonical (and only) console-auth surface
// since U8 retired the legacy `/auth/login`, `/auth/register`,
// `/auth/userinfo`, `/auth/consent`, `/auth/authorize`, and
// `/auth/google/*` handlers along with `auth_service`.

/// Handle `GET /auth/callback?code=…&state=…` on `console.zeroship.ai`.
///
/// Reads the signed `__Host-zs_console_stash` cookie, exchanges the
/// code with hydra via `ConsoleOidcRp::finish_callback`, persists a
/// row in `auth.console_sessions`, sets `__Host-zs_console_session`,
/// clears the stash cookie, and 302s back to the original path the
/// user was trying to reach when the dance started.
pub async fn auth_callback(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    let oidc_rp = &state.oidc_rp;
    let pg = &state.auth_pg;

    // 1. Parse query (code + state). Hydra may also send `error=...`
    //    for user-denied consent; surface it directly.
    let query_str = req.uri().query().unwrap_or("");
    let mut code: Option<String> = None;
    let mut state_param: Option<String> = None;
    let mut oauth_error: Option<String> = None;
    for (k, v) in url::form_urlencoded::parse(query_str.as_bytes()) {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state_param = Some(v.into_owned()),
            "error" => oauth_error = Some(v.into_owned()),
            _ => {}
        }
    }
    if let Some(e) = oauth_error {
        return render_callback_error(state.insecure_dev, &format!("oauth error: {e}"));
    }
    let (Some(code), Some(state_param)) = (code, state_param) else {
        return render_callback_error(
            state.insecure_dev,
            "missing code or state query parameter",
        );
    };

    // 2. Read the signed stash cookie.
    let cookie_header = req
        .headers()
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let Some(stash) = crate::oidc_rp::parse_console_stash_cookie(cookie_header, state.insecure_dev) else {
        return render_callback_error(state.insecure_dev, "missing stash cookie");
    };

    // 3. Exchange the code with hydra + verify the ID token.
    let (claims, original_path) = match oidc_rp
        .finish_callback(&code, &state_param, &stash)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "control: oidc callback failed");
            return render_callback_error(state.insecure_dev, &e.to_string());
        }
    };

    // 4. Create a per-origin console session row.
    let session = match crate::console_sessions::create(pg, &claims).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "control: console session create failed");
            return render_callback_error(state.insecure_dev, "session create failed");
        }
    };
    let user_id = Uuid::parse_str(&claims.sub).ok();
    let ev = AuditEvent {
        event_type: "console_session_create",
        outcome: "success",
        user_id: user_id.as_ref(),
        client_id: Some("console.zeroship.ai"),
        auth_method: Some("oidc"),
        detail: serde_json::json!({
            "session_id": session.id.to_string(),
            "issuer": claims.iss,
            "email_verified": claims.email_verified,
        }),
        ..AuditEvent::from_request(&req)
    };
    if let Err(err) = auth_audit::emit_strict(pg, &ev).await {
        tracing::error!(error = %err, "control: console session audit insert failed");
        return render_callback_error(state.insecure_dev, "session create failed");
    }

    // 5. 302 back to the original path, set console-session cookie,
    //    clear the stash cookie. Two `Set-Cookie` headers on one
    //    response is valid per RFC 6265 §3.
    let mut builder = web::HttpResponse::Found();
    builder.header("location", sanitize_oidc_original_path(&original_path));
    builder.header(
        "set-cookie",
        crate::oidc_rp::set_console_session_cookie(&session.id, state.insecure_dev),
    );
    builder.header(
        "set-cookie",
        crate::oidc_rp::clear_console_stash_cookie(state.insecure_dev),
    );
    builder.finish()
}

/// Render the failed-callback page. Generic on purpose — leaking the
/// hydra error string to the user would help an attacker probe. The
/// structured error is already in the control log at warn / error.
fn render_callback_error(_insecure_dev: bool, msg: &str) -> web::HttpResponse {
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>Sign-in failed</title>\
        <h1>Sign-in failed</h1><p>{}</p>\
        <p><a href=\"/\">Back to dashboard</a></p>",
        html_escape(msg),
    );
    web::HttpResponse::BadRequest()
        .content_type("text/html; charset=utf-8")
        .body(body)
}

/// Minimal HTML-escape — enough to make the rendered error page safe
/// when the upstream message contains user input.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Validate the `__Host-zs_console_session` cookie and return either
/// the live session or an `Err(HttpResponse)` the caller must return
/// directly. On miss / expired:
///   - "wants HTML" (no `Accept` containing `application/json`,
///     `text/event-stream`, or `application/x-zship`) → 302 to
///     `/oauth2/auth` (kicks off the dance, original path stashed).
///   - everything else → 401 JSON envelope.
///
/// On success: returns the [`ConsoleSession`] with `idle_expires_at`
/// already slid forward by `console_sessions::validate`.
///
/// `state.insecure_dev = true` builds an `http://` redirect_uri;
/// production always uses `https://`. The redirect target is computed
/// from the request's `Host` header.
///
/// # Errors
///
/// Returns `Err(HttpResponse)` whenever the caller should send a
/// non-2xx response (missing cookie, validate miss, infrastructure not
/// configured). The error variant is always a fully-formed response —
/// never propagated up the stack as a real `Err`.
pub async fn require_console_session(
    req: &web::HttpRequest,
    state: &AppState,
) -> std::result::Result<crate::console_sessions::ConsoleSession, web::HttpResponse> {
    let cookie_header = req
        .headers()
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let Some(id) = crate::oidc_rp::parse_console_session_cookie(cookie_header, state.insecure_dev) else {
        return Err(reject_response(req, state));
    };

    let pg = &state.auth_pg;
    match crate::console_sessions::validate(pg, id).await {
        Ok(Some(session)) => Ok(session),
        Ok(None) => Err(reject_response(req, state)),
        Err(e) => {
            tracing::error!(error = %e, "control: console_sessions::validate failed");
            Err(web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({"error": "session validate failed"})))
        }
    }
}

/// 302 → hydra (HTML clients) or 401 JSON (API clients).
fn reject_response(req: &web::HttpRequest, state: &AppState) -> web::HttpResponse {
    if wants_html(req) {
        start_oidc_redirect(req, state)
    } else {
        reject_unauth_response()
    }
}

fn reject_unauth_response() -> web::HttpResponse {
    web::HttpResponse::Unauthorized()
        .json(&serde_json::json!({"error": "unauthenticated"}))
}

/// True when the request looks like an HTML/browser navigation.
/// Anything that prefers JSON/SSE/zship is an API call and gets a
/// 401, not a 302.
fn wants_html(req: &web::HttpRequest) -> bool {
    let accept = req
        .headers()
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    !accept.contains("application/json")
        && !accept.contains("text/event-stream")
        && !accept.contains("application/x-zship")
}

/// Build the 302 → hydra redirect that kicks off the OIDC dance.
/// Stash cookie carries the PKCE verifier + state + original_path so
/// the callback can resume.
fn start_oidc_redirect(req: &web::HttpRequest, state: &AppState) -> web::HttpResponse {
    let oidc_rp = &state.oidc_rp;
    let original_path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/")
        .to_string();
    let original_path = sanitize_oidc_original_path(&original_path);
    let host = req
        .headers()
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let scheme = if state.insecure_dev { "http" } else { "https" };
    let redirect_uri = format!("{scheme}://{host}/auth/callback");

    let (auth_url, stash) =
        oidc_rp.build_authorize_redirect(&original_path, &redirect_uri);

    let mut builder = web::HttpResponse::Found();
    builder.header("location", auth_url);
    builder.header(
        "set-cookie",
        crate::oidc_rp::set_console_stash_cookie(&stash, state.insecure_dev),
    );
    builder.finish()
}

fn sanitize_oidc_original_path(path: &str) -> String {
    if is_safe_oidc_original_path(path) {
        path.to_string()
    } else {
        "/".to_string()
    }
}

fn is_safe_oidc_original_path(path: &str) -> bool {
    if path == "/" {
        return true;
    }

    let bytes = path.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'/' {
        return false;
    }

    if !matches!(
        bytes[1],
        b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_'
    ) {
        return false;
    }

    let first_segment_end = path[1..]
        .find('/')
        .map(|idx| idx + 1)
        .unwrap_or(path.len());
    !path[1..first_segment_end].contains(':')
}

#[cfg(test)]
mod console_auth_tests {
    //! Standalone tests for the `require_console_session` helper.
    //!
    //! Constructing a full `AppState` here would require a live PG,
    //! so we exercise the cookie-parsing / `wants_html` / `reject`
    //! branches via the leaf helpers. The full integration is covered
    //! by `console_sessions_test.rs` which runs against a real PG.

    use super::*;
    use ntex::http::header::{self, HeaderValue};
    use ntex::http::Method;
    use ntex::web::test::TestRequest;

    #[test]
    fn wants_html_returns_true_for_browser_accept() {
        let req = TestRequest::default()
            .header(
                header::ACCEPT,
                HeaderValue::from_static(
                    "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
                ),
            )
            .method(Method::GET)
            .to_http_request();
        assert!(wants_html(&req));
    }

    #[test]
    fn wants_html_returns_false_for_json_accept() {
        let req = TestRequest::default()
            .header(
                header::ACCEPT,
                HeaderValue::from_static("application/json"),
            )
            .method(Method::GET)
            .to_http_request();
        assert!(!wants_html(&req));
    }

    #[test]
    fn wants_html_returns_false_for_sse_accept() {
        let req = TestRequest::default()
            .header(
                header::ACCEPT,
                HeaderValue::from_static("text/event-stream"),
            )
            .method(Method::GET)
            .to_http_request();
        assert!(!wants_html(&req));
    }

    #[test]
    fn wants_html_returns_true_for_missing_accept() {
        // Browsers always set Accept; missing-Accept is unusual but the
        // safe default is "treat as HTML" so curl users still get a
        // redirect they can follow.
        let req = TestRequest::default().method(Method::GET).to_http_request();
        assert!(wants_html(&req));
    }

    #[test]
    fn reject_unauth_response_is_401_json() {
        let resp = reject_unauth_response();
        assert_eq!(resp.status().as_u16(), 401);
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(ct.contains("application/json"), "want JSON, got {ct}");
    }

    #[test]
    fn oidc_original_path_rejects_protocol_relative_redirects() {
        assert_eq!(sanitize_oidc_original_path("//evil.com/path"), "/");
        assert_eq!(sanitize_oidc_original_path("/\\evil.com/path"), "/");
        assert_eq!(sanitize_oidc_original_path("/foo:bar/baz"), "/");
        assert_eq!(sanitize_oidc_original_path("https://evil.com/path"), "/");
    }

    #[test]
    fn oidc_original_path_keeps_origin_relative_paths() {
        assert_eq!(sanitize_oidc_original_path("/"), "/");
        assert_eq!(
            sanitize_oidc_original_path("/dashboard?welcome=true"),
            "/dashboard?welcome=true"
        );
        assert_eq!(
            sanitize_oidc_original_path("/_zs/auth/callback"),
            "/_zs/auth/callback"
        );
    }
}

// ---------------------------------------------------------------------------
// Streaming helper — used by `deploy()` to land the request body in a
// tmp file before mmap+ingest. Generic over the stream type so the
// helper is unit-testable with `futures::stream::iter`; production
// callers pass in `web::types::Payload` (which is `Stream<Item =
// Result<Bytes, PayloadError>>`).
// ---------------------------------------------------------------------------

/// Errors from `stream_body_to_tmp_file`. Maps cleanly onto HTTP
/// status codes — see `deploy()` for the response shape.
#[derive(Debug)]
pub(crate) enum StreamToTmpError {
    /// Couldn't open the tmp file for writing. Caller should return 500.
    OpenFailed(String),
    /// Body exceeded `max_bytes`. Tmp file has been removed.
    /// Caller should return 413.
    TooLarge { cap: u64, observed: u64 },
    /// Underlying payload error (client disconnected, decoding error,
    /// etc.). Tmp file has been removed. Caller should return 400.
    PayloadError(String),
    /// Disk write or sync failed. Tmp file has been removed (best
    /// effort). Caller should return 500.
    WriteFailed(String),
}

impl std::fmt::Display for StreamToTmpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OpenFailed(s) => write!(f, "tmp file open failed: {s}"),
            Self::TooLarge { cap, observed } => {
                write!(f, "body too large: {observed} > {cap}")
            }
            Self::PayloadError(s) => write!(f, "payload error: {s}"),
            Self::WriteFailed(s) => write!(f, "tmp write failed: {s}"),
        }
    }
}

/// Stream a body to a tmp file, enforcing `max_bytes` while writing.
/// On any error (incl. cap exceeded), the partial tmp file is removed.
/// On success, the file is fsynced and the total byte count returned.
///
/// Generic over the chunk type (`B: AsRef<[u8]>`) so this compiles
/// against both `ntex::util::Bytes` (production: `web::types::Payload`
/// yields ntex's bytes type) and stock `bytes::Bytes` (used by tests
/// constructing `futures::stream::iter`).
pub(crate) async fn stream_body_to_tmp_file<S, B, E>(
    stream: &mut S,
    tmp_path: &StdPath,
    max_bytes: u64,
) -> Result<u64, StreamToTmpError>
where
    S: Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    use compio::io::AsyncWriteAtExt;
    use futures::StreamExt;

    let file = compio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp_path)
        .await
        .map_err(|e| StreamToTmpError::OpenFailed(e.to_string()))?;

    let mut written: u64 = 0;
    while let Some(item) = stream.next().await {
        let chunk = match item {
            Ok(c) => c,
            Err(e) => {
                drop(file);
                let _ = compio::fs::remove_file(tmp_path).await;
                return Err(StreamToTmpError::PayloadError(e.to_string()));
            }
        };
        let chunk_slice: &[u8] = chunk.as_ref();
        let chunk_len = chunk_slice.len() as u64;
        let new_total = written + chunk_len;
        if new_total > max_bytes {
            drop(file);
            let _ = compio::fs::remove_file(tmp_path).await;
            return Err(StreamToTmpError::TooLarge {
                cap: max_bytes,
                observed: new_total,
            });
        }
        // compio File::write_all_at takes ownership of the buffer.
        // The chunk is a refcounted slice; copy into an owned Vec so
        // we can hand it to write_all_at. The to_vec() costs a single
        // chunk-sized alloc per chunk (typically 16-256 KiB).
        let owned: Vec<u8> = chunk_slice.to_vec();
        let compio::BufResult(res, _returned) =
            (&file).write_all_at(owned, written).await;
        if let Err(e) = res {
            drop(file);
            let _ = compio::fs::remove_file(tmp_path).await;
            return Err(StreamToTmpError::WriteFailed(format!(
                "write at offset {written}: {e}"
            )));
        }
        written = new_total;
    }
    if let Err(e) = file.sync_all().await {
        drop(file);
        let _ = compio::fs::remove_file(tmp_path).await;
        return Err(StreamToTmpError::WriteFailed(format!("sync_all: {e}")));
    }
    drop(file);
    Ok(written)
}

#[cfg(test)]
mod stream_tmp_tests {
    use super::*;
    use bytes::Bytes;
    use futures::stream;

    fn temp_path(label: &str) -> std::path::PathBuf {
        let unique = uuid::Uuid::new_v4().simple().to_string();
        std::env::temp_dir().join(format!("zs-stream-test-{label}-{unique}"))
    }

    #[compio::test]
    async fn happy_path_writes_concatenated_bytes() {
        let path = temp_path("happy");
        let chunks: Vec<Result<Bytes, &str>> = vec![
            Ok(Bytes::from_static(b"hello, ")),
            Ok(Bytes::from_static(b"streaming ")),
            Ok(Bytes::from_static(b"world!")),
        ];
        let mut s = stream::iter(chunks);
        let written = stream_body_to_tmp_file(&mut s, &path, 1024)
            .await
            .expect("stream ok");
        assert_eq!(written, b"hello, streaming world!".len() as u64);
        let contents = std::fs::read(&path).unwrap();
        assert_eq!(contents, b"hello, streaming world!");
        let _ = std::fs::remove_file(&path);
    }

    #[compio::test]
    async fn cap_exceeded_removes_tmp_file() {
        let path = temp_path("cap");
        let chunks: Vec<Result<Bytes, &str>> = vec![
            Ok(Bytes::from_static(b"AAAAAAAAAA")), // 10 bytes
            Ok(Bytes::from_static(b"BBBBBBBBBB")), // would push to 20, > 15
        ];
        let mut s = stream::iter(chunks);
        let err = stream_body_to_tmp_file(&mut s, &path, 15)
            .await
            .unwrap_err();
        match err {
            StreamToTmpError::TooLarge { cap: 15, observed: 20 } => {}
            other => panic!("expected TooLarge, got {other:?}"),
        }
        assert!(!path.exists(), "tmp file should be removed on cap-exceeded");
    }

    #[compio::test]
    async fn stream_error_removes_tmp_file() {
        let path = temp_path("err");
        let chunks: Vec<Result<Bytes, &str>> = vec![
            Ok(Bytes::from_static(b"some bytes")),
            Err("network blew up"),
        ];
        let mut s = stream::iter(chunks);
        let err = stream_body_to_tmp_file(&mut s, &path, 1024)
            .await
            .unwrap_err();
        match err {
            StreamToTmpError::PayloadError(detail) => {
                assert!(detail.contains("network blew up"), "got {detail}");
            }
            other => panic!("expected PayloadError, got {other:?}"),
        }
        assert!(!path.exists(), "tmp file should be removed on payload error");
    }

    #[compio::test]
    async fn empty_stream_writes_zero_bytes() {
        let path = temp_path("empty");
        let chunks: Vec<Result<Bytes, &str>> = vec![];
        let mut s = stream::iter(chunks);
        let written = stream_body_to_tmp_file(&mut s, &path, 1024)
            .await
            .expect("stream ok");
        assert_eq!(written, 0);
        // Empty file should exist (we created it before the loop).
        assert!(path.exists(), "tmp file should exist even when empty");
        let contents = std::fs::read(&path).unwrap();
        assert!(contents.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[compio::test]
    async fn create_new_fails_when_path_exists() {
        let path = temp_path("collide");
        std::fs::write(&path, b"pre-existing").unwrap();
        let chunks: Vec<Result<Bytes, &str>> = vec![Ok(Bytes::from_static(b"x"))];
        let mut s = stream::iter(chunks);
        let err = stream_body_to_tmp_file(&mut s, &path, 1024)
            .await
            .unwrap_err();
        match err {
            StreamToTmpError::OpenFailed(_) => {}
            other => panic!("expected OpenFailed, got {other:?}"),
        }
        // Pre-existing file must not be overwritten.
        let contents = std::fs::read(&path).unwrap();
        assert_eq!(contents, b"pre-existing");
        let _ = std::fs::remove_file(&path);
    }
}
