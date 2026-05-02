//! HTTP-only proxy handler for the sandbox preview-URL feature.
//!
//! See `docs/proposals/sandbox-preview-urls.md` § II.1 (agent endpoint
//! `ANY /proxy/{port}/{path*}`) and § II.1.x (response-path header
//! rewriting).
//!
//! ## What this does
//!
//! 1. The controller signs an outbound request and forwards it to
//!    `<agent_url>/proxy/{port}/{path}?{query}`.
//! 2. ntex routes the request to [`proxy_http`].
//! 3. The route-level [`crate::handlers::verify_signed`] middleware
//!    has already verified the v1.1 canonical (path + query + body
//!    under the `ED25519-V1.1` domain tag); we re-check via the
//!    same call-site for symmetry with the other handlers.
//! 4. We dial `127.0.0.1:{port}` inside the VM via `ureq` (sync, run
//!    on the compio blocking pool so the ntex worker stays free).
//! 5. We collect the upstream response, apply the response-path
//!    header rewrites (Set-Cookie Domain strip, absolute Location +
//!    Refresh host rewrite), and forward the bytes back.
//!
//! ## What this is NOT
//!
//! - **WebSocket** — Phase 2 handles Upgrade. v1 ships HTTP-only.
//! - **Streaming** — for Phase 1 the agent buffers the upstream body
//!   before responding to the controller; Phase 2 reworks for a
//!   streaming pipe (see § II.1 "Body streaming").
//!
//! ## How the host-rewrite token gets here
//!
//! The agent doesn't know the public preview hostname (it lives in
//! a microVM with no platform context). The controller does. So the
//! controller forwards `X-Forwarded-Host: preview-{slug}-{port}.preview.zeroship.dev`
//! on the request; the agent reads the header on the response path
//! and uses it as the rewrite target. (The alternative — emitting a
//! placeholder token from the agent and substituting at the
//! controller — adds another rewrite stage with no benefit;
//! threading the host through `X-Forwarded-Host` is one less moving
//! part.) The header is request-scoped and never logged or echoed
//! to the user app.

use std::io::Read;
use std::time::Instant;

use ntex::http::header::HeaderName;
use ntex::http::{StatusCode, Uri};
use ntex::util::Bytes;
use ntex::web::{self, HttpRequest, HttpResponse};
use serde_json::json;

use crate::handlers::AppState;
use crate::sig::{self, CanonicalKind};
use zeroship_core::preview_ports::{is_proxyable_port, DEFAULT_DENY};

/// Hard cap on the request body for the proxy. The cap exists because
/// the canonical-string includes the body hash, so we have to buffer
/// the whole body to compute SHA-256 before we can verify the
/// signature. Larger uploads are steered through `zeroship.storage`
/// presigned URLs (see § VI R-2 of the design doc).
///
/// Default 100 MiB; configurable via `SANDBOX_AGENT_PROXY_MAX_BODY_BYTES`.
pub const DEFAULT_MAX_BODY_BYTES: usize = 100 * 1024 * 1024;

/// Per-request timeout for the agent → upstream dial + response.
const UPSTREAM_TIMEOUT_SECS: u64 = 30;

/// Hop-by-hop headers per RFC 7230 §6.1. Stripped from both the
/// request (before forwarding upstream) and the response (before
/// returning to the controller). Lower-case for ntex's case-insensitive
/// header lookup.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

