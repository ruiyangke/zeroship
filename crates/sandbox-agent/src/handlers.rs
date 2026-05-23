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
use crate::sig::{self, AuthFail, CanonicalKind, Verifier};
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

/// 503 Service Unavailable — used for auth-gated endpoints once
/// `/shutdown` flips the drain flag. We return this BEFORE doing
/// any expensive work so a slow controller retry-loop can't pile
/// up new long-running execs while ntex's shutdown timeout
/// approaches.
fn draining() -> HttpResponse {
    HttpResponse::ServiceUnavailable().json(&json!({"error": "draining"}))
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

/// Pick the canonical-string version for a request based on the path.
///
/// **Round-6 § II.1 CRITICAL-4 dispatcher byte-equality invariant.**
/// The byte string we test (`req.path()`) is identical to the byte
/// string ntex's route matcher uses to dispatch the handler, with NO
/// normalization between them. A request whose URL is
/// `/proxy/5173/%2e%2e%2fexec` therefore lands on the proxy handler
/// AND on the v1.1 chooser; ntex never decodes the percent-escapes
/// before route matching, so the dispatcher and matcher agree.
///
/// Paths under `/proxy/` use [`CanonicalKind::V1_1`] (queries are
/// covered, with a domain-separator tag). Everything else stays on
/// [`CanonicalKind::V1`] (legacy `/exec`, `/files`, `/tree`, etc.).
pub(crate) fn canonical_kind_for(path: &str) -> CanonicalKind {
    if path.starts_with("/proxy/") {
        CanonicalKind::V1_1
    } else {
        CanonicalKind::V1
    }
}

/// HMAC verification for an auth-gated request. Reads the three
/// `X-Sbx-*` headers, recomputes the canonical-string signature over
/// the request method, path (+ query, for v1.1), timestamp, nonce,
/// and **body**, and rejects anything that doesn't match.
///
/// On any failure path, an audit event tagged with the specific
/// failure reason is emitted (so alerting can distinguish a
/// clock-skew operator mistake from an actual replay attack).
/// Returns true iff every check passes.
///
/// ## Canonical-version dispatch
///
/// Paths starting with `/proxy/` are verified under
/// [`CanonicalKind::V1_1`] — the canonical includes the URL query
/// and a `ED25519-V1.1` domain-separator tag. Every other path is
/// verified under [`CanonicalKind::V1`]; for those, query strings are
/// REJECTED outright (the same defense the original handler
/// implemented).
///
/// ## Why the path-prefix dispatch is byte-equal with the route matcher
///
/// `req.path()` here is the same byte slice ntex used to choose this
/// handler. We do not normalize, decode, or collapse — same input,
/// same dispatch. See [`canonical_kind_for`] for the full invariant.
fn verify_signed(req: &HttpRequest, body: &[u8], state: &AppState) -> bool {
    let method = req.method().as_str();
    let path = req.path();
    let kind = canonical_kind_for(path);

    // For v1: query strings are not in the canonical; refuse outright.
    // For v1.1: the helper folds the query into the canonical bytes.
    let path_query: String = match kind {
        CanonicalKind::V1 => {
            if req.uri().query().is_some() {
                audit::record(
                    audit::events::AUTH_FAIL,
                    &format!("method={method} path={path} reason=query-not-allowed"),
                );
                crate::metrics::inc_auth_fail("query-not-allowed");
                return false;
            }
            path.to_string()
        }
        CanonicalKind::V1_1 | CanonicalKind::V1_1_Ws => {
            // Reconstruct path-and-query from the request URI's
            // path_and_query() so the bytes match what the controller
            // signed. ntex's `uri()` returns a `http::Uri`; its
            // `path_and_query()` gives "/path?query" (no fragment).
            //
            // V1_1 and V1_1_Ws share the same path-query rules; they
            // differ only in the leading domain-separator tag inside
            // `build_canonical`. The dispatcher (canonical_kind_for)
            // chooses between them based on the request shape — the
            // ntex HTTP path always picks V1_1; the compio raw-TCP
            // WS handler (`proxy_ws.rs`) picks V1_1_Ws.
            match req.uri().path_and_query() {
                Some(pq) => sig::v1_1_path_query(pq.as_str()).to_string(),
                None => path.to_string(),
            }
        }
    };

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

    match state.verifier.verify_kind(
        kind,
        method,
        &path_query,
        body,
        ts_hdr,
        nonce_hdr,
        sig_hdr,
    ) {
        Ok(()) => true,
        Err(reason) => {
            let event = match reason {
                AuthFail::SkewTooLarge => audit::events::AUTH_SKEW,
                AuthFail::ReplayedNonce => audit::events::AUTH_REPLAY,
                _ => audit::events::AUTH_FAIL,
            };
            let r = reason.as_str();
            audit::record(
                event,
                &format!("method={method} path={path} reason={r}"),
            );
            crate::metrics::inc_auth_fail(r);
            false
        }
    }
}

/// Re-export of [`verify_signed`] for the proxy module, which lives
/// in a sibling file but needs to call the same auth machinery.
/// Kept module-private (the function is already tested via the
/// existing handler tests; the proxy module re-uses it 1:1).
pub(crate) fn verify_signed_pub(req: &HttpRequest, body: &[u8], state: &AppState) -> bool {
    verify_signed(req, body, state)
}

/// Bug #22 fix: variant of [`verify_signed`] that uses
/// [`sig::Verifier::verify_kind_skew_bypass`] so a controller-signed
/// request whose timestamp is far from the agent's frozen-at-snapshot
/// `CLOCK_REALTIME` still validates. ONLY used by the
/// [`clock_resync`] handler — every other auth-gated endpoint must
/// keep using the strict-skew [`verify_signed`]. Returns true iff
/// every check except wall-clock skew passes.
fn verify_signed_skew_bypass(req: &HttpRequest, body: &[u8], state: &AppState) -> bool {
    let method = req.method().as_str();
    let path = req.path();
    // /_clock_resync is a v1-canonical endpoint: it's NOT under
    // `/proxy/`, never carries a query string, and `canonical_kind_for`
    // would dispatch it as V1 anyway. Spell out V1 here so future
    // dispatcher edits can't accidentally upgrade the resync surface.
    let kind = CanonicalKind::V1;
    if req.uri().query().is_some() {
        audit::record(
            audit::events::AUTH_FAIL,
            &format!("method={method} path={path} reason=query-not-allowed-resync"),
        );
        crate::metrics::inc_auth_fail("query-not-allowed");
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
    match state.verifier.verify_kind_skew_bypass(
        kind,
        method,
        path,
        body,
        ts_hdr,
        nonce_hdr,
        sig_hdr,
    ) {
        Ok(()) => true,
        Err(reason) => {
            let event = match reason {
                AuthFail::ReplayedNonce => audit::events::AUTH_REPLAY,
                _ => audit::events::AUTH_FAIL,
            };
            let r = reason.as_str();
            audit::record(
                event,
                &format!("method={method} path={path} reason={r}-resync"),
            );
            crate::metrics::inc_auth_fail(r);
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

/// `GET /metrics` — Prometheus text exposition. **Unauthenticated**;
/// see the metrics module-level docs for the rationale.
pub async fn metrics(state: State) -> HttpResponse {
    let body = crate::metrics::render(state.started_at_unix);
    HttpResponse::Ok()
        .content_type("text/plain; version=0.0.4; charset=utf-8")
        .body(body)
}

/// `GET /version` — agent version + protocol + capabilities.
/// **Auth-gated.** Previous versions were unauthenticated, which
/// leaked `git_commit` (CVE-matchable build identifier),
/// `capabilities` (API surface enumeration), and
/// `started_at_unix` (process-age correlation) to anything that
/// reached :7777. Cluster NetworkPolicy is the primary gate, but
/// adding the agent-level Ed25519 check is free and closes the
/// information-disclosure surface as defense-in-depth.
///
/// The controller calls this once per session and has the key, so
/// the user-facing impact is zero. `pubkey_fingerprint` is also
/// included now so operators can confirm the agent is verifying
/// with the expected trust anchor.
pub async fn version_info(req: HttpRequest, state: State) -> HttpResponse {
    if !verify_signed(&req, &[], &state) { return unauthorized(); }
    HttpResponse::Ok().json(&json!({
        "agent_version": version::AGENT_VERSION,
        "git_commit": version::GIT_COMMIT,
        "protocol_version": version::PROTOCOL_VERSION,
        "capabilities": version::CAPABILITIES,
        "started_at_unix": state.started_at_unix,
        "pubkey_fingerprint": crate::sig::pubkey_fingerprint(state.verifier.pubkey()),
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
    // Reject new commands while the agent is draining. Existing
    // in-flight execs continue; new ones would only race ntex's
    // shutdown timeout and leave the workspace in a half-state.
    if state.is_draining() { return draining(); }

    crate::metrics::inc_exec_request();

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
                crate::metrics::inc_exec_timeout();
            }
            if out.status != 0 {
                crate::metrics::inc_exec_nonzero();
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
    // Read paths drain too: a 50,000-entry tree walk eats real CPU
    // and serialization budget; refusing during shutdown lets ntex's
    // 30s drain window actually drain.
    if state.is_draining() { return draining(); }

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
    if state.is_draining() { return draining(); }

    let p = path.into_inner();
    match state.workspace.read_file(&p) {
        Ok(bytes) => {
            crate::metrics::add_files_bytes_read(bytes.len() as u64);
            HttpResponse::Ok()
                .content_type(content_type(&p))
                .body(bytes)
        }
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
    if state.is_draining() { return draining(); }

    let p = path.into_inner();
    let n = body.len();
    match state.workspace.write_file(&p, &body) {
        Ok(()) => {
            crate::metrics::add_files_bytes_written(n as u64);
            HttpResponse::Ok().json(&json!({"written": p, "size": n}))
        }
        Err(e) => fs_error_response("write", &p, e, Some(n)),
    }
}

pub async fn delete_file(
    req: HttpRequest,
    state: State,
    path: web::types::Path<String>,
) -> HttpResponse {
    if !verify_signed(&req, &[], &state) { return unauthorized(); }
    if state.is_draining() { return draining(); }

    let p = path.into_inner();
    match state.workspace.delete_file(&p) {
        Ok(true) => HttpResponse::Ok().json(&json!({"deleted": p})),
        Ok(false) => err(404, format!("delete {p}: No such file or directory")),
        Err(e) => fs_error_response("delete", &p, e, None),
    }
}

// ─── /_clock_resync — bug #22: post-CH-restore wall-clock fixup ──
//
// The controller calls this once, immediately after `wait_for_livez`
// returns Ok during a snapshot wake. CH `--restore` brings the VM
// back with `CLOCK_REALTIME` frozen at the snapshot-time value, so
// every subsequent strict-skew auth-gated endpoint would 401 until
// something resynchronises the guest's wall clock.
//
// The handler:
//   1. Verifies the request signature WITHOUT applying the 5-second
//      skew window (`verify_signed_skew_bypass`).
//   2. Parses the body `{"ts": <unix_secs>}`.
//   3. Calls `settimeofday(2)` to set `CLOCK_REALTIME` to the
//      controller's signed ts.
//   4. Returns 200.
//
// Security: the signature requires the controller's private key, so
// an in-VM attacker cannot push the clock. The nonce LRU prevents
// replay. The endpoint is the ONLY path that bypasses the skew gate;
// every other endpoint uses `verify_signed`.
//
// Idempotency: a re-call with a fresh ts/nonce simply re-sets the
// clock; no state in the agent depends on "we already resynced" beyond
// the LRU's per-nonce uniqueness. The controller can retry on transient
// network errors without harm.

#[derive(Debug, Deserialize)]
pub struct ClockResyncBody {
    /// Unix seconds the controller wants the guest's `CLOCK_REALTIME`
    /// set to. The canonical body-hash binds this number to the
    /// signature, so an attacker can't substitute a different value.
    pub ts: u64,
}

pub async fn clock_resync(
    req: HttpRequest,
    state: State,
    body: Bytes,
) -> HttpResponse {
    // Verify with the skew-bypass surface BEFORE deserializing —
    // tampered payload would fail the body-hash gate.
    if !verify_signed_skew_bypass(&req, &body, &state) {
        return unauthorized();
    }
    // Even during drain the resync is harmless and lets the
    // controller's final teardown talk to the agent. Allow.

    let parsed: ClockResyncBody = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => return err(400, format!("invalid JSON body: {e}")),
    };

    // i64::try_from is the cleanest "is this representable as a
    // timeval.tv_sec?" gate. ~292 billion years of headroom on 64-bit
    // systems; nothing real will trip this.
    let tv_sec = match libc::time_t::try_from(parsed.ts) {
        Ok(n) => n,
        Err(_) => return err(400, format!("ts out of range: {}", parsed.ts)),
    };
    // settimeofday(2) requires CAP_SYS_TIME. PID 1 in the VM has the
    // full bounding set. The microsecond field is always 0 — we don't
    // need sub-second precision from a controller→agent handshake,
    // and a zero μs keeps the canonical body shape minimal.
    // SAFETY: tv is a fully-initialised C struct; the pointer points
    // at a stack local that outlives the syscall. settimeofday is
    // POSIX and thread-safe.
    let tv = libc::timeval { tv_sec, tv_usec: 0 };
    // SAFETY: `tv` is a fully-initialised C struct on the stack with
    // a lifetime that strictly outlives the syscall; we pass `NULL`
    // for the optional timezone argument (POSIX-deprecated). The
    // syscall is thread-safe and side-effect-free aside from
    // mutating the system wall clock. Wrapping libc time-set on the
    // resync endpoint is the controlled scope where unsafe is
    // unavoidable; the crate-wide `unsafe-code = "deny"` lint stays
    // on for every other call site.
    #[allow(unsafe_code)]
    let rc = unsafe { libc::settimeofday(&tv, std::ptr::null()) };
    if rc != 0 {
        let errno = std::io::Error::last_os_error();
        tracing::error!(
            errno = %errno,
            ts = parsed.ts,
            "clock_resync: settimeofday failed",
        );
        return err(500, format!("settimeofday failed: {errno}"));
    }
    tracing::info!(
        ts = parsed.ts,
        "clock_resync: CLOCK_REALTIME set from controller ts (bug #22 post-restore fixup)",
    );
    crate::metrics::inc_clock_resync();
    HttpResponse::Ok().json(&json!({"resynced": true, "ts": parsed.ts}))
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
    /// Deterministic 32-byte Ed25519 signing key for tests.
    /// **Test-only.** The agent never holds a SigningKey in
    /// production — only the controller does, and only the
    /// VerifyingKey is exposed to the agent.
    const TEST_SK_BYTES: [u8; 32] = [42u8; 32];

    fn test_signing_key() -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&TEST_SK_BYTES)
    }

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
        let pubkey_path = dir.join("controller-pubkey");
        let pk = test_signing_key().verifying_key();
        std::fs::File::create(&pubkey_path)
            .unwrap()
            .write_all(pk.as_bytes())
            .unwrap();
        let pubkey = crate::auth::load_pubkey_from_path(&pubkey_path).unwrap();
        let verifier = crate::sig::Verifier::new(pubkey);
        let workspace = crate::files::Workspace::open(&dir.join("ws")).unwrap();
        let state = AppState {
            verifier: Arc::new(verifier),
            workspace: Arc::new(workspace),
            draining: Arc::new(AtomicBool::new(false)),
            started_at_unix: 1234,
        };
        (state, dir)
    }

    /// Sign a request with the test signing key. Returns the three
    /// header values `(timestamp, nonce, signature)` to attach.
    fn sign(method: &str, path: &str, body: &[u8]) -> (String, String, String) {
        use std::sync::atomic::AtomicU64;
        static NONCE_COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = NONCE_COUNTER.fetch_add(1, AtomOrd::SeqCst);
        let nonce = format!("test-nonce-{}-{}", std::process::id(), n);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let sig = crate::sig::sign(&test_signing_key(), method, path, body, ts, &nonce);
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
                        web::resource("/_clock_resync")
                            .route(web::post().to(clock_resync)),
                    )
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
    async fn version_info_requires_auth() {
        let (state, _d) = make_state("ver_unauth");
        let app = make_app!(state);
        // Unsigned GET is now rejected (the previous behavior leaked
        // git_commit + capabilities to anything that reached :7777).
        let req = test::TestRequest::get().uri("/version").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[ntex::test]
    async fn version_info_has_required_fields() {
        let (state, _d) = make_state("ver");
        let app = make_app!(state);
        let req = signed("GET", "/version").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert!(body["agent_version"].is_string());
        assert!(body["git_commit"].is_string());
        assert_eq!(body["protocol_version"], 1);
        let caps = body["capabilities"].as_array().unwrap();
        let cap_strs: Vec<&str> = caps.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(cap_strs.contains(&"auth.ed25519-v1"));
        assert_eq!(body["started_at_unix"], 1234);
        // pubkey_fingerprint is included for ops verification —
        // 32 hex chars (16 bytes of SHA-256(pubkey)). Width is
        // load-bearing: pg `key_fp` CHECK requires `^[0-9a-f]{32}$`.
        let fp = body["pubkey_fingerprint"].as_str().unwrap();
        assert_eq!(fp.len(), 32);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// **Wire-shape stability** — the controller's stale-tenant
    /// detection (FM-A in nomad_ch.rs / k8s.rs) reads
    /// `pubkey_fingerprint` by string-keyed `serde_json::Value`
    /// access. A rename here (e.g. to `pubkey_fp`, `key_fp`,
    /// `controller_pubkey_fingerprint`) would silently break that
    /// check — wait_for_agent_livez would unwrap to None and fall
    /// into the legacy-agent path, undoing FM-A. The stable contract
    /// is: the EXACT key name `pubkey_fingerprint`, a string, 32
    /// hex chars (first 16 bytes of SHA-256(pubkey) hex-encoded).
    /// Width is also a contract: `sandbox.sandboxes.key_fp` has a
    /// pg-side `CHECK (key_fp ~ '^[0-9a-f]{32}$')`. If any field has
    /// to change, treat it as a wire-protocol breakage: bump
    /// PROTOCOL_VERSION + add a new capability + keep the old field
    /// for one release.
    #[ntex::test]
    async fn version_pubkey_fingerprint_field_name_is_stable_contract() {
        let (state, _d) = make_state("ver_stable");
        let app = make_app!(state);
        let req = signed("GET", "/version").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        // The exact field name controllers depend on:
        let fp_value = body
            .get("pubkey_fingerprint")
            .expect(
                "WIRE-CONTRACT BREAKAGE: /version no longer emits \
                 `pubkey_fingerprint` — the controller's stale-tenant \
                 detection reads this exact key. If you really must \
                 rename, bump PROTOCOL_VERSION and update both K8s \
                 and nomad-ch backends in the same commit.",
            );
        let fp = fp_value
            .as_str()
            .expect("WIRE-CONTRACT: pubkey_fingerprint must be a string");
        assert_eq!(
            fp.len(),
            32,
            "WIRE-CONTRACT: pubkey_fingerprint must be exactly 32 hex \
             chars (16 bytes of SHA-256(pubkey) hex-encoded). Pg \
             `key_fp` CHECK requires `^[0-9a-f]{{32}}$`; a shorter \
             value silently fails the snapshot/restore INSERT."
        );
        assert!(
            fp.chars().all(|c| c.is_ascii_hexdigit()),
            "WIRE-CONTRACT: pubkey_fingerprint must be hex (got {fp:?})"
        );
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
        let sig = crate::sig::sign(
            &test_signing_key(),
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

    // ─── Bug #22 fix: /_clock_resync ─────────────────────────────
    //
    // The endpoint MUST:
    //   1. Reject unsigned / wrong-key requests with 401.
    //   2. Reject tampered body with 401 (canonical hash mismatch).
    //   3. ACCEPT signed requests whose ts is far outside the normal
    //      5-second skew window — that's the entire point.
    //   4. Reject malformed bodies with 400 (not 401).
    //
    // Test 3 is the load-bearing regression assertion: the cluster
    // smoke (Appendix E) saw 9/9 wakes 401 on /exec because the
    // strict-skew gate rejected every controller-signed RPC after CH
    // `--restore`. The skew-bypass path on /_clock_resync is the
    // recovery handshake; if it stops accepting far-future ts the
    // wake path immediately re-breaks.

    /// Sign a `/_clock_resync` request with a CALLER-supplied ts so
    /// the test can vary it freely. The default `sign()` helper above
    /// always uses `SystemTime::now()` which can't simulate the
    /// snapshot→wake gap.
    fn sign_with_ts(
        method: &str,
        path: &str,
        body: &[u8],
        ts: u64,
        nonce: &str,
    ) -> (String, String, String) {
        let sig = crate::sig::sign(&test_signing_key(), method, path, body, ts, nonce);
        (ts.to_string(), nonce.to_string(), sig)
    }

    fn clock_resync_req(ts: u64, nonce: &str, body: &str) -> test::TestRequest {
        let (ts_hdr, nonce_hdr, sig) =
            sign_with_ts("POST", "/_clock_resync", body.as_bytes(), ts, nonce);
        test::TestRequest::post()
            .uri("/_clock_resync")
            .header("content-type", "application/json")
            .header("x-sbx-timestamp", ts_hdr)
            .header("x-sbx-nonce", nonce_hdr)
            .header("x-sbx-signature", sig)
            .set_payload(body.to_string())
    }

    /// Unauthenticated request → 401. The drop-through-to-400 case
    /// (missing JSON body) MUST NOT fire before the signature check.
    #[ntex::test]
    async fn clock_resync_without_signature_returns_401() {
        let (state, _d) = make_state("resync-noauth");
        let app = make_app!(state);
        let req = test::TestRequest::post()
            .uri("/_clock_resync")
            .header("content-type", "application/json")
            .set_payload(r#"{"ts":1700000000}"#)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// **CRITICAL**: a signed request whose ts is well outside the
    /// 5-second skew window MUST validate. This is the whole reason
    /// the endpoint exists; if the skew-bypass code path regresses,
    /// every cluster wake breaks on the first /exec.
    #[ntex::test]
    async fn clock_resync_accepts_far_future_ts() {
        let (state, _d) = make_state("resync-far");
        let app = make_app!(state);
        // We do NOT actually settimeofday in the test (the test
        // process isn't PID 1 + CAP_SYS_TIME), so this test ONLY
        // verifies the auth-gate behaves correctly: if the canonical
        // verifies under the skew-bypass path, the handler dispatches
        // to settimeofday. The test runs as a non-privileged user so
        // settimeofday returns EPERM → handler returns 500.
        //
        // What we assert here:
        //   - status code is NOT 401 (the bug we're fixing — auth
        //     was rejecting the call). Either 200 (test ran as root,
        //     unlikely in CI) or 500 (auth passed, settimeofday
        //     returned EPERM) is acceptable evidence the bypass
        //     works.
        let body = r#"{"ts":1900000000}"#;
        // 1900000000 is ~year 2030 — guaranteed > 5s skew at any
        // real wall-clock the test process will see.
        let req = clock_resync_req(1_900_000_000, "resync-far-test", body).to_request();
        let resp = test::call_service(&app, req).await;
        assert_ne!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "Bug #22 regression: skew-bypass rejected a far-future signed ts. \
             Auth path is wrong — every cluster wake will 401 on first /exec."
        );
        // Accept 200 (root) or 500 (non-root EPERM). Either proves
        // auth passed; what we forbid is 401.
        assert!(
            matches!(
                resp.status(),
                StatusCode::OK | StatusCode::INTERNAL_SERVER_ERROR
            ),
            "expected 200 or 500 post-auth; got {}",
            resp.status()
        );
    }

    /// Same shape, far-past ts. The skew check is `|now - ts| >
    /// SKEW_S`, so far-past values are equally rejected by the
    /// strict-skew gate — and equally must be accepted by the
    /// bypass.
    #[ntex::test]
    async fn clock_resync_accepts_far_past_ts() {
        let (state, _d) = make_state("resync-past");
        let app = make_app!(state);
        let body = r#"{"ts":1500000000}"#;
        let req = clock_resync_req(1_500_000_000, "resync-past-test", body).to_request();
        let resp = test::call_service(&app, req).await;
        assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// Tampered body (canonical hash mismatch) → 401. The skew-bypass
    /// path does NOT weaken any other check.
    #[ntex::test]
    async fn clock_resync_with_tampered_body_returns_401() {
        let (state, _d) = make_state("resync-tamper");
        let app = make_app!(state);
        // Sign for body A, send body B.
        let signed_body = r#"{"ts":1700000000}"#;
        let sent_body = r#"{"ts":1800000000}"#;
        let ts: u64 = 1_700_000_000;
        let nonce = "resync-tamper-test";
        let (ts_hdr, nonce_hdr, sig) =
            sign_with_ts("POST", "/_clock_resync", signed_body.as_bytes(), ts, nonce);
        let req = test::TestRequest::post()
            .uri("/_clock_resync")
            .header("content-type", "application/json")
            .header("x-sbx-timestamp", ts_hdr)
            .header("x-sbx-nonce", nonce_hdr)
            .header("x-sbx-signature", sig)
            .set_payload(sent_body.to_string())
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// Request signed with a non-controller key → 401. Wrong-key
    /// rejection MUST still fire under the skew-bypass path.
    #[ntex::test]
    async fn clock_resync_with_wrong_key_returns_401() {
        let (state, _d) = make_state("resync-wrongkey");
        let app = make_app!(state);
        // Sign with a key the agent's verifier does NOT trust.
        let other_sk = ed25519_dalek::SigningKey::from_bytes(&[99u8; 32]);
        let ts: u64 = 1_900_000_000;
        let nonce = "resync-wrongkey-test";
        let body = r#"{"ts":1900000000}"#;
        let sig = crate::sig::sign(&other_sk, "POST", "/_clock_resync", body.as_bytes(), ts, nonce);
        let req = test::TestRequest::post()
            .uri("/_clock_resync")
            .header("content-type", "application/json")
            .header("x-sbx-timestamp", ts.to_string())
            .header("x-sbx-nonce", nonce)
            .header("x-sbx-signature", sig)
            .set_payload(body.to_string())
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// Malformed JSON body → 400 (not 401). The signature check
    /// fires BEFORE serde_json, so this only happens when the bytes
    /// are signed correctly but the JSON is junk — a server bug, not
    /// an attack.
    #[ntex::test]
    async fn clock_resync_with_malformed_json_returns_400() {
        let (state, _d) = make_state("resync-badjson");
        let app = make_app!(state);
        let body = "not json at all";
        let req = clock_resync_req(1_900_000_000, "resync-badjson-test", body).to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// Replayed nonce → 401 (`ReplayedNonce` audit reason). The
    /// skew-bypass path does NOT weaken the LRU defense.
    #[ntex::test]
    async fn clock_resync_replay_returns_401() {
        let (state, _d) = make_state("resync-replay");
        let app = make_app!(state);
        let body = r#"{"ts":1900000000}"#;
        let ts: u64 = 1_900_000_000;
        let nonce = "resync-replay-test";
        let req1 = clock_resync_req(ts, nonce, body).to_request();
        let resp1 = test::call_service(&app, req1).await;
        // First call: not 401 (200 or 500 — see far-future test).
        assert_ne!(resp1.status(), StatusCode::UNAUTHORIZED);
        // Replay with the SAME nonce + ts + body must 401.
        let req2 = clock_resync_req(ts, nonce, body).to_request();
        let resp2 = test::call_service(&app, req2).await;
        assert_eq!(resp2.status(), StatusCode::UNAUTHORIZED);
    }
}
