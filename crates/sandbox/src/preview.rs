//! Controller-side preview proxy: `ANY /sandboxes/{id}/preview/{port}/{path*}`.
//!
//! See `docs/proposals/sandbox-preview-urls.md` § II.2 (controller
//! endpoint) and § II.1 (auth canonical).
//!
//! ## What this does
//!
//! 1. **Authenticate** — bearer creator-token; failure → 401 (uniform,
//!    does NOT vary by sandbox existence — round-6 H4).
//! 2. **Authorize** — the single coalesced gate
//!    [`authorize`]: principal must be present, the sandbox must
//!    exist, the principal must own it, the port must be in the
//!    proxyable allow-set. Any failure → 404 (uniform; audit-log
//!    records the actual reason).
//! 3. **Look up `SandboxAuth`** via `state.backend.session_auth(id)`.
//! 4. **Sign + forward** to `<agent_url>/proxy/{port}/{sub_path}?{query}`
//!    via ureq inside `compio::runtime::spawn_blocking`. Each retry
//!    mints a FRESH `(ts, nonce, sig)` per § II.1 "Retry semantics".
//! 5. **Forward response** with the agent's headers passed through
//!    (the agent already applied the response-path rewrites — see
//!    `proxy.rs` in the agent crate — so we trust them and emit
//!    verbatim).
//!
//! ## Phase 1 simplifications
//!
//! - **Bearer token auth only.** No share tokens (Phase 3).
//! - **No public DNS yet.** The preview hostname pattern
//!   `preview-{slug}-{port}.preview.zeroship.dev` is computed and
//!   forwarded as `X-Forwarded-Host` so the agent's response
//!   rewriter knows where to point absolute Locations / Refresh
//!   URLs, even though no DNS is provisioned yet (Phase 4).
//! - **No streaming, no WebSocket, no circuit breaker.** Phase 2.
//! - **`X-ZSPreview-Host` from inbound is dropped.** A creator
//!   couldn't override the rewrite target via a request header.
//!
//! ## What we sign
//!
//! The canonical-string is v1.1: `ED25519-V1.1\nMETHOD\n<path-and-query>\n
//! ts\nnonce\nsha256_hex(body)`. The agent inspects `req.path()` and
//! picks v1.1 when the path starts with `/proxy/`; that path is the
//! same byte string we sign here, so the dispatcher and the canonical
//! choice line up byte-for-byte (round-6 § II.1 CRITICAL-4).

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ntex::http::StatusCode;
use ntex::util::Bytes;
use ntex::web::{self, HttpRequest, HttpResponse};
use serde_json::json;
use uuid::Uuid;

use crate::auth;
use crate::preview_share::{
    self, validate_token, RegistrySecretLookup, TokenClaims, TokenError, TOKEN_RAW_MAX,
};
use crate::AppState;
use zeroship_core::preview_ports::{is_proxyable_port, DEFAULT_DENY};
use zeroship_sandbox_agent::sig::{self, CanonicalKind};

type State = web::types::State<Arc<AppState>>;

/// Hard cap on the request body for the preview proxy. Mirrors the
/// agent-side default (100 MiB) since both sides buffer.
pub const DEFAULT_MAX_BODY_BYTES: usize = 100 * 1024 * 1024;

/// Per-request timeout for the controller → agent leg.
const AGENT_TIMEOUT_SECS: u64 = 30;

/// Hop-by-hop headers (RFC 7230 §6.1). Stripped before signing
/// outbound and before relaying the response.
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

/// Authenticated principal.
#[derive(Debug, Clone)]
pub enum Principal {
    /// Creator session — the creator's bearer token plus their
    /// `?user_id=` claim. Authorisation matches against the sandbox
    /// record's `user_id`.
    Creator { user_id: String },
    /// Phase-3 share-token principal. The HMAC + claims have already
    /// been validated against the path-bound `(sandbox_id, port,
    /// method)`; the `authorize` re-check is belt-and-suspenders.
    ShareToken { claims: TokenClaims },
}