/// `ANY /proxy/{port}/{path}*` — the agent-side proxy handler.
///
/// Authentication is verified inline (matches the pattern of the
/// other handlers); the v1.1 canonical is dispatched automatically
/// by [`crate::handlers::canonical_kind_for`].
pub async fn proxy_http(
    req: HttpRequest,
    state: web::types::State<AppState>,
    parts: web::types::Path<(u16, String)>,
    body: Bytes,
) -> HttpResponse {
    // Body cap (defense-in-depth — ntex's PayloadConfig also caps,
    // but at the top of the stack so an oversize request is read +
    // discarded instead of buffered fully). 413 is the spec response.
    let max_body = max_body_bytes();
    if body.len() > max_body {
        return err(StatusCode::PAYLOAD_TOO_LARGE, "payload too large");
    }

    if !crate::handlers::verify_signed_pub(&req, &body, &state) {
        return HttpResponse::Unauthorized().json(&json!({"error": "unauthorized"}));
    }

    if state.is_draining() {
        return HttpResponse::ServiceUnavailable().json(&json!({"error": "draining"}));
    }

    let (port, sub_path) = parts.into_inner();

    // Defense-in-depth port deny (controller also checks; agent is
    // the last line). The dynamic deny-list is not yet wired through
    // sandbox config in Phase 1 — DEFAULT_DENY ships everywhere.
    if !is_proxyable_port(port, DEFAULT_DENY) {
        return err(StatusCode::BAD_REQUEST, "port not allowed");
    }

    let method = req.method().clone();
    let query = req.uri().query().unwrap_or("");

    // Upstream URL: 127.0.0.1:{port}/{sub_path}?{query}
    //
    // Per § II.1 "Path normalization & smuggling defenses": the
    // sub_path is passed through raw (no URL-decode, no reencode, no
    // `..` collapsing). The byte string we sign is the byte string
    // we forward.
    let upstream_url = if query.is_empty() {
        format!("http://127.0.0.1:{port}/{sub_path}")
    } else {
        format!("http://127.0.0.1:{port}/{sub_path}?{query}")
    };

    // Read X-Forwarded-Host for response rewrites (Set-Cookie / Location).
    // The controller emits this; the user app inside the VM cannot —
    // its Host: header goes to its own dev server, not back here.
    let forwarded_host = req
        .headers()
        .get("x-forwarded-host")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let forwarded_proto = req
        .headers()
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("https")
        .to_string();

    // Build the outbound header list; strip hop-by-hop and the
    // X-Sbx-* headers (controller-internal trace IDs that the user
    // app should never see).
    let mut req_headers: Vec<(String, String)> = Vec::new();
    for (k, v) in req.headers().iter() {
        let kl = k.as_str().to_ascii_lowercase();
        if HOP_BY_HOP.contains(&kl.as_str()) {
            continue;
        }
        if kl.starts_with("x-sbx-") {
            continue;
        }
        // Don't echo X-Forwarded-Host upstream — the user app's `Host`
        // is the original upstream Host (loopback). § II.1.x request
        // path: pass `Host` through to upstream; XFH is informational
        // and most apps ignore it. We keep XFH transparent so frameworks
        // that DO rely on it (Express trust-proxy, Rails) get the
        // preview hostname; OK because we control the controller and
        // it's a trusted source of this header.
        if let Ok(vs) = v.to_str() {
            req_headers.push((k.as_str().to_string(), vs.to_string()));
        }
    }

    let logmeta = ProxyLogMeta {
        method: method.as_str().to_string(),
        port,
        path: sub_path.clone(),
        started: Instant::now(),
    };

    // ureq is sync; run on the compio blocking pool so we don't
    // hold an ntex worker for the duration of an upstream call.
    let body_bytes = body.to_vec();
    let upstream_url_clone = upstream_url.clone();
    let method_str = method.as_str().to_string();
    let result = compio::runtime::spawn_blocking(move || {
        forward_blocking(&method_str, &upstream_url_clone, &req_headers, &body_bytes)
    })
    .await;

    let (status, headers, body) = match result {
        Ok(Ok(triple)) => triple,
        Ok(Err(e)) => {
            logmeta.emit(0, 0);
            tracing::warn!(error = %e, port, path = %sub_path, "[sandbox-agent/proxy] upstream error");
            return err_with_code(StatusCode::BAD_GATEWAY, "upstream", &e);
        }
        Err(_join_panic) => {
            // The blocking pool's task panicked; the error payload is
            // a `Box<dyn Any>` (no Display) — we don't try to format it.
            logmeta.emit(0, 0);
            tracing::warn!("[sandbox-agent/proxy] blocking-pool join failure (panicked)");
            return err_with_code(
                StatusCode::BAD_GATEWAY,
                "upstream",
                "blocking-pool join failure",
            );
        }
    };

    // Apply response-path rewrites: Set-Cookie Domain strip, Location
    // and Refresh host rewrites. The rewrite target is the preview
    // hostname (from `X-Forwarded-Host`); if the controller didn't
    // provide one we still strip Set-Cookie Domain (defense-in-depth)
    // but pass Location/Refresh through.
    let preview_host = forwarded_host.as_deref();
    let preview_origin = preview_host.map(|h| format!("{forwarded_proto}://{h}"));
    let rewritten = rewrite_response_headers(&headers, preview_origin.as_deref());

    let mut resp = HttpResponse::build(status);
    for (k, v) in &rewritten {
        let kl = k.to_ascii_lowercase();
        if HOP_BY_HOP.contains(&kl.as_str()) {
            continue;
        }
        // Avoid clashing with the body length we set via .body(...);
        // ntex computes Content-Length from the buffered body. Drop
        // the upstream's Content-Length so we don't double-emit.
        if kl == "content-length" {
            continue;
        }
        // ntex's `header()` appends; `set_header()` overwrites. We
        // want append so multi-valued headers (Set-Cookie!) keep
        // their multiplicity and emission order.
        resp.header(k.as_str(), v.as_str());
    }

    let bytes_out = body.len();
    logmeta.emit(status.as_u16(), bytes_out);
    resp.body(body)
}

fn max_body_bytes() -> usize {
    #[cfg(test)]
    {
        let v = tests::TEST_BODY_CAP_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed);
        if v != 0 {
            return v;
        }
    }
    std::env::var("SANDBOX_AGENT_PROXY_MAX_BODY_BYTES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_BODY_BYTES)
}

fn err(status: StatusCode, msg: &str) -> HttpResponse {
    HttpResponse::build(status).json(&json!({"error": msg}))
}

fn err_with_code(status: StatusCode, code: &str, msg: &str) -> HttpResponse {
    HttpResponse::build(status).json(&json!({"error": msg, "code": code}))
}

