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

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use lru::LruCache;
use ntex::util::Bytes;
use ntex::web::{self, HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::json;

use crate::audit;
use crate::exec;
use crate::files::Workspace;
use crate::sig::{self, AuthFail, CanonicalKind, ResyncBody, Verifier};
use crate::version;

/// **R7-S1.** The agent's own sandbox UUID, learned at boot from
/// `SANDBOX_AGENT_SANDBOX_ID` env (preferred) or `/run/keys/sandbox-id`.
/// Set once via [`init_sandbox_id_from_env`] from `main.rs` before
/// the server starts; reading is `&'static str`-cheap thereafter.
///
/// The clock-resync handler asserts the controller-signed body's
/// `sandbox_id` field equals this value; a captured resync from a
/// different sandbox is rejected before the LRU is consulted.
///
/// **Why a static, not an `AppState` field.** The existing
/// `state_with_paths` constructor (in `lib.rs`) takes (pubkey_path,
/// workspace_path); adding a third parameter would ripple through
/// every test helper and the SDK call sites. A process-local
/// `OnceLock` set from `main.rs` is the smaller blast radius for the
/// R7-S1 hardening pass — handlers read it at most once per resync
/// request, `OnceLock` is lock-free on the hot path.
///
/// Tests set this via [`test_set_sandbox_id`] so the handler can be
/// driven through the in-memory ntex transport without env var racing.
static SANDBOX_ID: OnceLock<String> = OnceLock::new();

/// **R7-S1.** Process-local LRU of recently-seen resync challenges
/// (hex strings). Sized to absorb a burst of restore cycles plus any
/// retry-on-transient logic the controller may grow in future — see
/// `RESYNC_CHALLENGE_CAPACITY` for the sizing argument.
///
/// Initialised lazily on first access via the helper accessor below
/// so test code that never touches resync doesn't allocate the LRU.
static RESYNC_CHALLENGES: OnceLock<Mutex<LruCache<String, ()>>> = OnceLock::new();

/// Bound on the resync-challenge LRU.
///
/// Sized for N concurrent restores × M retries per restore = N×M entries.
/// With max 12 vm_index slots × 2–3 retries each = ~36; 32 gives some
/// headroom but caps memory growth (the agent's heap is part of the CH
/// memory image, so an unbounded cache would bloat every snapshot).
///
/// The previous value of 4 was fragile against any future retry-on-
/// transient logic that pushes 3+ resyncs per cycle: LRU eviction of a
/// recently-issued challenge would re-open the replay window the cache
/// exists to close. 32 is comfortably above worst-case observed traffic
/// while remaining ~2 KB of resident heap.
const RESYNC_CHALLENGE_CAPACITY: usize = 32;

/// R7-S1 hex-encoding width for the resync challenge. 32 random bytes
/// → 64 lowercase hex chars. The agent rejects any other length before
/// consulting the LRU so a "challenge=" or 1-byte stub can't get past
/// the body-shape gate.
const RESYNC_CHALLENGE_HEX_LEN: usize = 64;

/// R7-S1 init — call once from `main.rs` after parsing env. Reads the
/// `SANDBOX_AGENT_SANDBOX_ID` env var (preferred) or
/// `/run/keys/sandbox-id` file (fallback for wrappers that mount the
/// id rather than env-inject it). Returns `Err` if neither source is
/// available; the caller should log + treat as a hard error (the
/// resync handler will reject every request without a known id, which
/// would wedge every cluster wake → loud crash is better).
///
/// Idempotent: a second call after the OnceLock is set returns Ok
/// without re-reading. Subsequent calls with a DIFFERENT id return Ok
/// but do NOT overwrite — the OnceLock semantics are write-once.
///
/// `pub(crate)` so the public surface stays narrow — the only intended
/// caller is `main.rs`, which now goes through
/// [`crate::boot_init_sandbox_id`] (the canonical, purposefully-named
/// binary entry point).
pub(crate) fn init_sandbox_id_from_env() -> Result<(), String> {
    if SANDBOX_ID.get().is_some() {
        return Ok(());
    }
    let id = read_sandbox_id_from_sources(SANDBOX_ID_FALLBACK_PATH)?;
    // OnceLock::set returns Err if already set — that's a no-op here
    // (we checked above). Tolerate the race for symmetry.
    let _ = SANDBOX_ID.set(id);
    Ok(())
}

/// Production fallback path for the sandbox id when the env var is
/// unset. Mounted by the wrapper at the same place as the controller
/// pubkey. Lives in a constant so tests can address it (the test
/// helper uses a temp path via [`read_sandbox_id_from_sources`]).
const SANDBOX_ID_FALLBACK_PATH: &str = "/run/keys/sandbox-id";

/// Read the sandbox id from the env var (preferred) or the supplied
/// fallback file path. Returns the trimmed id string or an error
/// describing which source failed. **Pure** — does not touch the
/// [`SANDBOX_ID`] OnceLock. Factored out of [`init_sandbox_id_from_env`]
/// so the parsing/fallback/empty-id branches can be exercised by direct
/// tests without burning the global OnceLock on every test.
///
/// Behaviour parity with the previous inline body:
///   - env var wins when present (even when empty — the empty-id guard
///     then trips)
///   - on env-var absent (`VarError::NotPresent`), read the file; map
///     any IO error to a single descriptive string
///   - file contents are trimmed (drops trailing newline from the
///     wrapper's `echo "$id" >` pattern)
///   - empty id (from either source) → error
fn read_sandbox_id_from_sources(fallback_path: &str) -> Result<String, String> {
    let id = if let Ok(v) = std::env::var("SANDBOX_AGENT_SANDBOX_ID") {
        v
    } else {
        // Fallback: a small file mounted by the wrapper. Same path
        // convention as the pubkey mount.
        std::fs::read_to_string(fallback_path)
            .map_err(|e| format!(
                "SANDBOX_AGENT_SANDBOX_ID env unset and {fallback_path} read failed: {e}"
            ))?
            .trim()
            .to_string()
    };
    if id.is_empty() {
        return Err("sandbox_id is empty".to_string());
    }
    Ok(id)
}

/// Test-only setter for the boot-time sandbox_id. Idempotent across
/// the test suite because OnceLock writes are single-shot; tests that
/// need a fresh id should run in their own process (or use
/// [`test_set_sandbox_id`] from the first test that touches it).
#[cfg(test)]
pub fn test_set_sandbox_id(id: &str) {
    let _ = SANDBOX_ID.set(id.to_string());
}

/// Read the boot-time sandbox_id. Returns `None` if init was never
/// called (handlers treat this as an unrecoverable misconfiguration
/// and 500 the request).
fn boot_sandbox_id() -> Option<&'static str> {
    SANDBOX_ID.get().map(String::as_str)
}