/// `ANY /sandboxes/{id}/preview/{port}/{path*}` — controller-side
/// preview forwarder.
pub async fn preview_proxy(
    req: HttpRequest,
    state: State,
    path: web::types::Path<(String, u16, String)>,
    body: Bytes,
) -> HttpResponse {
    // 1. Body cap. The whole body has to be buffered to compute its
    // SHA-256 before signing — see § II.1 "Body streaming". Reject
    // before any signature work.
    if body.len() > max_body_bytes() {
        return uniform_413();
    }

    let (sandbox_id_str, port, sub_path) = path.into_inner();
    let sandbox_id_opt: Option<Uuid> = sandbox_id_str.parse().ok();

    // 2. Cookie-conversion: if the request carries `?t=<token>`, this
    //    is the share-token first-hit. Validate, set `__Host-zsbx_share_<sbx>`,
    //    303-redirect to the same path with `?t=` stripped (round-6
    //    `__zsbx_share` flow). On both `?t=` AND cookie present, `?t=`
    //    wins (§ II.4 "If `?t=` is present AND the cookie is also
    //    present: validate both; the `?t=` wins"). Sec-Fetch CSRF
    //    guard fires on this path.
    if let Some(raw_token) = query_param(&req, "t") {
        return handle_cookie_conversion(
            &req,
            &state,
            &sandbox_id_str,
            sandbox_id_opt,
            port,
            &raw_token,
        );
    }

    // 3. Authenticate. Try the share-token cookie FIRST (cheaper —
    //    no header allocation), fall through to creator bearer.
    //    Both paths fail closed on bad input. Uniform 401 with NO
    //    sandbox-existence oracle (round-6 H4).
    let principal_opt = authenticate(&req, &state, sandbox_id_opt, port);

    // Coalesced check: principal-Some + uuid parsed + info-Some +
    // ownership-match + port-allowed. Anything failing produces an
    // identical 404 on the wire (we audit-log the actual reason).
    let info_opt = sandbox_id_opt.and_then(|id| state.sandboxes.get(&id));
    let port_allowed = is_proxyable_port(port, DEFAULT_DENY);

    let authorized = match (&principal_opt, &info_opt, port_allowed) {
        (Some(p), Some(info), true) => {
            authorize_with_method(p, info, port, req.method().as_str())
        }
        _ => false,
    };

    if !authorized {
        let reason = match (&principal_opt, &info_opt, port_allowed) {
            (None, _, _) => "auth-failed",
            (_, None, _) => "sandbox-not-found",
            (_, _, false) => "port-denied",
            (Some(_), Some(_), true) => "not-owner",
        };
        eprintln!(
            "[sandbox/preview] authorize-failed sandbox_id={sandbox_id_str} \
             port={port} reason={reason}"
        );
        // 401 if we can't even authenticate (so the creator gets a
        // login prompt); else 404 (uniform, no existence oracle).
        if principal_opt.is_none() {
            return uniform_401();
        }
        return uniform_404();
    }

    // Audit-log every accepted share-token use so `GET /share` shows
    // updated last_used / use_count.
    if let (Some(Principal::ShareToken { claims }), Some(id)) =
        (&principal_opt, sandbox_id_opt)
    {
        state.sandboxes.record_audit_use(id, &claims.tid);
    }

    // 3. Resolve agent_url + signing_key for this sandbox.
    let auth_bundle = match state.backend.session_auth(sandbox_id_opt.unwrap()).await {
        Ok(a) => a,
        Err(e) => {
            eprintln!(
                "[sandbox/preview] session_auth failed sandbox_id={sandbox_id_str} \
                 port={port} error={e}"
            );
            // The sandbox passed the registry check but the backend
            // doesn't know it — the runtime is stale. Surface as
            // 502; the creator UI maps to "agent_unreachable".
            return err_with_code(StatusCode::BAD_GATEWAY, "agent_unreachable");
        }
    };

    // 4. Build outbound request.
    let outbound_path = format!("/proxy/{port}/{sub_path}");
    let query = req.uri().query().unwrap_or("");
    let path_query = if query.is_empty() {
        outbound_path.clone()
    } else {
        format!("{outbound_path}?{query}")
    };

    // 5. Compose the preview-host string. No public DNS yet (Phase 4),
    // but we still emit the deterministic pattern as `X-Forwarded-Host`
    // so the agent's response-rewriter knows where to point absolute
    // Locations.
    let info = info_opt.unwrap();
    let preview_host = compute_preview_host(&info.sandbox_id, port);

    // 6. Mint fresh (ts, nonce). `sign_kind` builds the v1.1 canonical
    // (path + query, ED25519-V1.1 tag). Per § II.1 retry-semantics,
    // each retry would mint a fresh tuple; Phase 1 has no retry.
    let ts = unix_now();
    let nonce = mint_nonce();
    let signature = sig::sign_kind(
        CanonicalKind::V1_1,
        &auth_bundle.signing_key,
        req.method().as_str(),
        &path_query,
        &body,
        ts,
        &nonce,
    );

    // 7. Build the request-header list:
    //    - drop hop-by-hop, X-Sbx-* (we re-sign the request fresh
    //      with our own X-Sbx-*), and X-ZSPreview-Host (sanitize: the
    //      client must not control the rewrite target).
    //    - add X-Forwarded-Host (the preview hostname), -Proto,
    //      -For (the immediate client IP).
    //    - add the three X-Sbx-* signature headers.
    let mut req_headers: Vec<(String, String)> = Vec::new();
    for (k, v) in req.headers().iter() {
        let kl = k.as_str().to_ascii_lowercase();
        if HOP_BY_HOP.contains(&kl.as_str()) {
            continue;
        }
        if kl.starts_with("x-sbx-") {
            // Caller's X-Sbx-* would otherwise overwrite our fresh
            // signature in the agent's view. Drop.
            continue;
        }
        if kl == "x-zspreview-host" {
            // Round-6: the rewrite target MUST NOT be client-controlled.
            continue;
        }
        if kl == "x-forwarded-host"
            || kl == "x-forwarded-proto"
            || kl == "x-forwarded-for"
        {
            // We set our own; drop the inbound to avoid double-emission.
            continue;
        }
        if let Ok(vs) = v.to_str() {
            req_headers.push((k.as_str().to_string(), vs.to_string()));
        }
    }
    req_headers.push(("X-Forwarded-Host".into(), preview_host.clone()));
    req_headers.push(("X-Forwarded-Proto".into(), "https".into()));
    if let Some(ip) = req
        .connection_info()
        .remote()
        .map(|s| s.split(':').next().unwrap_or(s).to_string())
    {
        req_headers.push(("X-Forwarded-For".into(), ip));
    }
    req_headers.push(("X-Sbx-Timestamp".into(), ts.to_string()));
    req_headers.push(("X-Sbx-Nonce".into(), nonce.clone()));
    req_headers.push(("X-Sbx-Signature".into(), signature));

    // 8. Forward via ureq on the blocking pool.
    let agent_url = auth_bundle.agent_url.clone();
    let url = format!("{agent_url}{path_query}");
    let method = req.method().as_str().to_string();
    let body_vec = body.to_vec();
    let result = compio::runtime::spawn_blocking(move || {
        forward_blocking(&method, &url, &req_headers, &body_vec)
    })
    .await;

    let (status, headers, body_out) = match result {
        Ok(Ok(triple)) => triple,
        Ok(Err(e)) => {
            eprintln!(
                "[sandbox/preview] agent forward error sandbox_id={sandbox_id_str} \
                 port={port} error={e}"
            );
            return err_with_code(StatusCode::BAD_GATEWAY, "agent_unreachable");
        }
        Err(_join_panic) => {
            eprintln!(
                "[sandbox/preview] blocking-pool join failure sandbox_id={sandbox_id_str}"
            );
            return err_with_code(StatusCode::BAD_GATEWAY, "agent_unreachable");
        }
    };

    // 9. Emit response. The agent has already applied the
    //    response-path rewrites (Set-Cookie Domain strip, Location /
    //    Refresh host rewrite); we just pass headers through. Strip
    //    hop-by-hop and Content-Length (ntex computes that from the
    //    buffered body).
    let mut resp = HttpResponse::build(status);
    for (k, v) in &headers {
        let kl = k.to_ascii_lowercase();
        if HOP_BY_HOP.contains(&kl.as_str()) {
            continue;
        }
        if kl == "content-length" {
            continue;
        }
        resp.header(k.as_str(), v.as_str());
    }
    resp.body(body_out)
}