/// Sync HTTP/1.1 forward. Returns (status, headers, body).
///
/// `headers` is a list (preserves duplicate header names — `Set-Cookie`
/// is the obvious case where order + multiplicity matter on the
/// response side; preserving on the request side is symmetric).
fn forward_blocking(
    method: &str,
    upstream_url: &str,
    req_headers: &[(String, String)],
    body: &[u8],
) -> Result<(StatusCode, Vec<(String, String)>, Vec<u8>), String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(UPSTREAM_TIMEOUT_SECS))
        // We're a transparent proxy — the BROWSER follows redirects,
        // not us. ureq's default of 5 would silently follow a 302
        // and the agent would forward the redirect target's body
        // back as if it were the original response.
        .redirects(0)
        .build();

    let mut req = agent.request(method, upstream_url);
    for (k, v) in req_headers {
        req = req.set(k, v);
    }

    let resp = match if body.is_empty() {
        req.call()
    } else {
        req.send_bytes(body)
    } {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => return Err(format!("dial upstream {upstream_url}: {e}")),
    };

    let status = StatusCode::from_u16(resp.status())
        .map_err(|e| format!("invalid upstream status {}: {e}", resp.status()))?;

    // Snapshot headers BEFORE consuming the body (ureq's Response
    // borrows the underlying connection for header introspection).
    // ureq exposes `header_names()` which iterates headers; for each
    // name we collect every value (multi-valued headers like
    // Set-Cookie). This preserves multiplicity and emission order.
    let mut headers: Vec<(String, String)> = Vec::new();
    for name in resp.headers_names() {
        // Set-Cookie is multi-valued; ureq's `all` returns each value.
        for value in resp.all(&name) {
            headers.push((name.clone(), value.to_string()));
        }
    }

    // Read the response body. Cap at the same body limit so a
    // hostile upstream can't OOM the agent.
    let mut buf = Vec::new();
    let max = max_body_bytes();
    let mut reader = resp.into_reader().take(max as u64 + 1);
    reader
        .read_to_end(&mut buf)
        .map_err(|e| format!("read upstream body: {e}"))?;
    if buf.len() > max {
        return Err(format!("upstream body exceeded cap of {max} bytes"));
    }

    Ok((status, headers, buf))
}

/// Apply the four response-path header transforms from § II.1.x:
///
/// - **Strip `Domain=` from every Set-Cookie value.** Multi-cookie
///   support (`Set-Cookie` is multi-valued); preserves emission
///   order. Handles the quoted-attribute form (`Domain="x"`).
/// - **Rewrite absolute `Location:` host+port** to the preview
///   origin if the URL is absolute and the host is an in-VM upstream
///   loopback (`localhost`, `127.0.0.1`, `0.0.0.0`, `[::1]`).
///   Preserve path+query+fragment byte-exactly. Relative Locations
///   pass through.
/// - **Rewrite `Refresh: 0; url=…`** the same way as Location.
/// - **Other headers pass through** unchanged.
///
/// `preview_origin` is `Some("https://preview-…")` when the
/// controller forwarded an `X-Forwarded-Host`; `None` if absent
/// (the rewrites that need the origin pass through; Set-Cookie
/// Domain strip still runs).
pub(crate) fn rewrite_response_headers(
    headers: &[(String, String)],
    preview_origin: Option<&str>,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::with_capacity(headers.len());
    for (k, v) in headers {
        let kl = k.to_ascii_lowercase();
        if kl == "set-cookie" {
            out.push((k.clone(), strip_cookie_domain(v)));
        } else if kl == "location" {
            out.push((k.clone(), rewrite_absolute_location(v, preview_origin)));
        } else if kl == "refresh" {
            out.push((k.clone(), rewrite_refresh_url(v, preview_origin)));
        } else {
            out.push((k.clone(), v.clone()));
        }
    }
    out
}

/// Strip the `Domain=...` attribute from a single Set-Cookie value.
///
/// Cookies are `name=value; attr1=v1; attr2; attr3="v3"`. The
/// rewriter splits on `;`, drops every segment whose key (case-
/// insensitively) is `domain`, and rejoins. Quoted attribute values
/// (`Domain="x"`) are handled by the per-segment scan — the entire
/// segment, including the quoted value, is dropped.
///
/// Per RFC 6265 §5.2 the cookie attribute names are case-insensitive;
/// values are not, but we don't need to interpret the value — only
/// detect the attribute key.
pub(crate) fn strip_cookie_domain(value: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in value.split(';') {
        let trimmed = part.trim_start();
        // The cookie pair `name=value` is the first segment; we never
        // strip it. Subsequent segments are attributes; we strip the
        // segment whose key is `domain`.
        let key_lower = trimmed
            .split_once('=')
            .map(|(k, _)| k.trim().to_ascii_lowercase())
            .unwrap_or_else(|| trimmed.trim().to_ascii_lowercase());
        if key_lower == "domain" {
            // Drop this segment entirely.
            continue;
        }
        parts.push(part);
    }
    // Rejoin preserving the original spacing semantics: each segment
    // is rejoined on `;` (no extra trim). This keeps the original
    // formatting (e.g. `; Path=/`) intact for the segments that
    // pass through.
    parts.join(";")
}

/// Rewrite an absolute `Location:` URL whose host is an in-VM upstream
/// loopback to `preview_origin`. Path + query + fragment preserved
/// byte-exact. Relative Locations or non-loopback hosts pass through.
pub(crate) fn rewrite_absolute_location(
    value: &str,
    preview_origin: Option<&str>,
) -> String {
    rewrite_absolute_url(value, preview_origin)
}

/// Rewrite the `url=...` attribute inside a `Refresh:` header value
/// (`0; url=http://localhost:5173/x`). Same loopback-host targeting
/// as `Location`.
pub(crate) fn rewrite_refresh_url(value: &str, preview_origin: Option<&str>) -> String {
    // Refresh = "<seconds>; url=<url>" (case-insensitive `url=`).
    // We split on `;` and rewrite the URL segment in place.
    let mut parts: Vec<String> = Vec::new();
    for part in value.split(';') {
        let trimmed = part.trim_start();
        if let Some(rest) = trim_prefix_ascii_ci(trimmed, "url=") {
            // Preserve leading whitespace (so `; url=` round-trips).
            let prefix_ws_len = part.len() - trimmed.len();
            let lead = &part[..prefix_ws_len];
            let rewritten = rewrite_absolute_url(rest, preview_origin);
            parts.push(format!("{lead}url={rewritten}"));
        } else {
            parts.push(part.to_string());
        }
    }
    parts.join(";")
}