/// Lazily-initialised handle to the resync challenge LRU.
fn resync_challenges() -> &'static Mutex<LruCache<String, ()>> {
    RESYNC_CHALLENGES.get_or_init(|| {
        let cap = NonZeroUsize::new(RESYNC_CHALLENGE_CAPACITY)
            .expect("RESYNC_CHALLENGE_CAPACITY > 0");
        Mutex::new(LruCache::new(cap))
    })
}

/// Clear the resync-challenge LRU. Test-only escape hatch kept for
/// future tests that need a deterministic starting state.
///
/// **Production callers MUST NOT use this** — dropping the LRU lets
/// a replay window open. The R7-S1 regression suite ([
/// `clock_resync_accepts_fresh_challenge`,
/// `clock_resync_rejects_replayed_challenge`]) does NOT call this;
/// every test uses [`unique_challenge`] to generate a collision-
/// resistant per-test value. Clearing mid-suite would race the
/// replay-rejection test (its second call relies on the LRU still
/// containing the challenge from the first call).
#[cfg(test)]
#[allow(dead_code)] // kept as a deliberate escape hatch; see doc-comment.
fn test_clear_resync_challenges() {
    let mut g = resync_challenges().lock().unwrap_or_else(|p| p.into_inner());
    g.clear();
}

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
//
// Every error response below funnels through `crate::error_envelope`
// so the wire shape matches proposal § 10.0 A4:
//     { "error": "<machine_kind>", "message": "<human prose>" }
// Pre-migration these emitted `{"error":"<prose>"}` (no `message`)
// or `{"error":"<kind>"}` (also no `message`) — see the module
// docstring on `error_envelope` for the full account.

fn unauthorized() -> HttpResponse {
    crate::error_envelope::error_response(
        ntex::http::StatusCode::UNAUTHORIZED,
        "unauthorized",
        "authentication required",
    )
}

/// 503 Service Unavailable — used for auth-gated endpoints once
/// `/shutdown` flips the drain flag. We return this BEFORE doing
/// any expensive work so a slow controller retry-loop can't pile
/// up new long-running execs while ntex's shutdown timeout
/// approaches.
fn draining() -> HttpResponse {
    crate::error_envelope::error_response(
        ntex::http::StatusCode::SERVICE_UNAVAILABLE,
        "draining",
        "agent is draining for shutdown",
    )
}

fn err(status: u16, msg: impl Into<String>) -> HttpResponse {
    crate::error_envelope::error_from_status(status, msg)
}