fn max_body_bytes() -> usize {
    #[cfg(test)]
    {
        let v = TEST_BODY_CAP_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed);
        if v != 0 {
            return v;
        }
    }
    std::env::var("SANDBOX_PREVIEW_MAX_BODY_BYTES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_BODY_BYTES)
}

#[cfg(test)]
pub(crate) static TEST_BODY_CAP_OVERRIDE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Mint a fresh nonce. Format mirrors the agent's tests:
/// `pid-counter-nanos` — short, ASCII alphanumeric + `-`, deterministically
/// unique across this process.
fn mint_nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("preview-{pid}-{n}-{nanos}")
}

/// Compute the preview hostname. No public DNS yet; the value is
/// emitted as `X-Forwarded-Host` so the agent's response rewriter
/// knows where to point absolute Locations / Refresh URLs.
///
/// Pattern: `preview-{slug}-{port}.preview.zeroship.dev` where
/// `{slug}` is the sandbox-id stripped to ASCII-lowercase
/// alphanumeric + `-`. Phase 4 lands real DNS for this.
fn compute_preview_host(sandbox_id: &str, port: u16) -> String {
    // Sandbox IDs are UUIDs (see registry); we lowercase + drop dashes
    // for a compact slug. Subdomain RFC permits up to 63 chars.
    let slug: String = sandbox_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    format!("preview-{slug}-{port}.preview.zeroship.dev")
}

