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
use crate::exec;
use crate::files::Workspace;
use crate::sig::{AuthFail, Verifier};
use crate::version;

/// State shared by every handler. Cheap to clone (`Arc` inside).
#[derive(Clone, Debug)]
pub struct AppState {
    pub verifier: Arc<Verifier>,
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

/// HMAC verification for an auth-gated request. Reads the three
/// `X-Sbx-*` headers, recomputes the canonical-string HMAC over the
/// request method, path, timestamp, nonce, and **body**, and rejects
/// anything that doesn't match in constant time.
///
/// On any failure path, an audit event tagged with the specific
/// failure reason is emitted (so alerting can distinguish a
/// clock-skew operator mistake from an actual replay attack).
/// Returns true iff every check passes.
///
/// **Query strings are rejected outright.** The signed canonical
/// string covers `req.path()`, which excludes the query. A signed
/// `/foo` would otherwise also be valid for `/foo?evil=1` — and a
/// future handler that reads query params would silently accept the
/// attacker-controlled bit. We refuse query strings until we
/// explicitly extend the canonical to include them and bump
/// `PROTOCOL_VERSION`.
fn verify_signed(req: &HttpRequest, body: &[u8], state: &AppState) -> bool {
    let method = req.method().as_str();
    let path = req.path();

    if req.uri().query().is_some() {
        audit::record(
            audit::events::AUTH_FAIL,
            &format!("method={method} path={path} reason=query-not-allowed"),
        );
        return false;
    }

    let h = req.headers();
    let ts_hdr = h
        .get("x-sbx-timestamp")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let nonce_hdr = h
        .get("x-sbx-nonce")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let sig_hdr = h
        .get("x-sbx-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    match state
        .verifier
        .verify(method, path, body, ts_hdr, nonce_hdr, sig_hdr)
    {
        Ok(()) => true,
        Err(reason) => {
            let event = match reason {
                AuthFail::SkewTooLarge => audit::events::AUTH_SKEW,
                AuthFail::ReplayedNonce => audit::events::AUTH_REPLAY,
                _ => audit::events::AUTH_FAIL,
            };
            audit::record(
                event,
                &format!(
                    "method={method} path={path} reason={}",
                    reason.as_str()
                ),
            );
            false
        }
    }
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
    if !verify_signed(&req, &[], &state) { return unauthorized(); }
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
    body: Bytes,
) -> HttpResponse {
    // Verify the signature BEFORE deserializing — body bytes are
    // covered by the canonical hash, so any tampered payload fails
    // the HMAC check before serde_json sees it.
    if !verify_signed(&req, &body, &state) { return unauthorized(); }

    let parsed: ExecBody = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => return err(400, format!("invalid JSON body: {e}")),
    };

    let cwd = parsed
        .cwd
        .as_deref()
        .unwrap_or_else(|| state.workspace.path().to_str().unwrap_or("/workspace"));
    let timeout = parsed.timeout_ms.unwrap_or(exec::DEFAULT_TIMEOUT_MS);
    match exec::run(&parsed.cmd, cwd, timeout).await {
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
    if !verify_signed(&req, &[], &state) { return unauthorized(); }

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
    if !verify_signed(&req, &[], &state) { return unauthorized(); }

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
    if !verify_signed(&req, &body, &state) { return unauthorized(); }

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
    if !verify_signed(&req, &[], &state) { return unauthorized(); }

    let p = path.into_inner();
    match state.workspace.delete_file(&p) {
        Ok(true) => HttpResponse::Ok().json(&json!({"deleted": p})),
        Ok(false) => err(404, format!("delete {p}: No such file or directory")),
        Err(e) => fs_error_response("delete", &p, e, None),
    }
}

#[cfg(test)]
mod tests {
    //! HTTP-level handler tests using `ntex::web::test`. Each test
    //! builds a fresh `AppState` against a unique temp workspace and
    //! a freshly-mounted token, then issues requests through the
    //! in-memory test transport.

    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering as AtomOrd};
    use ntex::http::StatusCode;
    use ntex::web::test;

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);
    const TEST_TOKEN: &str = "test-token-must-be-at-least-32-chars-long-32";

    /// Serializes tests that mutate the global `reap::REAPER_HEALTHY`
    /// flag. Shared with `reap::tests` so a reap test and a handler
    /// test never race the flag concurrently.
    use crate::reap::TEST_FLAG_LOCK as REAPER_STATE_LOCK;