/// Helper: strip an ASCII case-insensitive prefix.
fn trim_prefix_ascii_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() < prefix.len() {
        return None;
    }
    let head = &s.as_bytes()[..prefix.len()];
    if head.eq_ignore_ascii_case(prefix.as_bytes()) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

/// Core URL rewriter — used by both Location and Refresh url=.
fn rewrite_absolute_url(value: &str, preview_origin: Option<&str>) -> String {
    let v = value.trim();
    let lower = v.to_ascii_lowercase();
    let (scheme_len, _scheme) = if lower.starts_with("http://") {
        (7, "http")
    } else if lower.starts_with("https://") {
        (8, "https")
    } else {
        return value.to_string();
    };
    let after_scheme = &v[scheme_len..];
    // `host[:port]/path...` — find the path delimiter. The host runs
    // up to the first `/`, `?`, or `#`. If none, the whole string
    // is the host (rare, but possible: `http://localhost`).
    let path_start = after_scheme
        .find(|c| c == '/' || c == '?' || c == '#')
        .unwrap_or(after_scheme.len());
    let hostport = &after_scheme[..path_start];
    let tail = &after_scheme[path_start..];

    // Strip the optional port to test the host alone.
    let host_only = match hostport.rsplit_once(':') {
        // `[::1]:8080` — IPv6 in brackets. The rsplit hits the last `:`.
        // We accept either form and check the bracket-stripped host.
        Some((h, _p)) => h,
        None => hostport,
    };
    // Strip IPv6 brackets if present.
    let host_clean = host_only
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host_only);

    if !is_in_vm_loopback(host_clean) {
        return value.to_string();
    }
    let Some(origin) = preview_origin else {
        // No preview host known — pass through. The browser will
        // fail (loopback unreachable) but we don't have anywhere to
        // point it.
        return value.to_string();
    };
    // Compose: <preview_origin> + <tail>. Preserve the path+query+
    // fragment byte-exactly.
    if tail.is_empty() {
        // No path: emit `https://preview-…`. Most apps emit a path
        // (the redirect-to-`/` is the Vite default, with a trailing
        // slash); the no-path case is harmless.
        origin.to_string()
    } else {
        format!("{origin}{tail}")
    }
}

/// In-VM upstream loopback hosts. Matches the spec's enumeration:
/// `localhost`, `127.0.0.1`, `0.0.0.0`, `::1`.
fn is_in_vm_loopback(host: &str) -> bool {
    matches!(host.to_ascii_lowercase().as_str(), "localhost" | "127.0.0.1" | "0.0.0.0" | "::1")
}

/// Per-request log line emitter. Spec format:
/// `[sandbox-agent/proxy] req method=... port=... path=... status=... bytes=... elapsed_ms=...`
struct ProxyLogMeta {
    method: String,
    port: u16,
    path: String,
    started: Instant,
}

impl ProxyLogMeta {
    fn emit(&self, status: u16, bytes: usize) {
        let elapsed_ms = self.started.elapsed().as_millis();
        eprintln!(
            "[sandbox-agent/proxy] req method={} port={} path={} status={} bytes={} elapsed_ms={}",
            self.method, self.port, self.path, status, bytes, elapsed_ms,
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Compile-only references — keep imports legal where they're not
// used elsewhere in this module.

#[allow(dead_code)]
fn _ref_imports() {
    // sig and Uri / HeaderName aren't used in the public surface
    // (we use string headers throughout for ureq compatibility);
    // referenced here so a future rev that needs typed access can
    // pull them in without re-adding the import.
    let _ = sig::v1_1_path_query;
    let _: Option<HeaderName> = None;
    let _: Option<Uri> = None;
    let _: CanonicalKind = CanonicalKind::V1_1;
}

#[cfg(test)]
mod tests {
    use super::*;
    use ntex::http::StatusCode;
    use ntex::web::test;
    use std::io::{Read as IoRead, Write as IoWrite};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;

    /// Tests-only override for the body cap. Zero means "no override
    /// — fall through to env-var / default". Read by [`max_body_bytes`].
    /// Each test that uses it sets it at the top and resets to 0 at
    /// the end (or via a guard).
    pub(super) static TEST_BODY_CAP_OVERRIDE: AtomicUsize = AtomicUsize::new(0);

    /// RAII reset for the body-cap override.
    struct BodyCapGuard;
    impl Drop for BodyCapGuard {
        fn drop(&mut self) {
            TEST_BODY_CAP_OVERRIDE.store(0, Ordering::Relaxed);
        }
    }
    fn set_body_cap(cap: usize) -> BodyCapGuard {
        TEST_BODY_CAP_OVERRIDE.store(cap, Ordering::Relaxed);
        BodyCapGuard
    }

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);
    /// Same deterministic key the rest of the agent's test scaffold uses.
    const TEST_SK_BYTES: [u8; 32] = [42u8; 32];

    fn test_signing_key() -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&TEST_SK_BYTES)
    }