/// Authenticate the request. Either:
///
/// 1. A creator's bearer token + `?user_id=<id>` claim → `Principal::Creator`.
/// 2. A `__Host-zsbx_share_<sbx>` cookie carrying a share-token →
///    `Principal::ShareToken`.
///
/// Returns `None` if neither path validates. The bearer check fails
/// closed; the cookie path runs full HMAC + claims validation
/// (matches the `?t=` path's validator — round-6 CRITICAL-3 cookie-
/// after-revoke invariant).
fn authenticate(
    req: &HttpRequest,
    state: &AppState,
    sandbox_id_opt: Option<Uuid>,
    port: u16,
) -> Option<Principal> {
    // Try share-token cookie first. The cookie's name is
    // `__Host-zsbx_share_<sbx>`; we look up the sandbox slug from the
    // sandbox-id passed by the route. Validation runs against the
    // sandbox's per-sandbox secret ring AND scopes the method.
    if let Some(id) = sandbox_id_opt {
        if let Some(claims) = try_share_cookie(req, state, id, port) {
            return Some(Principal::ShareToken { claims });
        }
    }

    // Fall through to creator bearer auth.
    if !auth::check(req, state) {
        return None;
    }
    let user_id = query_param(req, "user_id")?;
    if user_id.is_empty() {
        return None;
    }
    Some(Principal::Creator { user_id })
}

/// Look up `__Host-zsbx_share_<sbx>` and validate the carried token.
/// Returns `Some(claims)` only on a fully-valid HMAC + claims check
/// against the request's `(sandbox_id, port, method)` triple. Any
/// failure → `None` (the caller falls through to bearer auth).
fn try_share_cookie(
    req: &HttpRequest,
    state: &AppState,
    sandbox_id: Uuid,
    port: u16,
) -> Option<TokenClaims> {
    let cookie_name = format!("__Host-zsbx_share_{}", sandbox_slug(sandbox_id));
    let raw_cookie_header = req.headers().get("cookie")?.to_str().ok()?;
    let token = parse_cookie_value(raw_cookie_header, &cookie_name)?;
    if token.len() > TOKEN_RAW_MAX {
        return None;
    }
    let lookup = RegistrySecretLookup {
        registry: &state.sandboxes,
        sandbox_id,
    };
    validate_token(
        &token,
        &sandbox_id.to_string(),
        port,
        req.method().as_str(),
        unix_now(),
        &lookup,
    )
    .ok()
}