    fn unique_dir(label: &str) -> std::path::PathBuf {
        let n = TEST_COUNTER.fetch_add(1, AtomOrd::SeqCst);
        let pid = std::process::id();
        let p = std::env::temp_dir().join(format!("zsbx-htest-{label}-{pid}-{n}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn make_state(label: &str) -> (AppState, std::path::PathBuf) {
        let dir = unique_dir(label);
        let token_path = dir.join("token");
        let mut f = std::fs::File::create(&token_path).unwrap();
        f.write_all(TEST_TOKEN.as_bytes()).unwrap();
        let key = crate::auth::load_key_from_path(&token_path).unwrap();
        let verifier = crate::sig::Verifier::new(key);
        let workspace = crate::files::Workspace::open(&dir.join("ws")).unwrap();
        let state = AppState {
            verifier: Arc::new(verifier),
            workspace: Arc::new(workspace),
            draining: Arc::new(AtomicBool::new(false)),
            started_at_unix: 1234,
        };
        (state, dir)
    }

    /// Sign a request with the test key. Returns the three header
    /// values `(timestamp, nonce, signature)` to attach.
    fn sign(method: &str, path: &str, body: &[u8]) -> (String, String, String) {
        use std::sync::atomic::AtomicU64;
        static NONCE_COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = NONCE_COUNTER.fetch_add(1, AtomOrd::SeqCst);
        let nonce = format!("test-nonce-{}-{}", std::process::id(), n);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let sig = crate::sig::sign_for_test(
            TEST_TOKEN.as_bytes(),
            method,
            path,
            body,
            ts,
            &nonce,
        );
        (ts.to_string(), nonce, sig)
    }

    /// Build the test app inline. Macro keeps each test a one-liner
    /// but avoids the typed-helper signature that depends on
    /// ntex private types.
    macro_rules! make_app {
        ($state:expr) => {
            test::init_service(
                ntex::web::App::new()
                    .state($state)
                    .service(web::resource("/livez").route(web::get().to(livez)))
                    .service(web::resource("/readyz").route(web::get().to(readyz)))
                    .service(web::resource("/version").route(web::get().to(version_info)))
                    .service(web::resource("/healthz").route(web::get().to(livez)))
                    .service(web::resource("/exec").route(web::post().to(exec_cmd)))
                    .service(web::resource("/tree").route(web::get().to(file_tree)))
                    .service(web::resource("/shutdown").route(web::post().to(shutdown)))
                    .service(
                        web::resource("/files/{path}*")
                            .route(web::get().to(read_file))
                            .route(web::put().to(write_file))
                            .route(web::delete().to(delete_file)),
                    ),
            )
            .await
        };
    }

    /// Parse `WebResponse` body as JSON. ntex 3 doesn't ship a
    /// `read_body_json` helper, so we wrap `read_body` + serde_json.
    async fn body_json(resp: ntex::web::WebResponse) -> serde_json::Value {
        let bytes = test::read_body(resp).await;
        serde_json::from_slice(&bytes).expect("response body is valid JSON")
    }

    /// Build a TestRequest with HMAC-signed `X-Sbx-*` headers for an
    /// auth-gated endpoint that takes no body.
    fn signed(method: &str, path: &str) -> test::TestRequest {
        signed_with_body(method, path, "")
    }

    /// Build a TestRequest with HMAC-signed `X-Sbx-*` headers and an
    /// attached payload. The body bytes are baked into the signature
    /// canonical string, so altering the payload after this call
    /// will break verification (which is the whole point).
    fn signed_with_body(method: &str, path: &str, body: &str) -> test::TestRequest {
        let (ts, nonce, sig) = sign(method, path, body.as_bytes());
        let r = match method {
            "GET" => test::TestRequest::get(),
            "POST" => test::TestRequest::post(),
            "PUT" => test::TestRequest::put(),
            "DELETE" => test::TestRequest::delete(),
            other => panic!("unsupported method: {other}"),
        };
        let r = r
            .uri(path)
            .header("x-sbx-timestamp", ts)
            .header("x-sbx-nonce", nonce)
            .header("x-sbx-signature", sig);
        if body.is_empty() { r } else { r.set_payload(body.to_string()) }
    }

    // ─── Probes / version (unauthenticated) ────────────────────

    #[ntex::test]
    async fn livez_returns_200() {
        let (state, _d) = make_state("livez");
        let app = make_app!(state);
        let req = test::TestRequest::get().uri("/livez").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[ntex::test]
    async fn healthz_aliases_livez() {
        let (state, _d) = make_state("hz");
        let app = make_app!(state);
        let req = test::TestRequest::get().uri("/healthz").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[ntex::test]
    async fn readyz_ready_when_not_draining() {
        let _lock = REAPER_STATE_LOCK.lock().unwrap();
        crate::reap::test_set_healthy(true);
        let (state, _d) = make_state("ready");
        let app = make_app!(state);
        let req = test::TestRequest::get().uri("/readyz").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[ntex::test]
    async fn readyz_503_when_draining() {
        let _lock = REAPER_STATE_LOCK.lock().unwrap();
        crate::reap::test_set_healthy(true);
        let (state, _d) = make_state("draining");
        state.mark_draining();
        let app = make_app!(state);
        let req = test::TestRequest::get().uri("/readyz").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[ntex::test]
    async fn readyz_503_when_reaper_down() {
        let _lock = REAPER_STATE_LOCK.lock().unwrap();
        crate::reap::test_set_healthy(false);
        let (state, _d) = make_state("noreaper");
        let app = make_app!(state);
        let req = test::TestRequest::get().uri("/readyz").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        crate::reap::test_set_healthy(true); // restore for other tests
    }

    #[ntex::test]
    async fn version_info_has_required_fields() {
        let (state, _d) = make_state("ver");
        let app = make_app!(state);
        let req = test::TestRequest::get().uri("/version").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert!(body["agent_version"].is_string());
        assert!(body["git_commit"].is_string());
        assert_eq!(body["protocol_version"], 1);
        let caps = body["capabilities"].as_array().unwrap();
        let cap_strs: Vec<&str> = caps.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(cap_strs.contains(&"auth.hmac-v1"));
        assert_eq!(body["started_at_unix"], 1234);
    }

    // ─── Auth: 401 on missing / replayed / tampered ───────────

    #[ntex::test]
    async fn auth_gated_endpoint_without_signature_returns_401() {
        let (state, _d) = make_state("noauth");
        let app = make_app!(state);
        let req = test::TestRequest::post()
            .uri("/exec")
            .set_payload(r#"{"cmd": "echo hi"}"#)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[ntex::test]
    async fn auth_gated_endpoint_with_bad_signature_returns_401() {
        let (state, _d) = make_state("bad_sig");
        let app = make_app!(state);
        let body = r#"{"cmd": "echo hi"}"#;
        // Sign for the WRONG path — server recomputes for the actual
        // path, mismatch, 401.
        let (ts, nonce, sig) = sign("POST", "/somewhere-else", body.as_bytes());
        let req = test::TestRequest::post()
            .uri("/exec")
            .header("x-sbx-timestamp", ts)
            .header("x-sbx-nonce", nonce)
            .header("x-sbx-signature", sig)
            .set_payload(body)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[ntex::test]
    async fn replayed_request_rejected_second_time() {
        // First call: sign + send → OK. Second call: same nonce →
        // 401 ReplayedNonce. The Verifier's LRU is per-AppState, so
        // we share state across two requests.
        let (state, _d) = make_state("replay");
        let app = make_app!(state);
        let body = r#"{"cmd": "echo hi"}"#;
        let (ts, nonce, sig) = sign("POST", "/exec", body.as_bytes());
        let mk = || {
            test::TestRequest::post()
                .uri("/exec")
                .header("x-sbx-timestamp", ts.clone())
                .header("x-sbx-nonce", nonce.clone())
                .header("x-sbx-signature", sig.clone())
                .set_payload(body.to_string())
                .to_request()
        };
        let r1 = test::call_service(&app, mk()).await;
        assert_eq!(r1.status(), StatusCode::OK);
        let r2 = test::call_service(&app, mk()).await;
        assert_eq!(r2.status(), StatusCode::UNAUTHORIZED);
    }

    #[ntex::test]
    async fn body_tamper_after_signing_rejected() {
        // Sign for body A, send body B → HMAC mismatch → 401.
        let (state, _d) = make_state("tamper");
        let app = make_app!(state);
        let body_signed = r#"{"cmd": "echo expected"}"#;
        let body_sent = r#"{"cmd": "rm -rf /"}"#;
        let (ts, nonce, sig) = sign("POST", "/exec", body_signed.as_bytes());
        let req = test::TestRequest::post()
            .uri("/exec")
            .header("x-sbx-timestamp", ts)
            .header("x-sbx-nonce", nonce)
            .header("x-sbx-signature", sig)
            .set_payload(body_sent)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[ntex::test]
    async fn query_string_rejected_even_if_signed() {
        // A signed request whose URI carries a `?query=...` is
        // refused — the canonical only covers `path()`, so the
        // query is unauthenticated. Forward-compat tripwire.
        let (state, _d) = make_state("query");
        let app = make_app!(state);
        // Sign for the bare path "/tree" (no query). Then send
        // the request to "/tree?evil=1" — our verify_signed sees
        // a query and refuses regardless of the otherwise-valid sig.
        let (ts, nonce, sig) = sign("GET", "/tree", &[]);
        let req = test::TestRequest::get()
            .uri("/tree?evil=1")
            .header("x-sbx-timestamp", ts)
            .header("x-sbx-nonce", nonce)
            .header("x-sbx-signature", sig)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[ntex::test]
    async fn skewed_timestamp_rejected() {
        let (state, _d) = make_state("skew");
        let app = make_app!(state);
        // Sign with a timestamp 60 s in the past → outside SKEW_S=5.
        use std::time::{SystemTime, UNIX_EPOCH};
        let ts_old = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 60;
        let nonce = "skew-nonce";
        let body = r#"{"cmd": "echo hi"}"#;
        let sig = crate::sig::sign_for_test(
            TEST_TOKEN.as_bytes(),
            "POST",
            "/exec",
            body.as_bytes(),
            ts_old,
            nonce,
        );
        let req = test::TestRequest::post()
            .uri("/exec")
            .header("x-sbx-timestamp", ts_old.to_string())
            .header("x-sbx-nonce", nonce)
            .header("x-sbx-signature", sig)
            .set_payload(body)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // ─── /exec ────────────────────────────────────────────────

    #[ntex::test]
    async fn exec_runs_command_and_returns_output() {
        let (state, _d) = make_state("exec_ok");
        let app = make_app!(state);
        let body = r#"{"cmd": "echo agent_test"}"#;
        let req = signed_with_body("POST", "/exec", body).to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["status"], 0);
        assert_eq!(body["stdout"].as_str().unwrap().trim(), "agent_test");
        assert_eq!(body["timed_out"], false);
    }

    #[ntex::test]
    async fn exec_propagates_nonzero_exit() {
        let (state, _d) = make_state("exec_exit");
        let app = make_app!(state);
        let body = r#"{"cmd": "exit 42"}"#;
        let req = signed_with_body("POST", "/exec", body).to_request();
        let resp = test::call_service(&app, req).await;
        let body = body_json(resp).await;
        assert_eq!(body["status"], 42);
    }

    #[ntex::test]
    async fn exec_invalid_json_returns_400() {
        let (state, _d) = make_state("exec_badjson");
        let app = make_app!(state);
        let body = r#"not json at all"#;
        let req = signed_with_body("POST", "/exec", body).to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // ─── /files write/read/tree/delete ────────────────────────

    #[ntex::test]
    async fn write_then_read_roundtrip() {
        let (state, _d) = make_state("rw");
        let app = make_app!(state);

        let req = signed_with_body("PUT", "/files/note.txt", "hello world").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let req = signed("GET", "/files/note.txt").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = test::read_body(resp).await;
        assert_eq!(&body[..], b"hello world");
    }

    #[ntex::test]
    async fn read_missing_returns_404() {
        let (state, _d) = make_state("read_404");
        let app = make_app!(state);
        let req = signed("GET", "/files/missing.txt").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[ntex::test]
    async fn write_to_path_with_parent_dir_creates_parents() {
        let (state, dir) = make_state("nested");
        let app = make_app!(state);
        let req = signed_with_body("PUT", "/files/src/lib/index.ts", "export {}").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(dir.join("ws/src/lib/index.ts").exists());
    }

    #[ntex::test]
    async fn delete_existing_returns_200() {
        let (state, _d) = make_state("del_ok");
        state.workspace.write_file("a.txt", b"x").unwrap();
        let app = make_app!(state);
        let req = signed("DELETE", "/files/a.txt").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[ntex::test]
    async fn delete_missing_returns_404_with_path() {
        let (state, _d) = make_state("del_404");
        let app = make_app!(state);
        let req = signed("DELETE", "/files/no-such").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = body_json(resp).await;
        assert!(body["error"].as_str().unwrap().contains("no-such"));
    }

    #[ntex::test]
    async fn tree_lists_workspace() {
        let (state, _d) = make_state("tree");
        state.workspace.write_file("a.txt", b"a").unwrap();
        state.workspace.write_file("nested/b.txt", b"b").unwrap();
        let app = make_app!(state);
        let req = signed("GET", "/tree").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["truncated"], false);
        let entries = body["entries"].as_array().unwrap();
        let paths: Vec<&str> = entries.iter().map(|e| e["path"].as_str().unwrap()).collect();
        assert!(paths.contains(&"a.txt"));
        assert!(paths.contains(&"nested/b.txt"));
        for e in entries {
            assert!(e["mtime_unix"].is_number());
        }
    }

    // ─── /shutdown ────────────────────────────────────────────

    #[ntex::test]
    async fn shutdown_flips_drain_flag() {
        let _lock = REAPER_STATE_LOCK.lock().unwrap();
        crate::reap::test_set_healthy(true);
        let (state, _d) = make_state("shut");
        assert!(!state.is_draining());
        let app = make_app!(state.clone());

        let req = signed("POST", "/shutdown").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(state.is_draining(), "shutdown must flip the drain flag");

        let req = test::TestRequest::get().uri("/readyz").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[ntex::test]
    async fn shutdown_requires_auth() {
        let (state, _d) = make_state("shut_noauth");
        let app = make_app!(state);
        let req = test::TestRequest::post().uri("/shutdown").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // ─── Symlink-escape responses (403 + audit signal) ────────

    #[ntex::test]
    async fn symlink_leaf_returns_403() {
        let (state, dir) = make_state("symleaf");
        std::os::unix::fs::symlink("/etc/passwd", dir.join("ws/escape")).unwrap();
        let app = make_app!(state);
        let req = signed("GET", "/files/escape").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[ntex::test]
    async fn deep_path_returns_400() {
        let (state, _d) = make_state("deep");
        let app = make_app!(state);
        // 33 components > MAX_PATH_COMPONENTS (32)
        let mut deep = String::from("/files/");
        for _ in 0..33 {
            deep.push_str("a/");
        }
        deep.push_str("file.txt");
        let req = signed_with_body("PUT", &deep, "x").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // ─── content_type unit tests ───────────────────────────────

    #[test]
    fn content_type_known_extensions() {
        assert_eq!(content_type("file.html"), "text/html; charset=utf-8");
        assert_eq!(content_type("file.css"), "text/css; charset=utf-8");
        assert_eq!(content_type("file.js"), "application/javascript; charset=utf-8");
        assert_eq!(content_type("file.tsx"), "application/javascript; charset=utf-8");
        assert_eq!(content_type("file.json"), "application/json; charset=utf-8");
        assert_eq!(content_type("file.md"), "text/plain; charset=utf-8");
        assert_eq!(content_type("file.svg"), "image/svg+xml");
        assert_eq!(content_type("file.png"), "image/png");
        assert_eq!(content_type("file.jpg"), "image/jpeg");
        assert_eq!(content_type("file.jpeg"), "image/jpeg");
    }

    #[test]
    fn content_type_unknown_falls_back_to_octet_stream() {
        assert_eq!(content_type("file.xyz"), "application/octet-stream");
        assert_eq!(content_type("noext"), "application/octet-stream");
        assert_eq!(content_type(""), "application/octet-stream");
    }

    #[test]
    fn content_type_is_case_insensitive() {
        assert_eq!(content_type("file.HTML"), "text/html; charset=utf-8");
        assert_eq!(content_type("file.PNG"), "image/png");
    }

    // ─── fs_error_response unit tests ──────────────────────────

    #[test]
    fn fs_error_response_symlink_yields_403() {
        let r = fs_error_response("read", "x", "read x: refusing to follow symlink".into(), None);
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn fs_error_response_escape_yields_403() {
        let r = fs_error_response("read", "x", "read x: path escapes workspace".into(), None);
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn fs_error_response_too_large_yields_400() {
        let r = fs_error_response("write", "big", "file too large: 100".into(), Some(100));
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn fs_error_response_not_found_yields_404() {
        let r = fs_error_response("read", "x", "read x: No such file or directory".into(), None);
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn fs_error_response_other_yields_400() {
        let r = fs_error_response("read", "x", "read x: some other I/O error".into(), None);
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    }
}