    fn unique_dir(label: &str) -> std::path::PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let p = std::env::temp_dir().join(format!("zsbx-proxytest-{label}-{pid}-{n}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn make_state(label: &str) -> AppState {
        use std::io::Write;
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
        AppState {
            verifier: Arc::new(verifier),
            workspace: Arc::new(workspace),
            draining: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            started_at_unix: 1234,
        }
    }

    fn sign_v1_1(method: &str, path_query: &str, body: &[u8]) -> (String, String, String) {
        static NONCE_COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = NONCE_COUNTER.fetch_add(1, Ordering::SeqCst);
        let nonce = format!("proxy-test-{}-{}", std::process::id(), n);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let sig = sig::sign_kind(
            CanonicalKind::V1_1,
            &test_signing_key(),
            method,
            path_query,
            body,
            ts,
            &nonce,
        );
        (ts.to_string(), nonce, sig)
    }

    /// Spawn a one-shot HTTP/1.1 fixture upstream on `127.0.0.1:0`.
    /// Returns (port, stop). The fixture replies with the caller-supplied
    /// status + body + extra headers verbatim. Lives long enough for
    /// each test to run a few requests through it.
    fn spawn_upstream(
        status: u16,
        extra_headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> (u16, Arc<AtomicBool>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        thread::spawn(move || {
            while !stop_clone.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(std::time::Duration::from_millis(200)))
                            .ok();
                        let mut buf = [0u8; 4096];
                        let _ = stream.read(&mut buf);
                        let mut hdrs = format!(
                            "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nConnection: close\r\n",
                            body.len()
                        );
                        for (k, v) in &extra_headers {
                            hdrs.push_str(&format!("{k}: {v}\r\n"));
                        }
                        hdrs.push_str("\r\n");
                        let _ = stream.write_all(hdrs.as_bytes());
                        let _ = stream.write_all(&body);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        (port, stop)
    }

    macro_rules! make_app {
        ($state:expr) => {
            test::init_service(
                ntex::web::App::new()
                    .state($state)
                    // Per-resource payload limit override — production
                    // main.rs sets this to DEFAULT_MAX_BODY_BYTES + slack;
                    // tests use 256 MiB so we can exercise the
                    // body-cap-413 path with a 101 MiB payload.
                    .state(
                        web::types::PayloadConfig::default()
                            .limit(256 * 1024 * 1024),
                    )
                    .service(
                        web::resource("/proxy/{port}/{path}*")
                            .state(
                                web::types::PayloadConfig::default()
                                    .limit(256 * 1024 * 1024),
                            )
                            .route(web::route().to(proxy_http)),
                    ),
            )
            .await
        };
    }

    // ─── Existing pure unit tests ─────────────────────────────────

    #[test]
    fn strip_cookie_domain_simple() {
        let v = "foo=bar; Domain=localhost; Path=/";
        // Path/spacing of remaining segments preserved.
        let out = strip_cookie_domain(v);
        assert!(!out.to_ascii_lowercase().contains("domain="));
        assert!(out.contains("foo=bar"));
        assert!(out.contains("Path=/"));
    }

    #[test]
    fn strip_cookie_domain_quoted_value() {
        // RFC 6265 doesn't permit quoted Domain= but the wild does.
        // The strip rule MUST handle the quoted form too.
        let v = "a=1; Domain=\"localhost\"; HttpOnly";
        let out = strip_cookie_domain(v);
        assert!(!out.to_ascii_lowercase().contains("domain"));
        assert!(out.contains("a=1"));
        assert!(out.contains("HttpOnly"));
    }

    #[test]
    fn strip_cookie_domain_case_insensitive() {
        // Attribute names are case-insensitive (RFC 6265 §5.2).
        let v = "a=1; DoMaIn=foo; Path=/";
        let out = strip_cookie_domain(v);
        assert!(!out.to_ascii_lowercase().contains("domain"));
    }

    #[test]
    fn strip_cookie_domain_no_attr_passthrough() {
        let v = "a=1; Path=/; HttpOnly";
        let out = strip_cookie_domain(v);
        // No Domain= present → output equals input (modulo no
        // structural change).
        assert_eq!(out, v);
    }

    #[test]
    fn strip_cookie_domain_preserves_value_equals() {
        // `name=value with =` should not get truncated.
        let v = "session=eyJ.abc=def; Domain=x; Path=/";
        let out = strip_cookie_domain(v);
        assert!(out.contains("session=eyJ.abc=def"));
        assert!(!out.to_ascii_lowercase().contains("domain"));
    }

    #[test]
    fn rewrite_location_loopback_to_preview() {
        let preview = "https://preview-abc-5173.preview.zeroship.dev";
        // Path + query + fragment preserved byte-exact.
        let inp = "http://127.0.0.1:5173/login?next=/dash#x";
        let out = rewrite_absolute_location(inp, Some(preview));
        assert_eq!(
            out,
            "https://preview-abc-5173.preview.zeroship.dev/login?next=/dash#x"
        );
    }

    #[test]
    fn rewrite_location_localhost_to_preview() {
        let preview = "https://preview-abc-5173.preview.zeroship.dev";
        let inp = "http://localhost:5173/foo?bar=baz#frag";
        let out = rewrite_absolute_location(inp, Some(preview));
        assert_eq!(
            out,
            "https://preview-abc-5173.preview.zeroship.dev/foo?bar=baz#frag"
        );
    }

    #[test]
    fn rewrite_location_zero_zero_zero_zero_to_preview() {
        let preview = "https://preview-abc-5173.preview.zeroship.dev";
        let inp = "http://0.0.0.0:5173/login";
        let out = rewrite_absolute_location(inp, Some(preview));
        assert_eq!(out, "https://preview-abc-5173.preview.zeroship.dev/login");
    }

    #[test]
    fn rewrite_location_relative_passthrough() {
        let preview = "https://preview-abc-5173.preview.zeroship.dev";
        for inp in ["/login", "./foo", "?q=1", "../../up"] {
            assert_eq!(rewrite_absolute_location(inp, Some(preview)), inp);
        }
    }

    #[test]
    fn rewrite_location_external_host_passthrough() {
        let preview = "https://preview-abc-5173.preview.zeroship.dev";
        let inp = "http://example.com/foo";
        assert_eq!(rewrite_absolute_location(inp, Some(preview)), inp);
    }

    #[test]
    fn rewrite_refresh_url_loopback() {
        let preview = "https://preview-abc-5173.preview.zeroship.dev";
        let inp = "0; url=http://localhost:5173/x";
        let out = rewrite_refresh_url(inp, Some(preview));
        assert!(out.contains("url=https://preview-abc-5173.preview.zeroship.dev/x"));
        assert!(out.starts_with("0;"));
    }

    #[test]
    fn rewrite_refresh_url_external_passthrough() {
        let preview = "https://preview-abc-5173.preview.zeroship.dev";
        let inp = "5; url=https://example.com/foo";
        let out = rewrite_refresh_url(inp, Some(preview));
        assert_eq!(out, inp);
    }

    #[test]
    fn rewrite_response_headers_multi_set_cookie_order() {
        // Multiple Set-Cookie headers MUST preserve emission order
        // (browser cookie-jar uses last-write-wins on equal name+path).
        let preview = "https://preview-abc-5173.preview.zeroship.dev";
        let inp = vec![
            ("Set-Cookie".to_string(), "a=1; Domain=localhost; Path=/".to_string()),
            ("X-Other".to_string(), "passthrough".to_string()),
            ("Set-Cookie".to_string(), "b=2; Domain=localhost; HttpOnly".to_string()),
        ];
        let out = rewrite_response_headers(&inp, Some(preview));
        // Order preserved.
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].0, "Set-Cookie");
        assert!(!out[0].1.to_ascii_lowercase().contains("domain"));
        assert!(out[0].1.contains("a=1"));
        assert_eq!(out[1].0, "X-Other");
        assert_eq!(out[1].1, "passthrough");
        assert_eq!(out[2].0, "Set-Cookie");
        assert!(!out[2].1.to_ascii_lowercase().contains("domain"));
        assert!(out[2].1.contains("b=2"));
    }

    #[test]
    fn rewrite_response_headers_no_preview_origin_strip_only() {
        // No X-Forwarded-Host → Set-Cookie still gets Domain stripped
        // (safe, defense-in-depth), but Location is left alone (we
        // have nowhere to point it).
        let inp = vec![
            ("Set-Cookie".to_string(), "a=1; Domain=localhost".to_string()),
            ("Location".to_string(), "http://localhost:5173/foo".to_string()),
        ];
        let out = rewrite_response_headers(&inp, None);
        assert!(!out[0].1.to_ascii_lowercase().contains("domain"));
        assert_eq!(out[1].1, "http://localhost:5173/foo");
    }

    #[test]
    fn loopback_predicate_matches() {
        for h in ["localhost", "LOCALHOST", "127.0.0.1", "0.0.0.0", "::1"] {
            assert!(is_in_vm_loopback(h), "host {h} should be loopback");
        }
        for h in ["example.com", "10.99.101.2", "1.2.3.4", "localhost.example.com"] {
            assert!(!is_in_vm_loopback(h), "host {h} should NOT be loopback");
        }
    }

    #[test]
    fn rewrite_location_preserves_path_query_fragment_bytes_exact() {
        // The byte-exactness invariant: weird path characters that
        // happen to be URL-safe MUST round-trip unchanged. We don't
        // decode anything.
        let preview = "https://preview-abc-5173.preview.zeroship.dev";
        let inp = "http://localhost:5173/foo%2Fbar%3F?a=%2F&b=&c=x#y%23z";
        let out = rewrite_absolute_location(inp, Some(preview));
        assert_eq!(
            out,
            "https://preview-abc-5173.preview.zeroship.dev/foo%2Fbar%3F?a=%2F&b=&c=x#y%23z"
        );
    }

    #[test]
    fn rewrite_response_headers_unknown_host_passthrough() {
        // Set-Cookie still rewrites; everything else passes through.
        let preview = "https://preview-abc-5173.preview.zeroship.dev";
        let inp = vec![
            ("Content-Type".to_string(), "text/html".to_string()),
            ("Cache-Control".to_string(), "no-store".to_string()),
        ];
        let out = rewrite_response_headers(&inp, Some(preview));
        assert_eq!(out, inp);
    }

    // ─── ntex-driven HTTP handler tests ───────────────────────────

    #[ntex::test]
    async fn proxy_http_unsigned_returns_401() {
        let state = make_state("unsigned");
        let app = make_app!(state);
        let req = test::TestRequest::get().uri("/proxy/5173/foo").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[ntex::test]
    async fn proxy_http_v1_canonical_for_proxy_path_rejected() {
        // CRITICAL-4 negative: a v1 canonical (used by /exec etc.)
        // sent against `/proxy/...` MUST 401. The dispatcher picks
        // v1.1 for /proxy/ and the v1 sig won't validate.
        let state = make_state("wrongkind");
        let app = make_app!(state);
        // Sign with v1 (no domain tag, no query in canonical).
        let path = "/proxy/5173/foo";
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let nonce = "v1-on-proxy";
        let v1_sig = crate::sig::sign(&test_signing_key(), "GET", path, b"", ts, nonce);
        let req = test::TestRequest::get()
            .uri(path)
            .header("x-sbx-timestamp", ts.to_string())
            .header("x-sbx-nonce", nonce)
            .header("x-sbx-signature", v1_sig)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// Find a TCP port that's NOT listening (bind, capture port, drop).
    /// Race-y in theory; fine in practice for unit tests.
    fn unused_port() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    }

    #[ntex::test]
    async fn proxy_http_path_traversal_lands_on_proxy_handler_not_exec() {
        // CRITICAL-4: a request to `/proxy/<unused>/%2e%2e%2fexec`
        // MUST route to proxy_http (which 502s — no upstream
        // listening on the chosen port) and NEVER reach `/exec`.
        // The make_app here doesn't even register `/exec`, so a
        // routing leak would surface as 404 from the default
        // handler. We assert the status is 502 (upstream connect
        // refused) — proof the handler ran.
        let state = make_state("trav");
        let app = make_app!(state);
        let port = unused_port();
        let raw = format!("/proxy/{port}/%2e%2e%2fexec");
        let (ts, nonce, sig) = sign_v1_1("GET", &raw, b"");
        let req = test::TestRequest::get()
            .uri(&raw)
            .header("x-sbx-timestamp", ts)
            .header("x-sbx-nonce", nonce)
            .header("x-sbx-signature", sig)
            .to_request();
        let resp = test::call_service(&app, req).await;
        // We MUST hit the proxy handler. The handler dials a port
        // that isn't listening → 502.
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[ntex::test]
    async fn proxy_http_port_deny_22() {
        // SSH port — hard-coded deny.
        let state = make_state("port22");
        let app = make_app!(state);
        let path = "/proxy/22/x";
        let (ts, nonce, sig) = sign_v1_1("GET", path, b"");
        let req = test::TestRequest::get()
            .uri(path)
            .header("x-sbx-timestamp", ts)
            .header("x-sbx-nonce", nonce)
            .header("x-sbx-signature", sig)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[ntex::test]
    async fn proxy_http_port_deny_9229_v8_inspector() {
        let state = make_state("port9229");
        let app = make_app!(state);
        let path = "/proxy/9229/json";
        let (ts, nonce, sig) = sign_v1_1("GET", path, b"");
        let req = test::TestRequest::get()
            .uri(path)
            .header("x-sbx-timestamp", ts)
            .header("x-sbx-nonce", nonce)
            .header("x-sbx-signature", sig)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[ntex::test]
    async fn proxy_http_port_deny_agent_self_loop() {
        // The agent's own port 7777 — anti-loop.
        let state = make_state("port7777");
        let app = make_app!(state);
        let path = "/proxy/7777/version";
        let (ts, nonce, sig) = sign_v1_1("GET", path, b"");
        let req = test::TestRequest::get()
            .uri(path)
            .header("x-sbx-timestamp", ts)
            .header("x-sbx-nonce", nonce)
            .header("x-sbx-signature", sig)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[ntex::test]
    async fn proxy_http_body_cap_413() {
        // Use a tiny cap injected via TEST_BODY_CAP_OVERRIDE so the
        // test exercises the 413 path without allocating the full
        // 100 MiB. Production reads from the env var and falls back
        // to DEFAULT_MAX_BODY_BYTES; the override is test-only.
        let _g = set_body_cap(1024);
        let state = make_state("bodycap");
        let app = make_app!(state);
        let path = "/proxy/5173/upload";
        let body = vec![b'a'; 1025]; // 1 byte over the 1024-byte cap.
        let (ts, nonce, sig) = sign_v1_1("PUT", path, &body);
        let req = test::TestRequest::put()
            .uri(path)
            .header("x-sbx-timestamp", ts)
            .header("x-sbx-nonce", nonce)
            .header("x-sbx-signature", sig)
            .set_payload(body)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[ntex::test]
    async fn proxy_http_200_ok_round_trip() {
        // Boot a fixture upstream that echoes a small body.
        let body = b"hello-from-upstream".to_vec();
        let (port, stop) = spawn_upstream(200, vec![], body.clone());

        let state = make_state("ok");
        let app = make_app!(state);
        let path = format!("/proxy/{port}/index.html");
        let (ts, nonce, sig) = sign_v1_1("GET", &path, b"");
        let req = test::TestRequest::get()
            .uri(&path)
            .header("x-sbx-timestamp", ts)
            .header("x-sbx-nonce", nonce)
            .header("x-sbx-signature", sig)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = test::read_body(resp).await;
        assert_eq!(&bytes[..], &body[..]);
        stop.store(true, Ordering::Relaxed);
    }

    #[ntex::test]
    async fn proxy_http_503_passthrough() {
        let (port, stop) = spawn_upstream(503, vec![], b"down".to_vec());
        let state = make_state("503");
        let app = make_app!(state);
        let path = format!("/proxy/{port}/health");
        let (ts, nonce, sig) = sign_v1_1("GET", &path, b"");
        let req = test::TestRequest::get()
            .uri(&path)
            .header("x-sbx-timestamp", ts)
            .header("x-sbx-nonce", nonce)
            .header("x-sbx-signature", sig)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        stop.store(true, Ordering::Relaxed);
    }

    #[ntex::test]
    async fn proxy_http_set_cookie_domain_stripped_on_response() {
        // Upstream sets `Domain=localhost`; the agent strips it.
        let extra = vec![(
            "Set-Cookie".to_string(),
            "foo=bar; Domain=localhost; Path=/".to_string(),
        )];
        let (port, stop) = spawn_upstream(200, extra, b"".to_vec());
        let state = make_state("setcookie");
        let app = make_app!(state);
        let path = format!("/proxy/{port}/x");
        let (ts, nonce, sig) = sign_v1_1("GET", &path, b"");
        let req = test::TestRequest::get()
            .uri(&path)
            .header("x-sbx-timestamp", ts)
            .header("x-sbx-nonce", nonce)
            .header("x-sbx-signature", sig)
            .header(
                "x-forwarded-host",
                "preview-abc-5173.preview.zeroship.dev",
            )
            .to_request();
        let resp = test::call_service(&app, req).await;
        let h = resp.headers();
        let sc = h.get("set-cookie").expect("Set-Cookie present");
        let s = sc.to_str().unwrap();
        assert!(!s.to_ascii_lowercase().contains("domain"));
        assert!(s.contains("foo=bar"));
        stop.store(true, Ordering::Relaxed);
    }

    #[ntex::test]
    async fn proxy_http_location_loopback_rewritten() {
        let extra = vec![(
            "Location".to_string(),
            "http://localhost:5173/login?next=/dash#x".to_string(),
        )];
        let (port, stop) = spawn_upstream(302, extra, b"".to_vec());
        let state = make_state("loc");
        let app = make_app!(state);
        let path = format!("/proxy/{port}/foo");
        let (ts, nonce, sig) = sign_v1_1("GET", &path, b"");
        let req = test::TestRequest::get()
            .uri(&path)
            .header("x-sbx-timestamp", ts)
            .header("x-sbx-nonce", nonce)
            .header("x-sbx-signature", sig)
            .header(
                "x-forwarded-host",
                "preview-abc-5173.preview.zeroship.dev",
            )
            .header("x-forwarded-proto", "https")
            .to_request();
        let resp = test::call_service(&app, req).await;
        let loc = resp.headers().get("location").unwrap().to_str().unwrap();
        assert_eq!(
            loc,
            "https://preview-abc-5173.preview.zeroship.dev/login?next=/dash#x"
        );
        stop.store(true, Ordering::Relaxed);
    }

    #[ntex::test]
    async fn proxy_http_refresh_url_rewritten() {
        let extra = vec![(
            "Refresh".to_string(),
            "0; url=http://localhost:5173/x".to_string(),
        )];
        let (port, stop) = spawn_upstream(200, extra, b"".to_vec());
        let state = make_state("refresh");
        let app = make_app!(state);
        let path = format!("/proxy/{port}/y");
        let (ts, nonce, sig) = sign_v1_1("GET", &path, b"");
        let req = test::TestRequest::get()
            .uri(&path)
            .header("x-sbx-timestamp", ts)
            .header("x-sbx-nonce", nonce)
            .header("x-sbx-signature", sig)
            .header(
                "x-forwarded-host",
                "preview-abc-5173.preview.zeroship.dev",
            )
            .to_request();
        let resp = test::call_service(&app, req).await;
        let r = resp.headers().get("refresh").unwrap().to_str().unwrap();
        assert!(r.contains("https://preview-abc-5173.preview.zeroship.dev/x"));
        stop.store(true, Ordering::Relaxed);
    }

    #[ntex::test]
    async fn proxy_http_relative_location_passthrough() {
        let extra = vec![("Location".to_string(), "/login".to_string())];
        let (port, stop) = spawn_upstream(302, extra, b"".to_vec());
        let state = make_state("rellocation");
        let app = make_app!(state);
        let path = format!("/proxy/{port}/foo");
        let (ts, nonce, sig) = sign_v1_1("GET", &path, b"");
        let req = test::TestRequest::get()
            .uri(&path)
            .header("x-sbx-timestamp", ts)
            .header("x-sbx-nonce", nonce)
            .header("x-sbx-signature", sig)
            .header(
                "x-forwarded-host",
                "preview-abc-5173.preview.zeroship.dev",
            )
            .to_request();
        let resp = test::call_service(&app, req).await;
        let loc = resp.headers().get("location").unwrap().to_str().unwrap();
        assert_eq!(loc, "/login");
        stop.store(true, Ordering::Relaxed);
    }

    #[ntex::test]
    async fn proxy_http_body_tamper_rejected() {
        // Sign for body-A, send body-B → 401. Same shape as the /exec
        // tamper test, but exercising the v1.1 canonical via /proxy.
        let (port, stop) = spawn_upstream(200, vec![], b"".to_vec());
        let state = make_state("tamper");
        let app = make_app!(state);
        let path = format!("/proxy/{port}/api");
        let body_signed = b"expected payload";
        let body_sent = b"different payload";
        let (ts, nonce, sig) = sign_v1_1("POST", &path, body_signed);
        let req = test::TestRequest::post()
            .uri(&path)
            .header("x-sbx-timestamp", ts)
            .header("x-sbx-nonce", nonce)
            .header("x-sbx-signature", sig)
            .set_payload(body_sent.to_vec())
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        stop.store(true, Ordering::Relaxed);
    }
}