/// Cookie-conversion handler. § II.4 + § II.6:
///
/// - Validate the token from `?t=<token>`.
/// - On success: 303 redirect to the same URL minus `?t=`, with
///   `Set-Cookie: __Host-zsbx_share_<sbx>=<token>; HttpOnly; Secure;
///   SameSite=Strict; Path=/` and `Clear-Site-Data: "cache"`.
/// - On failure: uniform 401.
///
/// Sec-Fetch-* fail-closed (§ II.6 round-6 H6): a request without
/// Sec-Fetch-Site is rejected with `code: "client_too_old"`.
fn handle_cookie_conversion(
    req: &HttpRequest,
    state: &AppState,
    sandbox_id_str: &str,
    sandbox_id_opt: Option<Uuid>,
    port: u16,
    raw_token: &str,
) -> HttpResponse {
    // Sec-Fetch CSRF gate. Three required headers; missing ANY
    // triggers `client_too_old`. Top-level navigation only.
    if let Err(resp) = check_sec_fetch(req) {
        return resp;
    }
    if raw_token.len() > TOKEN_RAW_MAX {
        return uniform_401();
    }
    let id = match sandbox_id_opt {
        Some(id) => id,
        None => return uniform_401(),
    };
    let lookup = RegistrySecretLookup {
        registry: &state.sandboxes,
        sandbox_id: id,
    };
    let claims = match validate_token(
        raw_token,
        &id.to_string(),
        port,
        req.method().as_str(),
        unix_now(),
        &lookup,
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "[sandbox/preview] __zsbx_share token validation failed: {:?}",
                e
            );
            // Specifically surface "expired"/"revoked" so the AI builder UI
            // can render a tailored error; everything else collapses to 401.
            return token_error_response(e);
        }
    };

    // Record audit use BEFORE emitting the redirect — the conversion
    // counts as one use of the token.
    state.sandboxes.record_audit_use(id, &claims.tid);

    // Build the same path with `?t=` stripped. Other query params are
    // preserved (rare in practice, but if a user pastes
    // `?t=...&foo=bar`, the `foo=bar` survives the redirect).
    let stripped_query = strip_t_query(req.uri().query().unwrap_or(""));
    let path = req.path().to_string();
    let location = if stripped_query.is_empty() {
        path
    } else {
        format!("{path}?{stripped_query}")
    };

    let cookie_name = format!("__Host-zsbx_share_{}", sandbox_slug(id));
    // `__Host-` requires Secure AND Path=/ AND no Domain attribute
    // (RFC 6265bis). SameSite=Strict for share-cookie (§ II.6 byte-
    // exact contract).
    let set_cookie = format!(
        "{cookie_name}={raw_token}; HttpOnly; Secure; SameSite=Strict; Path=/"
    );

    HttpResponse::SeeOther()
        .header("Location", location.as_str())
        .header("Set-Cookie", set_cookie.as_str())
        .header("Cache-Control", "no-store")
        .header("Clear-Site-Data", "\"cache\"")
        .header("Referrer-Policy", "no-referrer")
        .json(&json!({"ok": true, "sandbox_id": sandbox_id_str, "port": port}))
}

/// Sec-Fetch-* enforcement (§ II.6 + round-6 H6 — fail-closed). The
/// `__zsbx_share` cookie-conversion is gated on a top-level
/// navigation: `Sec-Fetch-Mode: navigate` AND `Sec-Fetch-Dest:
/// document`. Sec-Fetch-Site may be `none` (typed-in URL),
/// `same-origin` (link from same origin), or `cross-site` (paste from
/// chat, Slack, etc — the canonical share-link UX).
///
/// Missing ANY of the three headers → `400 client_too_old`. Other
/// invalid combinations → `403 csrf`.
fn check_sec_fetch(req: &HttpRequest) -> Result<(), HttpResponse> {
    let site = req
        .headers()
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok());
    let mode = req
        .headers()
        .get("sec-fetch-mode")
        .and_then(|v| v.to_str().ok());
    let dest = req
        .headers()
        .get("sec-fetch-dest")
        .and_then(|v| v.to_str().ok());
    if site.is_none() || mode.is_none() || dest.is_none() {
        return Err(HttpResponse::BadRequest()
            .header("Cache-Control", "no-store")
            .json(&json!({
                "error": "client too old; Fetch Metadata required for cookie-conversion",
                "code": "client_too_old",
                "min_browser_versions": {
                    "chrome": 76, "firefox": 90, "safari": 16.4, "edge": 79
                },
            })));
    }
    let mode = mode.unwrap();
    let dest = dest.unwrap();
    let site = site.unwrap();
    // Top-level navigation: Mode=navigate, Dest=document.
    if !mode.eq_ignore_ascii_case("navigate") || !dest.eq_ignore_ascii_case("document") {
        return Err(HttpResponse::Forbidden()
            .header("Cache-Control", "no-store")
            .json(&json!({"error": "CSRF: not a top-level navigation", "code": "csrf"})));
    }
    // Site values that indicate an OK navigation. `none` =
    // user-typed-or-bookmark; `same-origin` = link from this same
    // origin; `cross-site` = paste from external (Slack, email, etc.).
    if !matches!(site, "none" | "same-origin" | "cross-site" | "same-site") {
        return Err(HttpResponse::Forbidden()
            .header("Cache-Control", "no-store")
            .json(&json!({"error": "CSRF: unrecognized Sec-Fetch-Site", "code": "csrf"})));
    }
    Ok(())
}