/// 404 Not Found in A4-envelope shape. Exposed publicly because the
/// binary's `default_service` lives in `main.rs` and needs to emit
/// the same wire shape as every other error path; without this
/// wrapper, `main.rs` would have to reach into `pub(crate)`
/// `error_envelope` (which it can't, as the binary is a separate
/// crate from the lib).
pub fn not_found() -> HttpResponse {
    crate::error_envelope::error_response(
        ntex::http::StatusCode::NOT_FOUND,
        "not_found",
        "not found",
    )
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
pub(crate) struct ExecBody {
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

// ─── /_clock_resync — bug #22 + R7-S1 hardening ─────────────────
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
//   2. Parses the body `{"sandbox_id": <uuid>, "ts": <unix_secs>,
//      "challenge": <hex64>}` (R7-S1 hardening — pre-R7-S1 the body
//      was just `{"ts": ...}`).
//   3. Asserts `sandbox_id` equals the agent's own boot-time-known id;
//      asserts `challenge` is exactly 64 lowercase hex chars and is
//      not in the resync-challenge LRU.
//   4. Calls `settimeofday(2)` to set `CLOCK_REALTIME` to the
//      controller's signed ts.
//   5. Records the challenge in the LRU and returns 200.
//
// Security: the signature requires the controller's private key, so
// an in-VM attacker cannot push the clock. The challenge LRU + the
// `sandbox_id` bind close R7-S1's replay-DoS surface: a captured
// cycle-N resync replayed against cycle-N+1 either matches the LRU
// (rejected) or carries the wrong sandbox_id (rejected). The nonce
// LRU on the verifier path remains in place; this layer is in
// addition.
//
// Idempotency: a re-call with a fresh challenge/nonce simply re-sets
// the clock; no agent-side state persists beyond the LRU's per-
// challenge uniqueness. The controller can retry on transient network
// errors by minting a fresh challenge.

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

    let parsed: ResyncBody = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => return err(400, format!("invalid JSON body: {e}")),
    };

    // R7-S1: sandbox_id bind. The controller signs the body with the
    // sandbox-specific UUID; the agent rejects any value that doesn't
    // match its own boot-time-known id. Misconfiguration (id not
    // initialised) is a hard 500 — every cluster wake would be
    // unauthenticated and we want operators to see a loud failure
    // instead of a silent skew-bypass-without-binding.
    let agent_id = match boot_sandbox_id() {
        Some(s) => s,
        None => {
            tracing::error!(
                "clock_resync: SANDBOX_ID not initialised — refusing resync \
                 (call init_sandbox_id_from_env in main before serving)"
            );
            audit::record(
                audit::events::AUTH_FAIL,
                "endpoint=clock_resync reason=sandbox-id-unset",
            );
            crate::metrics::inc_auth_fail("sandbox-id-unset");
            return err(500, "agent misconfigured: sandbox_id not initialised");
        }
    };
    if parsed.sandbox_id != agent_id {
        audit::record(
            audit::events::AUTH_FAIL,
            &format!(
                "endpoint=clock_resync reason=sandbox-id-mismatch \
                 want_len={} got_len={}",
                agent_id.len(),
                parsed.sandbox_id.len(),
            ),
        );
        crate::metrics::inc_auth_fail("sandbox-id-mismatch");
        return unauthorized();
    }

    // R7-S1: challenge shape gate. Must be exactly 64 lowercase hex
    // chars (32 bytes of random). We check shape BEFORE consulting the
    // LRU so an attacker can't pollute the LRU with cheap garbage
    // strings even with a valid signature on a malformed body.
    if parsed.challenge.len() != RESYNC_CHALLENGE_HEX_LEN
        || !parsed
            .challenge
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        audit::record(
            audit::events::AUTH_FAIL,
            &format!(
                "endpoint=clock_resync reason=challenge-bad-shape len={}",
                parsed.challenge.len()
            ),
        );
        crate::metrics::inc_auth_fail("challenge-bad-shape");
        return unauthorized();
    }

    // R7-S1: replay check + record. The challenge LRU's contains-check
    // is the load-bearing replay defense — a captured cycle-N resync
    // replayed against cycle-N+1 carries cycle-N's challenge → hit →
    // reject. We record the challenge IMMEDIATELY after the contains
    // check (before settimeofday) for two reasons:
    //
    //   1. The signature already binds the entire body (including the
    //      challenge) to the controller's private key, so recording a
    //      not-yet-acted-on challenge is safe — the same body can
    //      never be replayed by a different signer.
    //   2. If settimeofday fails (e.g., EPERM in tests where the
    //      process isn't PID 1), we still want the LRU to remember
    //      the challenge so a retry attempt from a captured replay
    //      can't slip through during the same agent lifetime.
    //
    // We drop the lock before settimeofday so the syscall doesn't
    // serialise behind LRU operations from any concurrent future
    // handler.
    {
        let mut cache = resync_challenges()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if cache.contains(&parsed.challenge) {
            audit::record(
                audit::events::AUTH_REPLAY,
                "endpoint=clock_resync reason=challenge-replayed",
            );
            crate::metrics::inc_auth_fail("challenge-replayed");
            return unauthorized();
        }
        cache.put(parsed.challenge.clone(), ());
    }

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
        // A4 envelope: `error` carries the machine kind, `message`
        // the prose. The path-containing detail moved to `message`
        // when this site migrated through `error_envelope`.
        assert_eq!(body["error"], "not_found");
        assert!(
            body["message"].as_str().unwrap().contains("no-such"),
            "message must still mention the path; got {body}",
        );
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

    // ─── Bug #22 + R7-S1: /_clock_resync ──────────────────────────
    //
    // The endpoint MUST:
    //   1. Reject unsigned / wrong-key requests with 401.
    //   2. Reject tampered body with 401 (canonical hash mismatch).
    //   3. ACCEPT signed requests whose ts is far outside the normal
    //      5-second skew window — that's the entire point.
    //   4. Reject malformed bodies with 400 (not 401).
    //   5. **R7-S1**: reject body that omits `sandbox_id` (treated as
    //      malformed JSON → 400 since serde rejects).
    //   6. **R7-S1**: reject body whose `sandbox_id` mismatches the
    //      agent's own boot-time-known id → 401.
    //   7. **R7-S1**: reject a body whose `challenge` is in the LRU
    //      → 401 (replay defense).
    //   8. **R7-S1**: accept a body whose `challenge` is fresh (32-byte
    //      hex), distinct from the previous test's value.
    //
    // Tests 5-8 are R7-S1's load-bearing regression assertions: the
    // pre-R7-S1 body shape was just `{"ts": <unix_secs>}`, with no
    // per-restore challenge and no sandbox_id bind. A network-adjacent
    // attacker who captured cycle-N's resync could race cycle-N+1 to
    // re-set CLOCK_REALTIME to the stale value, wedging every strict-
    // skew RPC. The challenge LRU + sandbox_id bind close that surface.

    /// Set the boot-time sandbox_id for the test process. Idempotent
    /// across the entire suite — OnceLock semantics mean the first
    /// call wins. All clock_resync tests use this same id so they can
    /// share the global state without racing.
    const TEST_SANDBOX_ID: &str = "01900000-0000-7000-8000-000000000000";

    fn ensure_test_sandbox_id() {
        test_set_sandbox_id(TEST_SANDBOX_ID);
    }

    /// Each test uses a unique challenge so they don't collide in the
    /// global RESYNC_CHALLENGES LRU. 32 random bytes → 64 hex chars;
    /// we synthesise from a counter + label so the hex pattern is
    /// distinctive in audit logs if a test fails.
    fn unique_challenge(label: &str) -> String {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, AtomOrd::SeqCst);
        // 64 lowercase hex chars. Embed label hash + counter to avoid
        // collision across tests that all use, say, "0000…0001".
        let mut bytes = [0u8; 32];
        let label_hash = label.bytes().fold(0u8, |a, b| a.wrapping_add(b));
        bytes[0] = label_hash;
        bytes[1..9].copy_from_slice(&n.to_be_bytes());
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Build the R7-S1 body shape for `/_clock_resync`.
    fn resync_body(ts: u64, challenge: &str) -> String {
        format!(
            r#"{{"sandbox_id":"{TEST_SANDBOX_ID}","ts":{ts},"challenge":"{challenge}"}}"#
        )
    }

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
        ensure_test_sandbox_id();
        let (state, _d) = make_state("resync-noauth");
        let app = make_app!(state);
        let challenge = unique_challenge("noauth");
        let body = resync_body(1_700_000_000, &challenge);
        let req = test::TestRequest::post()
            .uri("/_clock_resync")
            .header("content-type", "application/json")
            .set_payload(body)
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
        ensure_test_sandbox_id();
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
        let challenge = unique_challenge("far");
        let body = resync_body(1_900_000_000, &challenge);
        // 1900000000 is ~year 2030 — guaranteed > 5s skew at any
        // real wall-clock the test process will see.
        let req = clock_resync_req(1_900_000_000, "resync-far-test", &body).to_request();
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
        ensure_test_sandbox_id();
        let (state, _d) = make_state("resync-past");
        let app = make_app!(state);
        let challenge = unique_challenge("past");
        let body = resync_body(1_500_000_000, &challenge);
        let req = clock_resync_req(1_500_000_000, "resync-past-test", &body).to_request();
        let resp = test::call_service(&app, req).await;
        assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// Tampered body (canonical hash mismatch) → 401. The skew-bypass
    /// path does NOT weaken any other check.
    #[ntex::test]
    async fn clock_resync_with_tampered_body_returns_401() {
        ensure_test_sandbox_id();
        let (state, _d) = make_state("resync-tamper");
        let app = make_app!(state);
        // Sign for body A, send body B.
        let c1 = unique_challenge("tamper-1");
        let c2 = unique_challenge("tamper-2");
        let signed_body = resync_body(1_700_000_000, &c1);
        let sent_body = resync_body(1_800_000_000, &c2);
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
            .set_payload(sent_body)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// Request signed with a non-controller key → 401. Wrong-key
    /// rejection MUST still fire under the skew-bypass path.
    #[ntex::test]
    async fn clock_resync_with_wrong_key_returns_401() {
        ensure_test_sandbox_id();
        let (state, _d) = make_state("resync-wrongkey");
        let app = make_app!(state);
        // Sign with a key the agent's verifier does NOT trust.
        let other_sk = ed25519_dalek::SigningKey::from_bytes(&[99u8; 32]);
        let ts: u64 = 1_900_000_000;
        let nonce = "resync-wrongkey-test";
        let challenge = unique_challenge("wrongkey");
        let body = resync_body(1_900_000_000, &challenge);
        let sig = crate::sig::sign(&other_sk, "POST", "/_clock_resync", body.as_bytes(), ts, nonce);
        let req = test::TestRequest::post()
            .uri("/_clock_resync")
            .header("content-type", "application/json")
            .header("x-sbx-timestamp", ts.to_string())
            .header("x-sbx-nonce", nonce)
            .header("x-sbx-signature", sig)
            .set_payload(body)
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
        ensure_test_sandbox_id();
        let (state, _d) = make_state("resync-badjson");
        let app = make_app!(state);
        let body = "not json at all";
        let req = clock_resync_req(1_900_000_000, "resync-badjson-test", body).to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// Replayed nonce → 401 (`ReplayedNonce` audit reason). The
    /// skew-bypass path does NOT weaken the LRU defense. **Note**:
    /// this exercises the verifier's nonce LRU, which fires BEFORE
    /// the R7-S1 challenge LRU — same body + same nonce → nonce
    /// replay surfaces first.
    #[ntex::test]
    async fn clock_resync_replay_returns_401() {
        ensure_test_sandbox_id();
        let (state, _d) = make_state("resync-replay");
        let app = make_app!(state);
        let challenge = unique_challenge("nonce-replay");
        let body = resync_body(1_900_000_000, &challenge);
        let ts: u64 = 1_900_000_000;
        let nonce = "resync-replay-test";
        let req1 = clock_resync_req(ts, nonce, &body).to_request();
        let resp1 = test::call_service(&app, req1).await;
        // First call: not 401 (200 or 500 — see far-future test).
        assert_ne!(resp1.status(), StatusCode::UNAUTHORIZED);
        // Replay with the SAME nonce + ts + body must 401.
        let req2 = clock_resync_req(ts, nonce, &body).to_request();
        let resp2 = test::call_service(&app, req2).await;
        assert_eq!(resp2.status(), StatusCode::UNAUTHORIZED);
    }

    // ─── R7-S1 regression tests ───────────────────────────────────
    //
    // The 4 tests below pin the sandbox_id + per-restore challenge
    // binds that close R7-S1. They use distinct nonces + challenges
    // so each test exercises ONE constraint without bleeding into
    // the others via the shared verifier LRU or challenge LRU.

    /// **R7-S1**: a body that omits `sandbox_id` → 400. serde rejects
    /// the missing required field; the handler maps that to 400, not
    /// 401, because the signature itself was valid (the attacker
    /// would have needed the controller's private key to even reach
    /// the JSON parse).
    #[ntex::test]
    async fn clock_resync_rejects_missing_sandbox_id() {
        ensure_test_sandbox_id();
        let (state, _d) = make_state("resync-missing-sbid");
        let app = make_app!(state);
        let challenge = unique_challenge("missing-sbid");
        // No "sandbox_id" field — serde::Deserialize requires it.
        let body = format!(
            r#"{{"ts":1900000000,"challenge":"{challenge}"}}"#
        );
        let req = clock_resync_req(
            1_900_000_000,
            "resync-missing-sbid-nonce",
            &body,
        )
        .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "R7-S1 regression: body without sandbox_id must 400 (serde missing-field)"
        );
    }

    /// **R7-S1**: a body whose `sandbox_id` does not match the agent's
    /// own boot-time-known id → 401. This is the explicit replay
    /// defense for cross-sandbox capture: even if the attacker could
    /// somehow obtain a signed-and-fresh resync for sandbox A, they
    /// cannot replay it against sandbox B's agent because the body
    /// is bound to A's UUID.
    #[ntex::test]
    async fn clock_resync_rejects_wrong_sandbox_id() {
        ensure_test_sandbox_id();
        let (state, _d) = make_state("resync-wrong-sbid");
        let app = make_app!(state);
        let challenge = unique_challenge("wrong-sbid");
        // Different UUID — agent has TEST_SANDBOX_ID; signed body
        // carries some-other-sandbox-id.
        let other_sbid = "01900000-0000-7000-8000-000000000999";
        let body = format!(
            r#"{{"sandbox_id":"{other_sbid}","ts":1900000000,"challenge":"{challenge}"}}"#
        );
        let req = clock_resync_req(
            1_900_000_000,
            "resync-wrong-sbid-nonce",
            &body,
        )
        .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "R7-S1 regression: body with wrong sandbox_id must 401 (cross-sandbox replay)"
        );
    }

    /// **R7-S1 CRITICAL**: a body whose `challenge` is in the LRU
    /// (i.e., already seen by THIS agent process) → 401. This is the
    /// load-bearing defense against the post-restore replay DoS: an
    /// attacker who captured cycle-N's resync (sig + ts + nonce +
    /// body bytes verbatim) cannot replay it against cycle-N+1 with
    /// a NEW verifier nonce, because the challenge inside the body
    /// is still cycle-N's value and is in the agent's LRU. Note we
    /// use distinct nonces here so the verifier's nonce LRU does NOT
    /// fire first — the challenge LRU is what we're pinning.
    #[ntex::test]
    async fn clock_resync_rejects_replayed_challenge() {
        ensure_test_sandbox_id();
        let (state, _d) = make_state("resync-replay-chal");
        let app = make_app!(state);
        let challenge = unique_challenge("replay-chal");
        let body = resync_body(1_900_000_000, &challenge);

        // First call: use nonce N1. Should succeed past auth (200 or
        // 500 depending on settimeofday capability). The challenge is
        // now in the LRU.
        let req1 = clock_resync_req(
            1_900_000_000,
            "resync-replay-chal-n1",
            &body,
        )
        .to_request();
        let resp1 = test::call_service(&app, req1).await;
        assert_ne!(
            resp1.status(),
            StatusCode::UNAUTHORIZED,
            "first call must pass auth so the challenge enters the LRU"
        );

        // Second call: SAME challenge, fresh nonce N2 and fresh ts to
        // bypass the verifier's nonce/ts replay defenses. The
        // verifier accepts the signature (different canonical bytes
        // → different nonce / ts), but the handler MUST reject because
        // the challenge is already in the LRU.
        let req2 = clock_resync_req(
            1_900_000_001, // +1s — different canonical, signature still valid for THIS ts
            "resync-replay-chal-n2",
            &body, // same body bytes → same challenge → must lose to LRU
        )
        .to_request();
        // Note: the body bytes encode ts=1_900_000_000 inside; but
        // the signature is over the body bytes verbatim, so signing
        // with ts=1_900_000_001 in the HEADER does not change the
        // body. The body-hash slot in the canonical covers the body
        // bytes verbatim → signature is valid for THIS request even
        // though the inner ts is stale. That's the exact attack
        // scenario R7-S1 closes.
        let resp2 = test::call_service(&app, req2).await;
        assert_eq!(
            resp2.status(),
            StatusCode::UNAUTHORIZED,
            "R7-S1 CRITICAL: same challenge replayed with fresh outer nonce/ts \
             must 401 — challenge LRU is the load-bearing defense against \
             the post-restore replay-DoS."
        );
    }

    /// **R7-S1**: a body with a FRESH challenge (not in the LRU) →
    /// passes auth. Confirms the happy path still works after the
    /// hardening; without this, the previous three tests would
    /// trivially pass via a global "reject everything" defect.
    ///
    /// **Test isolation note.** The challenge LRU is process-global
    /// (`OnceLock<Mutex<LruCache>>`). Tests share it across the
    /// thread-pool, so we must NOT call `test_clear_resync_challenges`
    /// here — clearing mid-suite would race the
    /// `clock_resync_rejects_replayed_challenge` test (whose second
    /// call relies on the LRU still containing the challenge from
    /// the first call). Instead, every test uses `unique_challenge`
    /// to generate a collision-resistant value (per-test counter +
    /// per-test label byte → no cross-test collision).
    #[ntex::test]
    async fn clock_resync_accepts_fresh_challenge() {
        ensure_test_sandbox_id();
        let (state, _d) = make_state("resync-fresh-chal");
        let app = make_app!(state);
        let challenge = unique_challenge("fresh");
        let body = resync_body(1_900_000_000, &challenge);
        let req = clock_resync_req(
            1_900_000_000,
            "resync-fresh-chal-nonce",
            &body,
        )
        .to_request();
        let resp = test::call_service(&app, req).await;
        // Same accept-criterion as the bug-#22 far-future test:
        // 200 if root + CAP_SYS_TIME, 500 if EPERM. 401 is the
        // failure mode that breaks R7-S1.
        assert_ne!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "R7-S1: a fresh challenge must pass the LRU gate"
        );
        assert!(
            matches!(
                resp.status(),
                StatusCode::OK | StatusCode::INTERNAL_SERVER_ERROR
            ),
            "expected 200 or 500 post-auth; got {}",
            resp.status()
        );
    }

    // ─── A4 wire-shape tests (handlers helpers) ─────────────────────
    //
    // These pin the wire shape of `err`, `unauthorized`, and
    // `draining` (the three helpers funnelled through
    // `crate::error_envelope`). A regression that drops the `message`
    // field or flips `error` back to prose fails one of these tests
    // immediately.

    #[ntex::test]
    async fn a4_unauthorized_wire_shape() {
        let (state, _d) = make_state("a4_unauth");
        let app = make_app!(state);
        // /exec without signature → unauthorized()
        let req = test::TestRequest::post().uri("/exec").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "unauthorized");
        assert!(body["message"].is_string());
    }

    #[ntex::test]
    async fn a4_draining_wire_shape() {
        let (state, _d) = make_state("a4_drain");
        state.mark_draining();
        let app = make_app!(state);
        // /tree with valid signature but draining → draining()
        let req = signed("GET", "/tree").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "draining");
        assert!(body["message"].is_string());
    }

    #[ntex::test]
    async fn a4_err_400_wire_shape() {
        // Malformed JSON body on /exec → err(400, ...) →
        // {error: "invalid_input", message: <prose>}.
        let (state, _d) = make_state("a4_err400");
        let app = make_app!(state);
        let req = signed_with_body("POST", "/exec", "not json")
            .header("content-type", "application/json")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "invalid_input");
        assert!(body["message"].as_str().unwrap().contains("invalid JSON"));
    }

    // ─── R9-T7: direct tests for sandbox_id init ────────────────────
    //
    // Coverage gap: prior tests only set the boot-time sandbox_id via
    // `test_set_sandbox_id` (which pokes the OnceLock directly), so
    // the env-var / file-fallback / empty-id branches inside
    // [`init_sandbox_id_from_env`] had zero exercise. We can't drive
    // [`init_sandbox_id_from_env`] for every branch because the
    // [`SANDBOX_ID`] OnceLock is write-once per process — once any
    // earlier test sets it, init short-circuits at the get() check.
    //
    // The pure parsing/fallback/empty-id slice was therefore factored
    // out of init into [`read_sandbox_id_from_sources`] (no behaviour
    // change — init still calls it with the production
    // `/run/keys/sandbox-id` fallback). These tests exercise that
    // helper directly with a temp fallback path, plus one
    // [`init_sandbox_id_from_env`] end-to-end test that verifies the
    // OnceLock-set wiring on a known-good input.
    //
    // **Serialization**: env-var mutation (`std::env::set_var`) is
    // process-global and not thread-safe. The crate doesn't carry
    // `serial_test` and shouldn't grow a dev-dep just for this; we
    // serialize via a module-local Mutex (same pattern as
    // `REAPER_STATE_LOCK` re-exported earlier in this module).

    use std::sync::Mutex as StdMutex;

    /// Serializes every test that touches `SANDBOX_AGENT_SANDBOX_ID`.
    /// Tests of [`read_sandbox_id_from_sources`] / [`init_sandbox_id_from_env`]
    /// all take this lock first.
    static SANDBOX_ID_ENV_LOCK: StdMutex<()> = StdMutex::new(());

    /// Save-and-restore RAII for the env var. Drop restores the prior
    /// value so a panicking test doesn't poison the global env for
    /// the rest of the suite.
    struct EnvGuard {
        prior: Option<String>,
    }
    impl EnvGuard {
        fn new() -> Self {
            let prior = std::env::var("SANDBOX_AGENT_SANDBOX_ID").ok();
            std::env::remove_var("SANDBOX_AGENT_SANDBOX_ID");
            Self { prior }
        }
        fn set(&self, v: &str) {
            std::env::set_var("SANDBOX_AGENT_SANDBOX_ID", v);
        }
        fn unset(&self) {
            std::env::remove_var("SANDBOX_AGENT_SANDBOX_ID");
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prior {
                Some(v) => std::env::set_var("SANDBOX_AGENT_SANDBOX_ID", v),
                None => std::env::remove_var("SANDBOX_AGENT_SANDBOX_ID"),
            }
        }
    }

    /// Path to a fallback file that does NOT exist on disk. Used by
    /// tests that want the file-read fork of
    /// [`read_sandbox_id_from_sources`] to fail.
    fn missing_fallback_path() -> std::path::PathBuf {
        unique_dir("noexist").join("does-not-exist")
    }

    #[test]
    fn r9t7_read_env_var_happy_path_32hex() {
        let _lock = SANDBOX_ID_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let env = EnvGuard::new();
        // typed_id "simple" form — 32 lowercase hex chars, no hyphens.
        let id = "01900000000070008000000000000001";
        env.set(id);
        let got = read_sandbox_id_from_sources(&missing_fallback_path().to_string_lossy())
            .expect("32-hex id should be accepted");
        assert_eq!(got, id, "id should round-trip from env var unchanged");
    }

    #[test]
    fn r9t7_read_env_var_empty_string_rejected() {
        let _lock = SANDBOX_ID_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let env = EnvGuard::new();
        // Setting to "" is observable as VarError::NotUnicode-free Ok("");
        // the empty-id guard MUST trip before any caller stores "".
        env.set("");
        let r = read_sandbox_id_from_sources(&missing_fallback_path().to_string_lossy());
        assert!(r.is_err(), "empty env var must be rejected");
        assert_eq!(r.unwrap_err(), "sandbox_id is empty");
    }

    #[test]
    fn r9t7_read_env_var_arbitrary_string_accepted_no_shape_guard() {
        // SURPRISE PINNED HERE. The task brief assumed a typed_id /
        // UUID shape guard inside the init path; reading the code
        // shows there is none — only an empty-id check. Any non-empty
        // string is accepted. We pin the actual behaviour so any
        // future shape-tightening change shows up as a test break
        // (forcing the author to update both the guard and this pin).
        let _lock = SANDBOX_ID_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let env = EnvGuard::new();
        env.set("not-a-uuid");
        let got = read_sandbox_id_from_sources(&missing_fallback_path().to_string_lossy())
            .expect("any non-empty string is currently accepted — see R9-T7-FOLLOWUP");
        assert_eq!(got, "not-a-uuid");
    }

    #[test]
    fn r9t7_read_env_var_hyphenated_uuid_accepted() {
        // The clock_resync test suite uses the hyphenated form
        // "01900000-0000-7000-8000-000000000000" via TEST_SANDBOX_ID,
        // so the controller-signed body's `sandbox_id` field is
        // expected to be hyphenated. Pin that the env-var read does
        // NOT normalise to .simple() — hyphens survive verbatim.
        let _lock = SANDBOX_ID_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let env = EnvGuard::new();
        let id = "01900000-0000-7000-8000-000000000000";
        env.set(id);
        let got = read_sandbox_id_from_sources(&missing_fallback_path().to_string_lossy())
            .expect("hyphenated UUID accepted");
        assert_eq!(got, id, "hyphens preserved — no .simple()/.hyphenated() normalisation");
    }

    #[test]
    fn r9t7_read_file_fallback_used_when_env_absent() {
        let _lock = SANDBOX_ID_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let env = EnvGuard::new();
        env.unset();
        // Write a valid id (with a trailing newline — wrappers
        // commonly use `echo "$id" > …`; the helper must trim it).
        let dir = unique_dir("r9t7_fb");
        let file = dir.join("sandbox-id");
        std::fs::write(&file, "01900000000070008000000000000002\n").unwrap();
        let got = read_sandbox_id_from_sources(&file.to_string_lossy())
            .expect("file-fallback read should succeed");
        assert_eq!(got, "01900000000070008000000000000002",
            "trailing newline must be trimmed");
    }

    #[test]
    fn r9t7_read_no_env_no_file_returns_err() {
        let _lock = SANDBOX_ID_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let env = EnvGuard::new();
        env.unset();
        let nowhere = missing_fallback_path();
        let r = read_sandbox_id_from_sources(&nowhere.to_string_lossy());
        let err = r.expect_err("no env + missing file must error");
        // Error message names BOTH sources so the operator can see
        // why init failed without grepping the binary for context.
        assert!(err.contains("SANDBOX_AGENT_SANDBOX_ID env unset"),
            "err names the env var: {err}");
        assert!(err.contains(nowhere.to_str().unwrap()),
            "err names the file path: {err}");
    }

    #[test]
    fn r9t7_read_file_fallback_empty_after_trim_rejected() {
        // Wrapper writes an empty file (or just a newline) → trimmed
        // to "" → empty-id guard trips. Distinct from the no-env
        // case because here the file EXISTS but yields nothing
        // usable; the helper must not silently accept "".
        let _lock = SANDBOX_ID_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let env = EnvGuard::new();
        env.unset();
        let dir = unique_dir("r9t7_emptyfile");
        let file = dir.join("sandbox-id");
        std::fs::write(&file, "\n").unwrap();
        let r = read_sandbox_id_from_sources(&file.to_string_lossy());
        assert_eq!(r.unwrap_err(), "sandbox_id is empty");
    }

    #[test]
    fn r9t7_init_sandbox_id_from_env_e2e_wires_to_oncelock() {
        // End-to-end: drives [`init_sandbox_id_from_env`] itself
        // (not the extracted helper) to confirm the OnceLock wiring
        // is intact. The OnceLock is process-global and likely
        // already set by an earlier clock_resync test that called
        // `ensure_test_sandbox_id`; in that case init short-circuits
        // and returns Ok without re-reading env. Either way the
        // post-condition is the same: SANDBOX_ID.get() is Some and
        // boot_sandbox_id() yields a non-empty string.
        let _lock = SANDBOX_ID_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let env = EnvGuard::new();
        env.set("01900000000070008000000000000003");
        let r = init_sandbox_id_from_env();
        assert!(r.is_ok(), "init should succeed (env-var or already-set fast path)");
        let id = boot_sandbox_id().expect("OnceLock must be populated post-init");
        assert!(!id.is_empty(), "boot_sandbox_id never returns an empty string");
    }
}