/// Render the appropriate response for a token-error. `expired` and
/// `revoked` carry their own codes; everything else collapses to 401.
fn token_error_response(e: TokenError) -> HttpResponse {
    let code = e.wire_code();
    let status = match e {
        TokenError::Expired => StatusCode::UNAUTHORIZED,
        TokenError::Revoked => StatusCode::UNAUTHORIZED,
        TokenError::ScopeForbidden => StatusCode::FORBIDDEN,
        _ => StatusCode::UNAUTHORIZED,
    };
    HttpResponse::build(status).json(&json!({
        "error": "share token rejected",
        "code": code,
    }))
}

/// Compute the `__Host-zsbx_share_<sbx>` cookie suffix. The slug is
/// the typed-id with `_` → `-` (DNS-safe) and lower-cased; for v1
/// sandbox-ids are UUIDs, so we lowercase + strip non-alphanumerics
/// (matches `compute_preview_host`).
fn sandbox_slug(id: Uuid) -> String {
    id.to_string()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Pull a query parameter from the request's URI. Hand-parses to
/// avoid pulling another extractor through every signature.
fn query_param(req: &HttpRequest, key: &str) -> Option<String> {
    req.uri().query().and_then(|q| {
        q.split('&').find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            if k == key {
                // URL-decode `+` → ` ` is NOT performed; share tokens
                // and user_ids never contain spaces.
                Some(v.to_string())
            } else {
                None
            }
        })
    })
}

/// Pull a single cookie value out of the `Cookie:` header. Cookies
/// are `;`-separated `name=value` pairs (RFC 6265 §5.4); we split,
/// trim, and find by exact name.
fn parse_cookie_value(header: &str, name: &str) -> Option<String> {
    for piece in header.split(';') {
        let p = piece.trim();
        if let Some((k, v)) = p.split_once('=') {
            if k == name {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Strip `t=<value>` from a query string, preserving every other
/// `key=value` pair. The result has no leading `?`.
fn strip_t_query(q: &str) -> String {
    q.split('&')
        .filter(|piece| !piece.is_empty() && !piece.starts_with("t="))
        .collect::<Vec<_>>()
        .join("&")
}

/// `authorize(principal, info, port)` — the SOLE gate (round-6
/// CRITICAL-2). All conditions MUST hold:
///
/// 1. Port is in the proxyable allow-set (caller already checks this
///    before calling, but the gate re-checks as belt-and-suspenders).
/// 2. Principal owns the sandbox (Creator) OR the share-token claims
///    bind to this exact `(sbx, port)` tuple (ShareToken).
///
/// Method-scope enforcement (`ro` vs `rw`) lives in
/// [`authorize_with_method`]; this signature is preserved for
/// callers that don't need method-scope (the test surface).
pub fn authorize(
    principal: &Principal,
    info: &crate::backend::SandboxInfo,
    port: u16,
) -> bool {
    authorize_with_method(principal, info, port, "GET")
}

/// `authorize` plus method-scope enforcement for share tokens. Used
/// by the live request path; the legacy `authorize` is kept as a
/// thin wrapper for the unit tests that pre-date the method-scope.
pub fn authorize_with_method(
    principal: &Principal,
    info: &crate::backend::SandboxInfo,
    port: u16,
    method: &str,
) -> bool {
    if !is_proxyable_port(port, DEFAULT_DENY) {
        return false;
    }
    match principal {
        Principal::Creator { user_id } => &info.user_id == user_id,
        Principal::ShareToken { claims } => {
            // The claims were already validated end-to-end by the
            // public-edge layer (`try_share_cookie` / cookie-conversion)
            // against this exact `(sbx, port, method)`. Re-check here
            // belt-and-suspenders against principal-construction bugs.
            claims.sbx == info.sandbox_id
                && claims.port == port
                && preview_share::scope_allows_method(&claims.scope, method)
        }
    }
}

fn forward_blocking(
    method: &str,
    url: &str,
    req_headers: &[(String, String)],
    body: &[u8],
) -> Result<(StatusCode, Vec<(String, String)>, Vec<u8>), String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(AGENT_TIMEOUT_SECS))
        .redirects(0)
        .build();

    let mut req = agent.request(method, url);
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
        Err(e) => return Err(format!("dial agent {url}: {e}")),
    };

    let status = StatusCode::from_u16(resp.status())
        .map_err(|e| format!("invalid agent status {}: {e}", resp.status()))?;

    let mut headers: Vec<(String, String)> = Vec::new();
    for name in resp.headers_names() {
        for value in resp.all(&name) {
            headers.push((name.clone(), value.to_string()));
        }
    }

    use std::io::Read;
    let mut buf = Vec::new();
    let max = max_body_bytes();
    let mut reader = resp.into_reader().take(max as u64 + 1);
    reader
        .read_to_end(&mut buf)
        .map_err(|e| format!("read agent body: {e}"))?;
    if buf.len() > max {
        return Err(format!("agent body exceeded cap of {max} bytes"));
    }

    Ok((status, headers, buf))
}

// ─── uniform error responses (no oracle by sandbox existence) ──────

fn uniform_401() -> HttpResponse {
    HttpResponse::Unauthorized().json(&json!({
        "error": "unauthorized",
        "code": "auth_required",
    }))
}

fn uniform_404() -> HttpResponse {
    HttpResponse::NotFound().json(&json!({
        "error": "not found",
        "code": "not_found",
    }))
}

fn uniform_413() -> HttpResponse {
    HttpResponse::PayloadTooLarge().json(&json!({
        "error": "payload too large",
        "code": "payload_too_large",
    }))
}

fn err_with_code(status: StatusCode, code: &str) -> HttpResponse {
    HttpResponse::build(status).json(&json!({"error": code, "code": code}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::SandboxInfo;

    fn fake_info(id: &str, user: &str) -> SandboxInfo {
        SandboxInfo {
            sandbox_id: id.to_string(),
            user_id: user.to_string(),
            project_id: "p".into(),
            backend: "nomad-ch".into(),
            backend_hint: "test".into(),
            created_at_secs: 0,
            last_used_at_secs: 0,
        }
    }

    #[test]
    fn authorize_creator_ownership() {
        let info = fake_info("sbx", "alice");
        let alice = Principal::Creator { user_id: "alice".into() };
        let bob = Principal::Creator { user_id: "bob".into() };
        assert!(authorize(&alice, &info, 5173));
        assert!(!authorize(&bob, &info, 5173));
    }

    #[test]
    fn authorize_port_deny() {
        let info = fake_info("sbx", "alice");
        let alice = Principal::Creator { user_id: "alice".into() };
        // 22 is hardcoded-deny; even the owner can't proxy it.
        assert!(!authorize(&alice, &info, 22));
        assert!(!authorize(&alice, &info, 9229));
        assert!(!authorize(&alice, &info, 7777));
    }

    #[test]
    fn compute_preview_host_strips_dashes_and_lowercases() {
        // Direct shape check on a small input.
        let h = compute_preview_host("abc-DEF-123", 3000);
        assert_eq!(h, "preview-abcdef123-3000.preview.zeroship.dev");
        // UUIDs lose their dashes; case-folded.
        let h_uuid =
            compute_preview_host("11111111-2222-3333-4444-555555555555", 5173);
        assert_eq!(
            h_uuid,
            "preview-11111111222233334444555555555555-5173.preview.zeroship.dev"
        );
    }

    #[test]
    fn mint_nonce_well_formed() {
        let n = mint_nonce();
        assert!(!n.is_empty());
        assert!(n.len() <= 64, "nonce must respect the agent MAX_NONCE_LEN");
        assert!(n
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    }

    #[test]
    fn mint_nonce_unique_within_process() {
        let a = mint_nonce();
        let b = mint_nonce();
        assert_ne!(a, b);
    }
}
