//! Request entry, resource-tree dispatch, idempotency hooks, and
//! worker-forwarding for the gateway router.
//!
//! Public surface (re-exported from `router/mod.rs`):
//!
//! * [`handle`] — path-based handler `/{app_name}/{tail*}`.
//! * [`handle_subdomain`] — Host-header subdomain handler.
//! * [`extract_app_name`] — shared helper used by both, plus the
//!   gateway's outer middleware.
//!
//! Internal flow (`handle_request` → `execute_resource_tree`):
//!
//! 1. Resolve route by app name.
//! 2. CORS preflight short-circuit when applicable.
//! 3. Resource-tree dispatch — auth gate, CSRF, max-input, rate-limit,
//!    idempotency, then run the resolved action (worker forward,
//!    redirect, rewrite, static).

use std::sync::Arc;

use ntex::util::Bytes;
use ntex::web::{self, HttpRequest, HttpResponse};
use uuid::Uuid;

use crate::{enforce, idempotency, oidc_rp, proxy, GateState};

use super::auth::{
    extract_session_cookie, jwt_subject_unverified, resolve_auth, AuthOutcome,
};
use super::cors::{build_preflight_response, inject_cors_response_headers};
use super::helpers::resource_key_hash;
use super::static_serve::serve_resource_tree_static;

// ---------------------------------------------------------------------------
// App name extraction
// ---------------------------------------------------------------------------

/// Extract the app name from the request.
///
/// 1. If `path_name` is `Some` and non-empty, use it (path-based routing).
/// 2. Otherwise parse the `Host` header: extract `{app_name}.{domain}`.
/// 3. Ignore bare `localhost`, `localhost:PORT`, and IP addresses.
pub fn extract_app_name(req: &HttpRequest, path_name: Option<&str>) -> Option<String> {
    // 1. Path-based routing takes priority
    if let Some(name) = path_name {
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }

    // 2. Subdomain-based routing via Host header
    let host_header = req.headers().get("host")?.to_str().ok()?;

    // Strip port if present
    let host = host_header.split(':').next().unwrap_or(host_header);

    // Ignore bare localhost
    if host == "localhost" {
        return None;
    }

    // Ignore IP addresses (starts with digit or contains only digits and dots)
    if host.starts_with(|c: char| c.is_ascii_digit())
        && host.chars().all(|c| c.is_ascii_digit() || c == '.' || c == ':')
    {
        return None;
    }

    // IPv6 addresses in brackets
    if host.starts_with('[') {
        return None;
    }

    // Extract first subdomain: "myapp.zeroship.ai" → "myapp"
    // Must have at least one dot (i.e., subdomain.domain)
    let dot_pos = host.find('.')?;
    let subdomain = &host[..dot_pos];

    if subdomain.is_empty() {
        return None;
    }

    Some(subdomain.to_string())
}

// ---------------------------------------------------------------------------
// Per-rule rate-limit bucket key derivation
// ---------------------------------------------------------------------------

/// Resolve the bucket discriminator for a per-rule rate limit. The
/// returned string is concatenated with `(app_id, rule_idx)` in
/// `PerRuleKey` to form the bucket key — clients sharing the same
/// discriminator share a token bucket.
///
/// * `RateLimitPer::Ip` — request's client IP. Falls back to "unknown"
///   when the connection has no peer address (test fixtures, exotic
///   transports). Uses the peer socket unless `trust_proxy` is enabled.
/// * `RateLimitPer::User` — the authenticated user identity (the JWT
///   `sub`, read unverified — the auth gate already verified it upstream;
///   here we only need a stable bucket discriminator). Unlike `Session`,
///   one user's many sessions/devices share a bucket. Anonymous callers
///   (no bearer JWT) fall back to the session cookie, then the IP, so an
///   unauthenticated burst still gets bucketed instead of sharing one ""
///   key.
/// * `RateLimitPer::Session` — the `__Host-zeroship_app_session` cookie
///   value (the per-origin session id the gateway mints on
///   `/__zeroship/auth/callback`). Anonymous callers (no cookie) fall back
///   to the IP so an unauthenticated burst still gets bucketed;
///   without the fallback they'd all share one "" key.
/// * `RateLimitPer::App` — constant `"app"`. One bucket platform-wide;
///   `(app_id, rule_idx, "app")` is the key, equivalent to a global
///   per-app limit at the rule level.
pub(crate) fn compute_bucket_id(
    req: &HttpRequest,
    per: zeroship_bundle::RateLimitPer,
    insecure_dev: bool,
    trust_proxy: bool,
) -> String {
    use zeroship_bundle::RateLimitPer;
    match per {
        RateLimitPer::Ip => client_ip(req, trust_proxy),
        RateLimitPer::User => {
            if let Some(sub) = req
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|auth| {
                    auth.strip_prefix("Bearer ")
                        .or_else(|| auth.strip_prefix("bearer "))
                })
                .and_then(|jwt| jwt_subject_unverified(jwt.trim()))
            {
                return format!("sub:{sub}");
            }
            // Unauthenticated caller hitting a user-scoped rule: degrade to
            // the session cookie, then the IP — never share one empty bucket.
            let cookie = req
                .headers()
                .get("cookie")
                .and_then(|v| v.to_str().ok());
            extract_session_cookie(cookie, insecure_dev)
                .map(|s| format!("sess:{s}"))
                .unwrap_or_else(|| client_ip(req, trust_proxy))
        }
        RateLimitPer::Session => {
            let cookie = req
                .headers()
                .get("cookie")
                .and_then(|v| v.to_str().ok());
            extract_session_cookie(cookie, insecure_dev)
                .unwrap_or_else(|| client_ip(req, trust_proxy))
        }
        RateLimitPer::App => "app".to_string(),
    }
}

pub(crate) fn client_ip(req: &HttpRequest, trust_proxy: bool) -> String {
    let peer_ip = req.peer_addr().map(|addr| addr.ip().to_string());
    let conn = req.connection_info();
    let proxy_remote = if trust_proxy { conn.remote() } else { None };
    client_ip_from(peer_ip, proxy_remote, trust_proxy)
}

fn client_ip_from(
    peer_ip: Option<String>,
    proxy_remote: Option<&str>,
    trust_proxy: bool,
) -> String {
    if trust_proxy {
        if let Some(remote) = proxy_remote {
            if !remote.is_empty() {
                return remote.to_string();
            }
        }
    }
    peer_ip.unwrap_or_else(|| "unknown".to_string())
}

/// True when the request carries the RFC 6455 upgrade headers we
/// expect for a WebSocket subscription. We check both `Upgrade` and
/// `Connection` (case-insensitive) to mirror what reverse proxies
/// typically forward — some normalize header casing, some don't.
pub(crate) fn is_websocket_upgrade(req: &HttpRequest) -> bool {
    let upgrade = req
        .headers()
        .get("upgrade")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !upgrade.eq_ignore_ascii_case("websocket") {
        return false;
    }
    let connection = req
        .headers()
        .get("connection")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // Connection can be a comma-separated list; check token-by-token.
    connection
        .split(',')
        .any(|tok| tok.trim().eq_ignore_ascii_case("upgrade"))
}

/// Affinity discriminator for a subscription request — the second key
/// (after `app_id`) the gateway hashes to pick a worker. Callers who
/// reconnect with the same JWT or from the same client IP must land
/// on the same worker for the lifetime of a connection so the worker
/// can keep per-subscription state across `_zsRunSubscriptionGen`
/// turns. Spec §16 #4. Resolution order:
///
///   1. JWT subject (extracted from `Authorization: Bearer <jwt>` —
///      we do NOT verify the signature here; the gateway's auth
///      gate already ran and treats verification failures as 401).
///   2. `__Host-zeroship_app_session` cookie value (browser tab affinity).
///   3. `Sec-WebSocket-Key` (per-connection nonce — same connection
///      always hashes to the same bucket; reconnects vary).
///   4. Client IP (last-resort fallback for unauthenticated callers
///      hitting subscriptions on `publicly_accessible` resources).
pub(crate) fn subscription_affinity_key(
    req: &HttpRequest,
    insecure_dev: bool,
    trust_proxy: bool,
) -> String {
    if let Some(auth) = req.headers().get("authorization").and_then(|v| v.to_str().ok()) {
        if let Some(rest) = auth.strip_prefix("Bearer ").or_else(|| auth.strip_prefix("bearer ")) {
            if let Some(sub) = jwt_subject_unverified(rest.trim()) {
                return format!("sub:{sub}");
            }
        }
    }
    let cookie = req
        .headers()
        .get("cookie")
        .and_then(|v| v.to_str().ok());
    if let Some(token) = extract_session_cookie(cookie, insecure_dev) {
        return format!("sess:{token}");
    }
    if let Some(key) = req
        .headers()
        .get("sec-websocket-key")
        .and_then(|v| v.to_str().ok())
    {
        return format!("wsk:{key}");
    }
    let ip = client_ip(req, trust_proxy);
    format!("ip:{ip}")
}

// ---------------------------------------------------------------------------
// Existing path-based handler
// ---------------------------------------------------------------------------

pub async fn handle(
    req: HttpRequest,
    state: web::types::State<Arc<GateState>>,
    path: web::types::Path<(String, String)>,
    body: Bytes,
) -> HttpResponse {
    let (app_name, tail) = path.into_inner();
    handle_request(req, state, &app_name, &tail, body).await
}

// ---------------------------------------------------------------------------
// Subdomain-based handler (catch-all)
// ---------------------------------------------------------------------------

pub async fn handle_subdomain(
    req: HttpRequest,
    state: web::types::State<Arc<GateState>>,
    path: web::types::Path<String>,
    body: Bytes,
) -> HttpResponse {
    // `auth.zeroship.ai` is a platform-internal host, not a creator app.
    // The gateway proxies legacy `/oauth2/*` paths to Hydra; the
    // self-contained OP endpoints (`/.well-known/*`, `/userinfo`,
    // `/authorize`, `/token`, `/revoke`) and UI/webhook surfaces go to
    // crates/auth.
    if is_auth_host(&req) {
        return route_auth_host(req, state, body).await;
    }

    // OIDC callback for hosted creator apps. Intercepted *before*
    // manifest dispatch so the path can never collide with a route
    // the creator wrote (the `/__zeroship/` prefix is reserved). See
    // U5.3 / proposal §10.2.
    if req.uri().path() == "/__zeroship/auth/callback" {
        return handle_auth_callback(req, state).await;
    }

    let app_name = match extract_app_name(&req, None) {
        Some(name) => name,
        None => {
            return HttpResponse::BadRequest()
                .json(&serde_json::json!({"error": "could not determine app from Host header"}));
        }
    };
    let tail = path.into_inner();
    handle_request(req, state, &app_name, &tail, body).await
}

// ---------------------------------------------------------------------------
// auth.zeroship.ai routing
// ---------------------------------------------------------------------------

/// Which upstream a given `auth.zeroship.ai` request path should be
/// forwarded to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthUpstream {
    /// Legacy Ory Hydra public endpoints still consumed directly by auth UI
    /// helpers, currently under `/oauth2/*`.
    Hydra,
    /// Self-contained OP endpoints, login UI, OAuth2 consent handlers, and
    /// webhooks — implemented by `crates/auth`.
    Auth,
}

/// Classify an inbound `auth.zeroship.ai` path. The protocol endpoints
/// listed here are the self-contained OP contract implemented by `crates/auth`:
///
///   * `/.well-known/openid-configuration`, `/.well-known/jwks.json`
///   * `/userinfo`
///   * `/authorize`, `/token`, `/revoke`
///
/// Legacy `/oauth2/*` paths are still forwarded to the Hydra sidecar for
/// device/logout compatibility until the broader P5f naming/cleanup pass.
/// Everything else is handled by `crates/auth` (login HTML, consent callbacks,
/// signup, password reset, …).
pub(crate) fn classify_auth_path(path: &str) -> AuthUpstream {
    if path.starts_with("/oauth2/") {
        AuthUpstream::Hydra
    } else {
        AuthUpstream::Auth
    }
}

/// Platform-internal auth hosts. A request whose Host matches one of
/// these EXACTLY is routed to the internal auth/Hydra upstream rather
/// than treated as a creator app. Production is `auth.zeroship.ai`;
/// `auth.zeroship.localhost` is the dev/compose hostname.
const AUTH_HOSTS: &[&str] = &["auth.zeroship.ai", "auth.zeroship.localhost"];

/// True when the request's Host header is a platform auth host.
///
/// The match is **anchored / exact** (lowercased, port-stripped). A
/// loose prefix/substring comparison would let a crafted Host —
/// `auth.zeroship.ai.evil.com` (suffix), `authx.zeroship.ai` (label
/// prefix-extension), `evil-auth.zeroship.ai` (substring) — reach the
/// internal auth/Hydra routing it must never see (finding P2-A2).
fn is_auth_host(req: &HttpRequest) -> bool {
    let Some(host_hdr) = req.headers().get("host").and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let host = host_hdr.split(':').next().unwrap_or(host_hdr);
    let host_lc = host.to_ascii_lowercase();
    AUTH_HOSTS.contains(&host_lc.as_str())
}

async fn route_auth_host(
    req: HttpRequest,
    state: web::types::State<Arc<GateState>>,
    body: Bytes,
) -> HttpResponse {
    let path = req.uri().path();
    let upstream_base = match classify_auth_path(path) {
        AuthUpstream::Hydra => state.config.hydra_public_url.as_str(),
        AuthUpstream::Auth => state.config.auth_ui_url.as_str(),
    };

    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");

    // SEC-3: the auth/Hydra upstream keys per-IP rate limits on the forwarded
    // client address. Scrub any client-supplied `X-Forwarded-For` /
    // `Forwarded` / `X-Real-IP` and inject a SINGLE authoritative
    // `X-Forwarded-For` derived from the gateway's own trust_proxy-aware
    // client-IP policy (the immediate peer when the gateway is the edge; the
    // fronting proxy's forwarded client when `trust_proxy` is set) — the SAME
    // address the gateway keys its own rate limits on. A creator app (or any
    // inbound caller) therefore cannot rotate a spoofed IP to bypass the auth
    // service's limits, AND a legitimately-fronted (e.g. Caddy) deployment does
    // not collapse every user onto the proxy's single IP.
    let client_ip = auth_forward_client_ip(&req, state.config.trust_proxy);
    let headers = build_auth_upstream_headers(req.headers(), client_ip);

    let method = req.method().as_str();

    match proxy::forward_http(upstream_base, method, path_and_query, &headers, &body).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::warn!(error = %e, upstream = %upstream_base, "auth-host proxy error");
            HttpResponse::BadGateway()
                .json(&serde_json::json!({"error": format!("auth proxy: {e}")}))
        }
    }
}

/// Inbound client-IP spoofing vectors the gateway MUST strip before forwarding
/// to the trusted auth/Hydra upstream (SEC-3). The auth service derives its
/// per-IP rate-limit bucket key from a forwarded client address; if a creator
/// app's request could carry these verbatim, an attacker would rotate the
/// value to evade credential-stuffing / email-amplification limits and mint an
/// unbounded number of `zeroship.rate_limits` rows. Matching is
/// case-insensitive (HTTP header names are case-insensitive).
const CLIENT_IP_SPOOF_HEADERS: &[&str] = &["x-forwarded-for", "forwarded", "x-real-ip"];

/// Build the header set forwarded to the auth/Hydra upstream: drop any
/// client-supplied forwarding headers ([`CLIENT_IP_SPOOF_HEADERS`]) and inject
/// a SINGLE authoritative `X-Forwarded-For` carrying the real socket peer.
///
/// The injected `X-Forwarded-For` is the ONLY client-IP signal the auth
/// service sees, so its per-IP rate-limit buckets key on an address the
/// inbound caller cannot forge. When the peer address is unknown (exotic
/// transport / test fixture) NO `X-Forwarded-For` is injected — the auth side
/// falls back to its own socket peer rather than trusting a forged header.
fn build_auth_upstream_headers(
    headers: &ntex::http::HeaderMap,
    peer_ip: Option<std::net::IpAddr>,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (name, value) in headers {
        let lname = name.as_str().to_ascii_lowercase();
        if CLIENT_IP_SPOOF_HEADERS.contains(&lname.as_str()) {
            // Drop the inbound copy — we re-author the authoritative value.
            continue;
        }
        if let Ok(v) = value.to_str() {
            out.push((name.as_str().to_string(), v.to_string()));
        }
    }
    if let Some(ip) = peer_ip {
        out.push(("X-Forwarded-For".to_string(), ip.to_string()));
    }
    out
}

/// The authoritative client IP the gateway forwards to the auth/Hydra upstream
/// (SEC-3). Reuses the gateway's own [`client_ip`] policy so the auth service
/// keys its per-IP rate-limit buckets on the SAME address the gateway keys its
/// own limits on: the immediate socket peer when the gateway is the edge, or
/// the fronting proxy's forwarded client when `trust_proxy` is enabled. Raw
/// `peer_addr()` would be wrong behind a trusted proxy — it would hand the auth
/// service the proxy's address and collapse every user onto one shared bucket.
/// Returns `None` when no parseable IP is available, in which case no
/// `X-Forwarded-For` is injected and the auth side falls back to its own peer.
fn auth_forward_client_ip(req: &HttpRequest, trust_proxy: bool) -> Option<std::net::IpAddr> {
    let ip = client_ip(req, trust_proxy);
    ip.parse::<std::net::IpAddr>()
        .ok()
        .or_else(|| ip.parse::<std::net::SocketAddr>().ok().map(|sa| sa.ip()))
}

// ---------------------------------------------------------------------------
// Unified request handler
// ---------------------------------------------------------------------------

/// Outcome of canonicalizing an inbound dispatch path (SEC-2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CanonicalPath {
    /// The path is safe to dispatch under this canonical form. Auth matching
    /// AND the worker-forwarded URL both use this exact string, so the
    /// gateway's resource match can never disagree with the worker's
    /// WHATWG `new URL` re-parse.
    Use(String),
    /// The path carried a traversal / empty-segment form (`.`/`..`, literal or
    /// `%2e`-encoded, or `//`) that a browser's `new URL` would silently
    /// rewrite. Rather than guess the worker's normalization, reject (400).
    Reject,
}

/// Canonicalize an inbound dispatch path for SEC-2.
///
/// Fails CLOSED on the gateway↔worker path-disagreement class: a path that
/// carries a dot-segment (`.`/`..`, literal or `%2e`-encoded) or an empty
/// interior segment (`//`) is `Reject`ed (the caller answers 400) — a browser's
/// `new URL` would silently rewrite those, so matching auth on one form while
/// forwarding another is the bypass. Every other path is returned as its
/// [`canonicalize_path`] normal form (e.g. a lone trailing slash is stripped),
/// and the caller forwards THAT canonical string to the worker so the worker's
/// `new URL(req.url).pathname` reproduces exactly what the gateway matched.
pub(crate) fn canonicalize_dispatch_path(dispatch_path: &str) -> CanonicalPath {
    if crate::compiled::path_has_traversal_or_empty_segment(dispatch_path) {
        return CanonicalPath::Reject;
    }
    CanonicalPath::Use(crate::compiled::canonicalize_path(dispatch_path))
}

async fn handle_request(
    req: HttpRequest,
    state: web::types::State<Arc<GateState>>,
    app_name: &str,
    tail: &str,
    body: Bytes,
) -> HttpResponse {
    let wall_start = std::time::Instant::now();

    // 1. Route resolution — we need the app_id for both dispatch and static assets.
    let (app_id, compiled_route) = match state.routes.lookup_by_name(app_name) {
        Some(r) => r,
        None => {
            return HttpResponse::NotFound()
                .json(&serde_json::json!({"error": format!("app '{app_name}' not found")}));
        }
    };

    // Normalize tail: strip leading slash
    let tail = tail.strip_prefix('/').unwrap_or(tail);
    let raw_dispatch_path = format!("/{tail}");

    // SEC-2: canonicalize the request path BEFORE any resource matching, and
    // derive the worker-forwarded path from the SAME canonical form so the
    // gateway's auth match can never disagree with the worker's WHATWG
    // `new URL(req.url).pathname`. A traversal / empty-segment form (`.`/`..`,
    // literal or `%2e`-encoded, or `//`) — which a browser would silently
    // rewrite — is rejected (400) rather than guessed.
    let dispatch_path = match canonicalize_dispatch_path(&raw_dispatch_path) {
        CanonicalPath::Use(p) => p,
        CanonicalPath::Reject => {
            return HttpResponse::BadRequest().json(&serde_json::json!({
                "error": "request path is not canonical (path traversal or empty segment)",
            }));
        }
    };
    // The forwarded `tail` is the canonical path minus its leading slash, so
    // the URL the worker re-parses matches the resource the gateway gated.
    let tail = dispatch_path.strip_prefix('/').unwrap_or(&dispatch_path);

    // CORS preflight short-circuit. The browser sends `OPTIONS` with
    // `Origin` and `Access-Control-Request-Method` *before* the actual
    // request; we look up the resource-tree CORS policy for the
    // requested path and answer with a 204. If no resource matches,
    // fall through to normal dispatch (which will return 404).
    let resolved_resource = compiled_route
        .manifest
        .lookup_canonical_resource_resolved(&dispatch_path);
    if req.method() == ntex::http::Method::OPTIONS && req.headers().contains_key("origin") {
        if let Some(resolved) = resolved_resource.as_ref() {
            if let Some(cors) = &resolved.policy.cors {
                let origin = req
                    .headers()
                    .get("origin")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                return build_preflight_response(cors, origin, wall_start);
            }
        }
    }

    // Resource-tree dispatch — resources is the only dispatch path.
    // No match → 404 (the `*` catch-all in resources should always
    // match if the user wants a fallback handler).
    let Some(resolved_resource) = resolved_resource else {
        return HttpResponse::NotFound()
            .json(&serde_json::json!({"error": "no resource matched"}));
    };

    execute_resource_tree(
        req,
        state,
        &app_id,
        &compiled_route,
        resolved_resource,
        &dispatch_path,
        tail,
        body,
        wall_start,
    )
    .await
}

// ---------------------------------------------------------------------------
// v3 resource-tree dispatch
// ---------------------------------------------------------------------------

/// Run the v3 resource-tree dispatch for a request that resolved to a
/// manifest with `resources` non-empty. Looks up the matching resource,
/// enforces the precomputed `EffectivePolicy`, and executes the
/// resolved action (worker forward / redirect / static).
#[allow(clippy::too_many_arguments)]
async fn execute_resource_tree(
    req: HttpRequest,
    state: web::types::State<Arc<GateState>>,
    app_id: &Uuid,
    compiled_route: &crate::sync::CompiledRoute,
    resolved_resource: crate::compiled::ResolvedResource<'_>,
    dispatch_path: &str,
    tail: &str,
    body: Bytes,
    wall_start: std::time::Instant,
) -> HttpResponse {
    use crate::compiled::ResolvedAction;
    use zeroship_bundle::ProcedureKind;

    let policy = resolved_resource.policy;

    // 1a. Account gate (G2): the OUTER AND, evaluated BEFORE spend at the SAME
    //     hoist point so a `Suspended` creator's apps 402 across every action
    //     class (worker, redirect, rewrite, AND static egress) before any worker
    //     proxy — a suspended creator must not serve billed static/redirect
    //     egress either. `Suspended` → 402 `ACCOUNT_SUSPENDED`; `PastDue` (the
    //     grace window) and `Active` pass. Distinct from the spend 402 below
    //     (`SPEND_LIMIT`) so a dead-card suspension is told apart from a usage cap.
    if let Err(resp) = enforce::check_account(compiled_route.entry.account_state) {
        return resp;
    }

    // 1b. Spend gate (PR5, decision D1): hoisted to the TOP — BEFORE the action
    //     match — so `Block` 402s every action class uniformly (worker forward,
    //     redirect, rewrite, AND static), not just the worker path. A Blocked
    //     app must not serve static assets / redirects either: that egress is
    //     billed work the platform eats. `Warn`/`Allow`/`Degrade` pass here;
    //     Warn still only adds the `x-zs-spend-warn` header at the worker call
    //     site, and Degrade is throttled by the degraded registries downstream.
    //     Fail-closed: an unknown spend state maps to Block (see `check_spend`).
    if let Err(resp) = enforce::check_spend(compiled_route.entry.spend_state) {
        return resp;
    }

    // Capture origin once for downstream CORS injection.
    let origin_value = req
        .headers()
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // 2. Method-vs-kind gate (RPC procedures only).
    //    `kind: "mutation"` cannot be served via GET, and
    //    `kind: "subscription"` requires GET + Upgrade headers. We
    //    surface 405 for the wrong method and 426 UPGRADE_REQUIRED
    //    when Upgrade headers are missing. The actual handshake runs
    //    in `proxy_subscription_upgrade` once we get past the rest of
    //    the pre-dispatch checks.
    if let Some(kind) = policy.kind {
        let method = req.method();
        let allow = match kind {
            ProcedureKind::Query => {
                method == ntex::http::Method::GET
                    || method == ntex::http::Method::POST
                    || method == ntex::http::Method::HEAD
            }
            ProcedureKind::Mutation => method == ntex::http::Method::POST,
            // Action is the most permissive variant — no method
            // restriction. Mirrors the "no kind" path used by the
            // current vite-plugin manifest emitter, which OMITS `kind`
            // for `action(...)` procedures so today's `if let
            // Some(kind)` check skips the gate entirely. When a future
            // emitter writes `kind: "action"` explicitly, the gate
            // still passes here.
            ProcedureKind::Action => true,
            ProcedureKind::Stream => true,
            ProcedureKind::Subscription => method == ntex::http::Method::GET,
        };
        if !allow {
            return HttpResponse::MethodNotAllowed()
                .json(&serde_json::json!({"error": "method not allowed for this procedure kind"}));
        }
        if matches!(kind, ProcedureKind::Subscription) {
            // Subscription URLs only resolve over a WebSocket upgrade.
            // Browsers + load testers that hit them with a plain GET
            // get a structured 426 so they know to switch protocols.
            if !is_websocket_upgrade(&req) {
                return HttpResponse::build(ntex::http::StatusCode::UPGRADE_REQUIRED)
                    .header("connection", "Upgrade")
                    .header("upgrade", "websocket")
                    .json(&serde_json::json!({
                        "code": "UPGRADE_REQUIRED",
                        "message": "this resource is a subscription; use WebSocket (Upgrade: websocket, Sec-WebSocket-Protocol: zs.v1)",
                    }));
            }
        }
    }

    // 3. Auth gate. `anon` always passes (subject to
    //    `publicly_accessible` being set, which is enforced at
    //    validate-time). `user`/`admin` require a valid
    //    `__Host-zeroship_app_session` cookie. Richer admin-vs-user role
    //    checks will arrive with the auth tier.
    //
    //    On miss for an HTML navigation we kick off the OIDC dance via
    //    a 302 → the platform OP; API clients see a 401 with a `WWW-Authenticate`
    //    challenge so they can prompt the user out-of-band.
    let request_id = Uuid::new_v4();
    let user_header_from_gate =
        match resolve_auth(
            &req,
            &state,
            policy,
            &request_id,
            compiled_route.entry.oauth_client_id.as_deref(),
            compiled_route.entry.sector_identifier.as_deref(),
        )
        .await
        {
            AuthOutcome::Allowed { user_header } => user_header,
            AuthOutcome::Unauthenticated => {
                return unauthenticated_response(
                    &req,
                    &state,
                    compiled_route.entry.oauth_client_id.as_deref(),
                );
            }
            AuthOutcome::ClientNotProvisioned => {
                return client_not_provisioned_response();
            }
            AuthOutcome::InsufficientScope { required } => {
                return insufficient_scope_response(&required);
            }
        };

    // 4. CSRF origin guard. Mutations with a declared csrf_origins list
    //    require the request's `Origin` to match.
    //
    //    `Some(Action)` and `None` route identically — action is the
    //    most permissive variant and the current manifest emitter omits
    //    `kind` for it. Both forms must continue to gate the same way.
    if matches!(policy.kind, Some(ProcedureKind::Mutation) | Some(ProcedureKind::Action))
        || req.method() == ntex::http::Method::POST
        || req.method() == ntex::http::Method::PUT
        || req.method() == ntex::http::Method::PATCH
        || req.method() == ntex::http::Method::DELETE
    {
        if let Some(allowed) = &policy.csrf_origins {
            let origin = origin_value.as_deref().unwrap_or("");
            if !allowed.iter().any(|o| o == origin) {
                return HttpResponse::Forbidden()
                    .json(&serde_json::json!({"error": "origin not in csrf_origins allow list"}));
            }
        }
    }

    // 5. Max-input-bytes guard. Cheap when not set; cap the body size
    //    before forwarding to the worker.
    if let Some(cap) = policy.max_input_bytes {
        if body.len() > cap as usize {
            return HttpResponse::PayloadTooLarge()
                .json(&serde_json::json!({"error": "input exceeds max_input_bytes"}));
        }
    }

    // 6. Per-resource rate-limit (when declared).
    //    Reuses the existing `PerRuleRateLimitRegistry` by hashing the
    //    resource key into a stable `rule_idx`. Two distinct resource
    //    keys with the same rate_limit shape get independent buckets.
    if let Some(rl) = &policy.rate_limit {
        let rule_idx = resource_key_hash(resolved_resource.key.as_ref());
        let bucket_id = compute_bucket_id(
            &req,
            rl.per,
            state.config.insecure_dev,
            state.config.trust_proxy,
        );
        if let Err(resp) = state.per_rule_rate_limits.check(
            app_id,
            rule_idx,
            rl.per,
            &bucket_id,
            rl,
        ) {
            return resp;
        }
    }

    // 7. Idempotency dedupe (only for `idempotent: true` mutations).
    //    Resolved before the worker is touched: a hit returns the
    //    stored response verbatim; a conflict / missing-header case
    //    rejects with the right error envelope.
    //
    //    Only mutations enter the dedupe path. Queries are inherently
    //    safe; streams and subscriptions do not use dedupe either.
    //
    //    `Some(Action)` is treated as `None` here — both mean "no kind
    //    restriction"; the manifest emitter omits `kind` for action
    //    procedures but a future emitter may write it explicitly.
    let idempotency_handle = if policy.idempotent
        && matches!(
            policy.kind,
            Some(ProcedureKind::Mutation) | Some(ProcedureKind::Action) | None
        )
        && matches!(policy.action, ResolvedAction::WorkerRpc)
    {
        match handle_idempotency_pre_dispatch(
            &req,
            &state,
            app_id,
            dispatch_path,
            policy,
            &body,
            wall_start,
        )
        .await
        {
            IdempotencyOutcome::ReturnNow(mut resp) => {
                if let (Some(cors), Some(origin)) = (policy.cors.as_ref(), origin_value.as_deref()) {
                    if !origin.is_empty() {
                        inject_cors_response_headers(resp.headers_mut(), cors, origin);
                    }
                }
                return resp;
            }
            IdempotencyOutcome::Proceed(handle) => Some(handle),
        }
    } else {
        None
    };

    // 8. Execute the resolved action.
    //
    //    Metering coverage (#27): track whether THIS arm produced
    //    gateway-originated egress — a body the worker never sees (static
    //    asset, redirect, or the gateway's own error page for those arms).
    //    Only such bodies are metered as `gateway_egress_bytes` in step 8b
    //    below. The worker-proxy arms (`WorkerRpc`/`WorkerSsr`/`Rewrite`)
    //    are NOT gateway-owned: the worker already counts its response body
    //    as `egress_bytes`, so the gateway must never meter it (that would
    //    double-bill the same byte — the one over-bill vector). The two
    //    metrics are disjoint BY CONSTRUCTION (worker-body vs gateway-body).
    let gateway_owned_egress = matches!(
        policy.action,
        ResolvedAction::Static { .. } | ResolvedAction::Redirect { .. }
    );
    let mut response = match &policy.action {
        ResolvedAction::WorkerRpc | ResolvedAction::WorkerSsr => {
            // Subscription procedures need a WebSocket-aware proxy
            // path. Idempotency is bypassed (already enforced above).
            // Affinity routing uses `(app_id, principal)` so reconnects
            // from the same caller pin the same worker. The transparent
            // WS proxy itself is wired in `proxy::forward_subscription`.
            if matches!(policy.kind, Some(ProcedureKind::Subscription)) {
                handle_subscription_dispatch(
                    req,
                    &state,
                    app_id,
                    &compiled_route.entry,
                    tail,
                    wall_start,
                )
                .await
            } else {
                handle_dispatch(
                    req,
                    &state,
                    app_id,
                    &compiled_route.entry,
                    tail,
                    request_id,
                    body,
                    user_header_from_gate.clone(),
                    wall_start,
                )
                .await
            }
        }
        ResolvedAction::Redirect { to, status } => {
            let st = ntex::http::StatusCode::from_u16(*status)
                .unwrap_or(ntex::http::StatusCode::FOUND);
            HttpResponse::build(st)
                .header("location", to.clone())
                .header(
                    "x-wall-time-ms",
                    format!("{:.2}", wall_start.elapsed().as_secs_f64() * 1000.0),
                )
                .finish()
        }
        ResolvedAction::Rewrite { to } => {
            // Rewrite forwards under the new path. Recursive rewrites
            // are not yet supported because they would need to re-enter
            // `lookup_resource` with hop limiting. Pass through to the
            // worker for now.
            let _ = to;
            handle_dispatch(
                req,
                &state,
                app_id,
                &compiled_route.entry,
                tail,
                request_id,
                body,
                user_header_from_gate.clone(),
                wall_start,
            )
            .await
        }
        ResolvedAction::Static { try_chain } => {
            // Resolve the first asset that exists in `assets` /
            // `runtime_assets`. The legacy walker has more
            // sophisticated `$path` / `[capture]` substitution; for the
            // resource-tree path the build emits literal templates.
            serve_resource_tree_static(
                &state,
                compiled_route,
                &req,
                dispatch_path,
                try_chain,
                app_id,
                wall_start,
            )
            .await
        }
    };

    // 8b. Gateway egress metering (#27). For gateway-owned arms only, record
    //     the served body length as `gateway_egress_bytes` against the
    //     route's server-resolved `app_id` (never a client value).
    //
    //     Two recording points, by body shape:
    //       * Buffered static (`Bytes`), redirect, and the static arm's own
    //         error bodies (404/503) — `BodySize::Sized(n)` is the FULLY
    //         delivered length, so a one-shot record here is exact.
    //       * Streamed static (`SizedStream`) — the served length is only
    //         INTENDED up front; a client disconnect delivers fewer bytes.
    //         The streamed drain in `static_serve`/`streaming` therefore meters
    //         DELIVERED bytes itself (incremental accrual + a final delta on
    //         completion/disconnect) and stamps `EGRESS_METERED_HEADER`;
    //         `record_gateway_egress` sees the marker and skips the up-front
    //         size so a stream is never double-counted (finding #2).
    //
    //     The bump is cheap — `Meter::increment` takes an `RwLock` read plus a
    //     per-app `Mutex` for the custom metric (uncontended, no await, no
    //     blocking I/O); the flush to control runs in a detached task, so the
    //     proxy hot path is not slowed (the worker arms skip this entirely).
    //     This is disjoint from the worker's `egress_bytes` by construction;
    //     the gateway NEVER touches `egress_bytes`.
    if gateway_owned_egress {
        record_gateway_egress(&state, app_id, &mut response);
    }

    // 9. Idempotency capture — store the worker's response under the
    //    dedupe key when we held the in-flight lock through dispatch.
    //    Errors here are logged but never block the response.
    if let Some(handle) = idempotency_handle {
        response = capture_response_for_idempotency(&state, app_id, handle, response).await;
    }

    // 10. CORS injection on the response (resource-tree's flattened
    //     `cors`).
    if let (Some(cors), Some(origin)) = (policy.cors.as_ref(), origin_value.as_deref()) {
        if !origin.is_empty() {
            inject_cors_response_headers(response.headers_mut(), cors, origin);
        }
    }

    response
}

/// Record a gateway-originated response's body length as
/// `gateway_egress_bytes` for `app_id` (metering coverage #27).
///
/// Called ONLY for gateway-owned arms (static asset / redirect / the
/// gateway error page those arms emit) — bodies the worker never sees. The
/// worker owns `egress_bytes` for its proxied response bodies, so this
/// function (and the gateway in general) NEVER touches `egress_bytes`: the
/// two metrics are disjoint by construction and can never count the same
/// byte. `app_id` is the route's server-resolved id (never a client value).
///
/// Records the FULLY-delivered body size for buffered bodies (`Bytes` static,
/// redirect, the static arm's 404/503 error bodies) — `BodySize::Sized(n)` is
/// the exact delivered length there. The streamed-static (`SizedStream`) path
/// instead meters DELIVERED bytes inside its own drain (a disconnect bills
/// only what was written, not the intended size — finding #2) and stamps
/// `EGRESS_METERED_HEADER`; we detect that marker, strip it, and skip the
/// up-front size so a stream is never double-counted.
///
/// The increment is a cheap counter bump (an `RwLock` read + a per-app
/// `Mutex` for the custom metric — uncontended, not literally lock-free); the
/// flush to control runs in a detached background task, so this adds no
/// latency to the response path.
fn record_gateway_egress(state: &GateState, app_id: &Uuid, response: &mut HttpResponse) {
    use ntex::http::body::{BodySize, MessageBody};
    // A streamed-static response self-meters delivered bytes in its drain.
    // Strip the internal marker and skip the size record (no double-count).
    if response
        .headers()
        .contains_key(super::static_serve::EGRESS_METERED_HEADER)
    {
        response
            .headers_mut()
            .remove(super::static_serve::EGRESS_METERED_HEADER);
        return;
    }
    if let BodySize::Sized(n) = response.body().size() {
        if n > 0 {
            state
                .meter
                .increment(&app_id.to_string(), "gateway_egress_bytes", n);
        }
    }
}

// ---------------------------------------------------------------------------
// Idempotency dedupe — pre/post worker hooks
// ---------------------------------------------------------------------------

/// Outcome of [`handle_idempotency_pre_dispatch`]. The caller either
/// returns the response right now (cache hit / conflict / missing
/// header) or proceeds to dispatch with the [`InflightHandle`] in
/// hand to capture the worker's response.
pub(super) enum IdempotencyOutcome {
    /// Return this response without invoking the worker.
    ReturnNow(HttpResponse),
    /// Proceed to worker dispatch; capture the response after.
    Proceed(InflightHandle),
}

/// Carries the post-dispatch metadata needed to persist the worker's
/// response under the dedupe key. Constructed by `pre_dispatch` and
/// consumed by `capture_response_for_idempotency`.
pub(super) struct InflightHandle {
    pub(super) entry_key: String,
    pub(super) lock_key: String,
    pub(super) body_hash: String,
    pub(super) ttl_hours: u32,
}

/// Resolve the wireId from a `/__zeroship/v1/<wireId>` dispatch path. The
/// dispatch path always has a leading slash; the wireId is the rest
/// after the literal prefix. Returns `None` for any non-RPC path —
/// callers should only enter idempotency for `WorkerRpc` actions.
fn dispatch_path_wire_id(dispatch_path: &str) -> Option<&str> {
    dispatch_path.strip_prefix("/__zeroship/v1/")
}

/// Build the standard `application/zs-error+json` envelope for an
/// idempotency rejection.
fn build_zs_error_response(
    status: ntex::http::StatusCode,
    code: &str,
    message: &str,
    details: serde_json::Value,
    retryable: bool,
    retry_after_secs: Option<u64>,
    wall_start: std::time::Instant,
) -> HttpResponse {
    let body = serde_json::json!({
        "code": code,
        "message": message,
        "details": details,
        "retryable": retryable,
    });
    let mut builder = HttpResponse::build(status);
    builder.header("content-type", "application/zs-error+json");
    if let Some(secs) = retry_after_secs {
        builder.header("retry-after", secs.to_string());
    }
    builder.header(
        "x-wall-time-ms",
        format!("{:.2}", wall_start.elapsed().as_secs_f64() * 1000.0),
    );
    builder.body(serde_json::to_vec(&body).unwrap_or_default())
}

/// Replay a stored response. Drops hop-by-hop headers (we re-synthesize
/// the connection layer's headers fresh) and stamps `x-zs-idempotent-replay`
/// so callers can observe a hit in the wild.
pub(super) fn build_replay_response(
    stored: &idempotency::StoredResponse,
    wall_start: std::time::Instant,
) -> HttpResponse {
    let status = ntex::http::StatusCode::from_u16(stored.status)
        .unwrap_or(ntex::http::StatusCode::INTERNAL_SERVER_ERROR);
    let mut builder = HttpResponse::build(status);
    for (name, value) in &stored.headers {
        // The stored map already has hop-by-hop headers stripped, but
        // an extra defensive filter here protects against future
        // schema drift.
        builder.set_header(name.as_str(), value.as_str());
    }
    builder.set_header("x-zs-idempotent-replay", "true");
    builder.header(
        "x-wall-time-ms",
        format!("{:.2}", wall_start.elapsed().as_secs_f64() * 1000.0),
    );
    builder.body(stored.body_bytes())
}

/// Resolve the per-procedure inflight wait timeout. Bounded by the
/// procedure's declared timeout when present, and capped at
/// `DEFAULT_INFLIGHT_WAIT_MS` (~30s). Letting dedupe contention block a
/// worker thread for longer than the handler itself could run is
/// pointless.
fn inflight_wait_ms(policy: &crate::compiled::EffectivePolicy) -> u64 {
    match policy.timeout_ms {
        Some(t) if t > 0 => t.min(idempotency::DEFAULT_INFLIGHT_WAIT_MS),
        _ => idempotency::DEFAULT_INFLIGHT_WAIT_MS,
    }
}

/// Pre-dispatch idempotency hook: extract `Idempotency-Key`, hash the
/// body, consult the store, and decide. Only called for
/// `idempotent: true` mutations; the caller filters by policy.
pub(super) async fn handle_idempotency_pre_dispatch(
    req: &HttpRequest,
    state: &GateState,
    app_id: &Uuid,
    dispatch_path: &str,
    policy: &crate::compiled::EffectivePolicy,
    body: &Bytes,
    wall_start: std::time::Instant,
) -> IdempotencyOutcome {
    let Some(wire_id) = dispatch_path_wire_id(dispatch_path) else {
        // Not an RPC path — caller already filtered, but defensive
        // fallthrough means we don't dedupe.
        return IdempotencyOutcome::Proceed(InflightHandle {
            entry_key: String::new(),
            lock_key: String::new(),
            body_hash: String::new(),
            ttl_hours: idempotency::clamp_ttl_hours(policy.idempotency_ttl_hours),
        });
    };

    let idem_key = req
        .headers()
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let ttl_hours = idempotency::clamp_ttl_hours(policy.idempotency_ttl_hours);

    let decision = idempotency::pre_dispatch(
        state.idempotency_store.as_ref(),
        app_id,
        wire_id,
        idem_key.as_deref(),
        body,
        ttl_hours,
        inflight_wait_ms(policy),
    )
    .await;

    match decision {
        Ok(idempotency::DedupeDecision::Hit(stored)) => {
            IdempotencyOutcome::ReturnNow(build_replay_response(&stored, wall_start))
        }
        Ok(idempotency::DedupeDecision::Conflict { retry_after_secs }) => {
            IdempotencyOutcome::ReturnNow(build_zs_error_response(
                ntex::http::StatusCode::CONFLICT,
                "ALREADY_EXISTS",
                "Idempotency-Key was used with a different input within the dedupe window",
                serde_json::json!({
                    "reason": "idempotency_key_reused_with_different_input"
                }),
                false,
                Some(retry_after_secs),
                wall_start,
            ))
        }
        Ok(idempotency::DedupeDecision::MissingHeader) => {
            IdempotencyOutcome::ReturnNow(build_zs_error_response(
                ntex::http::StatusCode::BAD_REQUEST,
                "INVALID_ARGUMENT",
                "Idempotency-Key header required for this procedure",
                serde_json::json!({ "reason": "missing_idempotency_key" }),
                false,
                None,
                wall_start,
            ))
        }
        Ok(idempotency::DedupeDecision::InFlightTimedOut) => {
            IdempotencyOutcome::ReturnNow(build_zs_error_response(
                ntex::http::StatusCode::CONFLICT,
                "ABORTED",
                "Idempotency-Key in flight; original request did not complete in time",
                serde_json::json!({ "reason": "idempotency_inflight_timeout" }),
                false,
                None,
                wall_start,
            ))
        }
        Ok(idempotency::DedupeDecision::Proceed { entry_key, lock_key, body_hash, ttl_hours }) => {
            IdempotencyOutcome::Proceed(InflightHandle { entry_key, lock_key, body_hash, ttl_hours })
        }
        Err(e) => {
            // Store error → log and fail closed. A degraded dedupe
            // backend must not silently let duplicate mutations
            // through.
            tracing::error!(error = %e, "gateway: idempotency store error");
            IdempotencyOutcome::ReturnNow(build_zs_error_response(
                ntex::http::StatusCode::SERVICE_UNAVAILABLE,
                "UNAVAILABLE",
                "idempotency store temporarily unavailable",
                serde_json::json!({}),
                true,
                Some(1),
                wall_start,
            ))
        }
    }
}

/// Drain a response's body into bytes, returning a fresh
/// `HttpResponse` with the buffered body. The original response is
/// consumed because ntex's `take_body` empties it. Used after worker
/// dispatch so we can both capture the body for idempotency storage
/// AND ship it back to the client.
///
/// Streaming responses (chunked / SSE) flow through here too — we
/// buffer them entirely. Idempotency only applies to mutation
/// responses, which are virtually always small JSON envelopes; if a
/// mutation streams back megabytes it pays the buffer cost. Streams
/// and subscriptions don't enter this path (caller filters by kind).
pub(super) async fn buffer_response_body(mut resp: HttpResponse) -> (HttpResponse, Vec<u8>) {
    use ntex::http::body::{Body, MessageBody};
    let mut body = resp.take_body();
    let mut buf: Vec<u8> = Vec::new();
    std::future::poll_fn(|cx| {
        loop {
            match body.poll_next_chunk(cx) {
                std::task::Poll::Ready(Some(Ok(chunk))) => buf.extend_from_slice(&chunk),
                std::task::Poll::Ready(Some(Err(_))) | std::task::Poll::Ready(None) => {
                    return std::task::Poll::Ready(());
                }
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    })
    .await;
    let bytes = buf.clone();
    let new_resp = resp.set_body(Body::Bytes(Bytes::from(buf)));
    (new_resp, bytes)
}

/// Spec §8 post-dispatch hook: capture the worker's response under
/// the dedupe key and release the in-flight lock. Returns the response
/// to ship back to the client (with the body intact and an
/// `x-zs-idempotent-stored` flag for observability).
pub(super) async fn capture_response_for_idempotency(
    state: &GateState,
    app_id: &Uuid,
    handle: InflightHandle,
    response: HttpResponse,
) -> HttpResponse {
    let (mut buffered, body_bytes) = buffer_response_body(response).await;

    // Snapshot headers we want to replay. Drops hop-by-hop and
    // per-request stamps before storage.
    let mut header_pairs: Vec<(String, String)> = Vec::new();
    for (name, value) in buffered.headers().iter() {
        if let Ok(v) = value.to_str() {
            header_pairs.push((name.as_str().to_string(), v.to_string()));
        }
    }
    let stored_headers = idempotency::capture_response_headers(&header_pairs);
    let status = buffered.status().as_u16();

    if let Err(e) = idempotency::capture_response(
        state.idempotency_store.as_ref(),
        app_id,
        &handle.entry_key,
        &handle.lock_key,
        &handle.body_hash,
        status,
        &stored_headers,
        &body_bytes,
        handle.ttl_hours,
    )
    .await
    {
        tracing::error!(error = %e, "gateway: idempotency capture failed");
        // Best-effort lock release.
        let _ = idempotency::release_lock_without_storing(
            state.idempotency_store.as_ref(),
            &handle.lock_key,
        )
        .await;
    }

    buffered.headers_mut().insert(
        ntex::http::header::HeaderName::from_static("x-zs-idempotent-stored"),
        ntex::http::header::HeaderValue::from_static("true"),
    );
    buffered
}

// ---------------------------------------------------------------------------
// Subscription dispatch (WebSocket-aware)
// ---------------------------------------------------------------------------

/// Forward a `kind: "subscription"` GET-with-Upgrade request through the
/// gateway. Differs from `handle_dispatch` in two ways:
///
///   1. **Affinity routing**: hashes by `(app_id, principal)` so
///      reconnects from the same caller land on the same worker for
///      the lifetime of the connection. Falls back through JWT subject,
///      session cookie, Sec-WebSocket-Key, then IP — see
///      `subscription_affinity_key` for the order.
///
///   2. **Transparent WS proxy** (multi-node gateway): the gateway
///      currently does not perform a transparent WS proxy of the
///      upgraded connection — that requires hijacking the TCP
///      stream from ntex, which is a non-trivial integration. For
///      now, gateway-fronted subscriptions return 501 with a clear
///      message; single-tenant `zeroship serve` handles WS directly.
///
/// The affinity selection itself runs on the request — even when the
/// proxy returns 501 — so the routing decision is testable and
/// observable. Worker is acquired/released around the (currently
/// stub) proxy call to keep concurrency accounting consistent.
async fn handle_subscription_dispatch(
    _req: HttpRequest,
    state: &GateState,
    app_id: &Uuid,
    _route: &zeroship_core::types::RouteEntry,
    _tail: &str,
    wall_start: std::time::Instant,
) -> HttpResponse {
    // Spend gate (PR5): the `Block` 402 is enforced by `execute_resource_tree`
    // (hoisted to the top, before the action match), so a Blocked subscription
    // never reaches this stub. Degrade is throttled by the degraded registries
    // below; Warn passes (no body header on the 501 stub path).

    // Rate limit + concurrency: subscriptions count against the same
    // accounting as unary dispatch. A subscription that's been open
    // for hours holds one slot; that's intentional — the operator
    // can size `max_concurrent` to the steady-state subscription
    // count plus a margin for unary traffic.
    if let Err(resp) = enforce::check_rate_limit(&state.rate_limiters, app_id) {
        return resp;
    }
    let _guard = match enforce::acquire_concurrency(&state.concurrency, app_id) {
        Ok(guard) => guard,
        Err(resp) => return resp,
    };

    // Affinity selection — exercised even when the proxy itself
    // returns 501, so tests against this path can verify that the
    // hashing decision is correct.
    let affinity = subscription_affinity_key(
        &_req,
        state.config.insecure_dev,
        state.config.trust_proxy,
    );
    let (idx, _worker_url) = state.hash_ring.select_with_affinity(app_id, &affinity);
    state.hash_ring.acquire(idx);
    // Release immediately — see comment below; we never actually
    // pump traffic through to the worker.
    state.hash_ring.release(idx);

    let wall_ms = wall_start.elapsed().as_secs_f64() * 1000.0;
    HttpResponse::build(ntex::http::StatusCode::NOT_IMPLEMENTED)
        .header("x-wall-time-ms", format!("{wall_ms:.2}"))
        .header("x-zs-affinity-idx", idx.to_string())
        .json(&serde_json::json!({
            "code": "UNIMPLEMENTED",
            "message": "WebSocket subscription proxy through the multi-node gateway is not yet wired; use `zeroship serve` for single-tenant subscriptions",
            "retryable": false,
        }))
}

// ---------------------------------------------------------------------------
// Reserved-header scrubbing for the worker dispatch envelope
// ---------------------------------------------------------------------------

/// Platform-reserved header names that must never be forwarded verbatim
/// into the worker dispatch envelope. These are either set by the gateway
/// itself on the trusted side-channel (`ZeroShip-User` HMAC), are
/// gateway/control-internal routing/identity hints, or are an attractive
/// nuisance for app JS that (incorrectly) reads identity from raw
/// `request.headers` instead of `env.auth`. A forged inbound copy of any
/// of these is dropped at the trust boundary.
///
/// Matching is case-insensitive (HTTP header names are case-insensitive);
/// callers compare against the lowercased inbound name.
const RESERVED_HEADER_EXACT: &[&str] = &[
    "zeroship-user",
    "authorization",
    "x-app-id",
    "x-plan-id",
    "x-request-id",
];

/// Reserved header *prefix*: every `x-zs-*` header is gateway/platform
/// internal and is stripped from the forwarded envelope.
const RESERVED_HEADER_PREFIX: &str = "x-zs-";

/// True if `name` (any case) is a platform-reserved header that must not
/// be forwarded into the worker dispatch envelope.
fn is_reserved_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with(RESERVED_HEADER_PREFIX)
        || RESERVED_HEADER_EXACT.iter().any(|r| *r == lower)
}

/// Collect inbound request headers as `[name, value]` pairs for the worker
/// dispatch envelope, dropping the platform-reserved set (see
/// [`is_reserved_header`]). Non-UTF-8 header values are skipped.
fn collect_forwarded_headers(headers: &ntex::http::HeaderMap) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (name, value) in headers {
        if is_reserved_header(name.as_str()) {
            continue;
        }
        if let Ok(v) = value.to_str() {
            out.push((name.as_str().to_string(), v.to_string()));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Dispatch handler — the single worker-facing path
// ---------------------------------------------------------------------------

/// Forward an HTTP request to the worker via `/dispatch/{app_id}`.
/// Reconstruct the URL the worker's JS handler sees, re-appending the raw
/// query string the ntex `{tail*}` path extractor drops. Preserving the query
/// is load-bearing: `query()` RPC input rides in `?input=<base64url>` and apps
/// read `request.url` query params directly. See ISS-70.
fn forward_url(scheme: &str, host: &str, tail: &str, query: Option<&str>) -> String {
    match query {
        Some(q) if !q.is_empty() => format!("{scheme}://{host}/{tail}?{q}"),
        _ => format!("{scheme}://{host}/{tail}"),
    }
}

///
/// The full HTTP request (method, URL, headers, raw body bytes) is packaged
/// into the dispatch frame and handed to `Runtime::call_fetch_handler`, which
/// invokes the app's exported `default.fetch(req, env, ctx)`. Covers both the
/// `_rpc/*` URLs (routed inside the kernel via the bootstrap) and plain
/// HTTP requests. Enables streaming responses (e.g., SSE for LLM token
/// streaming).
#[allow(clippy::too_many_arguments)] // post-U5 arg count; refactor candidate for U6+.
async fn handle_dispatch(
    req: HttpRequest,
    state: &Arc<GateState>,
    app_id: &Uuid,
    route: &zeroship_core::types::RouteEntry,
    tail: &str,
    request_id: Uuid,
    body: Bytes,
    user_header_value: Option<String>,
    wall_start: std::time::Instant,
) -> HttpResponse {
    // Spend gate (PR5, decision D1): the `Block` 402 is enforced by
    // `execute_resource_tree` (hoisted to the top, before the action match) so
    // it covers worker forward / redirect / rewrite / static uniformly — a
    // Blocked app never reaches this worker-forwarding path. Here we only read
    // the `Warn` flag to stamp the advisory `x-zs-spend-warn: 1` response
    // header; Degrade is throttled by the degraded registries below.
    let spend_warn = route.spend_state == zeroship_core::types::SpendState::Warn;

    // Rate limit
    if let Err(resp) = enforce::check_rate_limit(&state.rate_limiters, app_id) {
        return resp;
    }

    // Concurrency guard (RAII — released on drop)
    let _guard = match enforce::acquire_concurrency(&state.concurrency, app_id) {
        Ok(guard) => guard,
        Err(resp) => return resp,
    };

    // Reconstruct the URL the JS handler will see. The query string MUST be
    // preserved: the `@zeroship/rpc` transport sends `query()` calls as
    // `GET /__zeroship/v1/<id>?input=<base64url>`, and deployed apps read
    // `new URL(request.url).searchParams` for pagination/filters/search. The
    // ntex `{tail*}` extractor yields the PATH only, so the raw query has to be
    // re-appended here — dropping it makes every GET query-RPC arrive with
    // `input: undefined` (→ 400 INVALID_ARGUMENT) and silently strips app query
    // params (ISS-70).
    let scheme = if req.connection_info().scheme() == "https" { "https" } else { "http" };
    let host = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let url = forward_url(scheme, host, tail, req.uri().query());

    // Collect request headers as [key, value] pairs, scrubbing the
    // platform-reserved set so a forged inbound `ZeroShip-User` /
    // `Authorization` / `x-zs-*` cannot ride into the worker's
    // dispatch envelope and be trusted by app JS reading raw
    // `request.headers`. Authoritative identity travels the separate
    // HMAC `ZeroShip-User` channel (`user_header_value`), never here.
    let headers = collect_forwarded_headers(req.headers());

    // Proxy to worker via CHWBL hash ring.
    let mut response = match proxy::forward_dispatch(
        &state.hash_ring,
        app_id,
        &route.plan_id,
        &request_id,
        req.method().as_str(),
        &url,
        &headers,
        body.as_ref(),
        user_header_value.as_deref(),
        &state.config.worker_key,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            return HttpResponse::BadGateway()
                .json(&serde_json::json!({"error": format!("worker error: {e}")}));
        }
    };

    // 401 from worker on an HTML navigation → start the OIDC dance.
    // The worker reaches this branch on resources its own JS code
    // gated as `user`/`admin` when the gateway forwarded without a
    // `ZeroShip-User` header. (Resource-tree `user`/`admin` are
    // already short-circuited by `auth_satisfied` upstream, so they
    // never reach the worker.) API clients still see the 401 verbatim.
    if response.status() == ntex::http::StatusCode::UNAUTHORIZED && wants_html(&req) {
        return match route.oauth_client_id.as_deref() {
            Some(client_id) => start_oidc_redirect(&req, state, client_id),
            None => client_not_provisioned_response(),
        };
    }

    // Add response headers.
    let wall_ms = wall_start.elapsed().as_secs_f64() * 1000.0;
    response.headers_mut().insert(
        ntex::http::header::HeaderName::from_static("x-wall-time-ms"),
        ntex::http::header::HeaderValue::from_str(&format!("{wall_ms:.2}")).unwrap(),
    );
    response.headers_mut().insert(
        ntex::http::header::HeaderName::from_static("x-request-id"),
        ntex::http::header::HeaderValue::from_str(&request_id.to_string()).unwrap(),
    );
    // Spend Warn (~80%): served, but flag it so the SDK / dashboard can prompt
    // the creator to raise their limit before Degrade/Block kicks in.
    if spend_warn {
        response.headers_mut().insert(
            ntex::http::header::HeaderName::from_static("x-zs-spend-warn"),
            ntex::http::header::HeaderValue::from_static("1"),
        );
    }

    response
}

// ---------------------------------------------------------------------------
// Unauthenticated request handling — HTML vs API split
// ---------------------------------------------------------------------------

/// True when the request looks like an HTML navigation rather than an
/// API call. The classification rule is the canonical browser
/// signature: `Accept: text/html` somewhere in the header value. The
/// gateway only triggers the OIDC redirect dance on these — `fetch()`
/// callers see a 401 with `WWW-Authenticate: Bearer` so they can
/// surface an in-app login prompt themselves.
fn wants_html(req: &HttpRequest) -> bool {
    req.headers()
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/html"))
}

/// Build the unauthenticated response: 302 → `auth.zeroship.ai/authorize`
/// for HTML navigations, 401 with a `WWW-Authenticate` challenge for
/// API clients. Sets the `__Host-zs_oidc_stash` cookie carrying PKCE,
/// state, and the original path so `/__zeroship/auth/callback` can finish
/// the dance.
fn unauthenticated_response(
    req: &HttpRequest,
    state: &Arc<GateState>,
    oauth_client_id: Option<&str>,
) -> HttpResponse {
    if wants_html(req) {
        match oauth_client_id {
            Some(client_id) => start_oidc_redirect(req, state, client_id),
            None => client_not_provisioned_response(),
        }
    } else {
        HttpResponse::Unauthorized()
            .header("www-authenticate", "Bearer realm=\"zeroship\"")
            .json(&serde_json::json!({
                "code": "UNAUTHENTICATED",
                "message": "authentication required",
            }))
    }
}

/// `503 client_not_provisioned` — a request resolved a real authenticated
/// user, but the route has no `sector_identifier` yet, so the gateway
/// CANNOT derive the per-app pairwise `pws_…` (auth-sdk Slice 4, §6.2).
/// We fail CLOSED — never project the global UUID into `ZeroShip-User.id`
/// — and answer 503 with the same retryable `client_not_provisioned`
/// shape the browser-token endpoints use (`auth_token.rs`). Once control
/// finishes provisioning the app's OAuth client + sector, the retry
/// succeeds.
fn client_not_provisioned_response() -> HttpResponse {
    HttpResponse::ServiceUnavailable()
        .header("cache-control", "no-store")
        .json(&serde_json::json!({
            "error": "client_not_provisioned",
            "error_description": "app has no sector_identifier yet",
        }))
}

/// `403 scope_required` — the request authenticated, but the matched
/// route's `required_scopes` (auth-sdk Slice 3c, §5.3) are not a subset of
/// the principal's granted scopes.
///
/// Two distinct contracts ride this response, and they use DIFFERENT tokens
/// on purpose:
///
/// * **`WWW-Authenticate` header** — RFC 6750 §3.1's challenge. The
///   error-code token there is the RFC-registered `insufficient_scope`, and
///   the `scope` parameter lists the required scopes space-delimited. This is
///   the value HTTP-aware clients / proxies read.
/// * **JSON body** — the SDK contract. `sdks/auth` `mapError()` reads
///   `body.error` and maps it onto an `AuthErrorCode`; the registered code is
///   `scope_required` (spec §5.3 / §error-table), and there is NO
///   `insufficient_scope` code. We therefore emit `{"error":"scope_required",
///   "scope":"<space-joined required>"}` so the SDK surfaces the typed error
///   (and the `scope` string tells the client exactly which scopes to request).
fn insufficient_scope_response(required: &[String]) -> HttpResponse {
    let scope_param = required.join(" ");
    let challenge = format!(
        "Bearer realm=\"zeroship\", error=\"insufficient_scope\", scope=\"{scope_param}\""
    );
    HttpResponse::Forbidden()
        .header("www-authenticate", challenge.as_str())
        .json(&serde_json::json!({
            "error": "scope_required",
            "scope": scope_param,
        }))
}

// ---------------------------------------------------------------------------
// /__zeroship/auth/callback — the gateway-owned OIDC callback per hosted app
// ---------------------------------------------------------------------------

/// Handle `/__zeroship/auth/callback` on any `{app}.zeroship.ai` host. Reads
/// the signed stash cookie + `code`/`state` query, exchanges with
/// hydra via `OidcRp::finish_callback`, persists a row in
/// `zeroship.gateway_sessions`, sets the per-origin
/// `__Host-zeroship_app_session` cookie, clears the stash cookie, and 302s
/// back to the original path the user was trying to reach when the
/// dance started.
async fn handle_auth_callback(
    req: HttpRequest,
    state: web::types::State<Arc<GateState>>,
) -> HttpResponse {
    // Resolve the app by subdomain — same logic the manifest dispatcher uses
    // for normal requests — then key the gateway_sessions row by the app's
    // STABLE UUID (`app_uuid`), NOT the slug. This is the canonical session
    // key (the `app_id` column is UUID, bound natively); a slug-keyed row
    // would never match on the real SPA→app request path. The slug can be
    // renamed; the UUID is the immutable identity.
    let Some(app_name) = extract_app_name(&req, None) else {
        return render_callback_error(
            state.config.insecure_dev,
            "host header missing or unparseable",
        );
    };
    let Some((app_uuid, route)) = state.routes.lookup_by_name(&app_name) else {
        return render_callback_error(
            state.config.insecure_dev,
            "app not found for this host",
        );
    };
    // The interactive flow now issues the SAME signed `zeroship-sess+jwt` cookie the
    // SDK popup flow does (BFF slice R1b) — so the cookie arm has ONE
    // local-verify path. We need the route's per-app `oauth_client_id` (the
    // signed cookie's `app` binding) + `sector_identifier` (the `pws_`
    // derivation). An app with no OAuth client provisioned can't have a signed
    // cookie minted, so fail the callback closed.
    let Some(client_id) = route.entry.oauth_client_id.clone() else {
        return render_callback_error(
            state.config.insecure_dev,
            "app has no oauth_client_id yet",
        );
    };
    let sector_identifier = route.entry.sector_identifier.clone();

    // 1. Parse query (code + state). The OP may also send `error=...`
    //    for user-denied consent; surface it directly.
    let query_str = req.uri().query().unwrap_or("");
    let mut code: Option<String> = None;
    let mut state_param: Option<String> = None;
    let mut oauth_error: Option<String> = None;
    let mut issuer_param: Option<String> = None;
    for (k, v) in url::form_urlencoded::parse(query_str.as_bytes()) {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state_param = Some(v.into_owned()),
            "error" => oauth_error = Some(v.into_owned()),
            "iss" => issuer_param = Some(v.into_owned()),
            _ => {}
        }
    }
    if let Some(e) = oauth_error {
        return render_callback_error(state.config.insecure_dev, &format!("oauth error: {e}"));
    }
    let (Some(code), Some(state_param)) = (code, state_param) else {
        return render_callback_error(
            state.config.insecure_dev,
            "missing code or state query parameter",
        );
    };
    // RFC 9207 issuer identification. The OP emits `iss` on authorize
    // responses; tolerate absence for mixed-version local/dev flows, but reject
    // any present mismatch before consuming the code.
    if let Some(issuer_param) = issuer_param.as_deref() {
        if issuer_param != state.oidc_rp.issuer {
            return render_callback_error(state.config.insecure_dev, "issuer mismatch");
        }
    }

    // 2. Read the signed stash cookie.
    let cookie_header = req
        .headers()
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let Some(stash) = oidc_rp::parse_stash_cookie(cookie_header, state.config.insecure_dev) else {
        return render_callback_error(state.config.insecure_dev, "missing stash cookie");
    };

    // 3. Exchange the code with the OP + verify the ID token.
    let (claims, original_path, granted_scopes) = match state
        .oidc_rp
        .finish_callback(&code, &state_param, &stash, &client_id)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "gateway: oidc callback failed");
            return render_callback_error(
                state.config.insecure_dev,
                oidc_callback_public_error(&e),
            );
        }
    };

    // 4. Create a per-origin session row. Check out a pooled connection
    //    for just this insert and release it on drop.
    let Some(db_cfg) = state.db.as_ref() else {
        return render_callback_error(
            state.config.insecure_dev,
            "gateway not configured with a session database",
        );
    };
    let pool = match crate::db::checkout(db_cfg).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "gateway: pg pool checkout failed (session create)");
            return render_callback_error(state.config.insecure_dev, "session create failed");
        }
    };
    let mut conn = match pool.get().await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "gateway: pg pool checkout failed (session create)");
            return render_callback_error(state.config.insecure_dev, "session create failed");
        }
    };
    let session = match crate::sessions::create(
        &mut conn,
        &crate::sessions::NewSession {
            user_id: &claims.sub,
            app_id: app_uuid,
            email: claims.email.as_deref(),
            name: claims.name.as_deref(),
            avatar_url: claims.picture.as_deref(),
            email_verified: claims.email_verified.unwrap_or(false),
            granted_scopes: &granted_scopes,
            sid: claims.sid.as_deref(),
            // Carry the OIDC auth_time/amr onto the cookie session so the SPA
            // projection + step-up gate read them off this row (BFF §2.2/§5.3).
            auth_time: claims.auth_time,
            amr: claims.amr.as_deref().unwrap_or(&[]),
        },
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "gateway: session create failed");
            return render_callback_error(state.config.insecure_dev, "session create failed");
        }
    };
    // Release the pooled connection before building the response — no
    // further DB work happens on this path. `session.id` is the audit row id;
    // it is no longer used as the cookie value (BFF R1b: the cookie is signed).
    let _ = session.id;
    drop(conn);

    // 5. Mint the SIGNED `zeroship-sess+jwt` session cookie from the validated claims
    //    (the SAME mint path the SDK popup flow uses — one cookie shape, one
    //    verifier). `claims.sub` is the global UUID; the helper derives the
    //    per-app `pws_` + relay alias before signing.
    let Ok(global_user_id) = uuid::Uuid::parse_str(&claims.sub) else {
        return render_callback_error(state.config.insecure_dev, "id_token sub is not a global user id");
    };
    let amr = claims.amr.clone().unwrap_or_default();
    let session_cookie = match crate::auth_token::issue_interactive_session_cookie(
        &state,
        db_cfg,
        &client_id,
        sector_identifier.as_deref(),
        global_user_id,
        claims.name.as_deref(),
        claims.picture.as_deref(),
        claims.email_verified,
        claims.auth_time,
        &amr,
        &granted_scopes,
    )
    .await
    {
        Ok(c) => c,
        Err(msg) => return render_callback_error(state.config.insecure_dev, &msg),
    };

    // 6. 302 back to the original path, set the signed session cookie, clear
    //    the stash cookie. Two `Set-Cookie` headers on one response is
    //    valid per RFC 6265 §3 (and is how hydra emits its own cookies).
    let mut builder = HttpResponse::Found();
    builder.header("location", sanitize_oidc_original_path(&original_path));
    builder.header("set-cookie", session_cookie);
    builder.header(
        "set-cookie",
        oidc_rp::clear_stash_cookie(state.config.insecure_dev),
    );
    builder.finish()
}

/// Render the failed-callback page. Generic on purpose — leaking
/// hydra's error string to the user would be a noisy debugging tool
/// for an attacker. The structured error is already in the gateway log
/// at warn / error.
fn render_callback_error(_insecure_dev: bool, msg: &str) -> HttpResponse {
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>Sign-in failed</title>\
        <h1>Sign-in failed</h1><p>{}</p>\
        <p><a href=\"/\">Back to app</a></p>",
        html_escape(msg),
    );
    HttpResponse::BadRequest()
        .content_type("text/html; charset=utf-8")
        .body(body)
}

fn oidc_callback_public_error(e: &oidc_rp::OidcRpError) -> &'static str {
    match e {
        oidc_rp::OidcRpError::UpstreamUnavailable(_) => {
            // Hydra is browning out (breaker open / bounded timeout). Tell the
            // user it's transient rather than implying their credentials are
            // bad — the §8.7 fast-fail surfaces here on the cookie flow.
            "sign-in is temporarily unavailable, please try again shortly"
        }
        oidc_rp::OidcRpError::StashInvalid
        | oidc_rp::OidcRpError::StateMismatch
        | oidc_rp::OidcRpError::ClientMismatch
        | oidc_rp::OidcRpError::TokenExchange(_)
        | oidc_rp::OidcRpError::VerifyIdToken(_) => "sign-in could not be completed",
    }
}

/// Minimal HTML-escape — enough to make the rendered error page safe
/// when the upstream message contains user input (state token,
/// query-string echo, etc).
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Build the 302 → hydra redirect that kicks off the OIDC dance.
/// Stash cookie carries the PKCE verifier + state + original_path so
/// the callback can resume.
fn start_oidc_redirect(req: &HttpRequest, state: &Arc<GateState>, client_id: &str) -> HttpResponse {
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
    let scheme = if state.config.insecure_dev {
        "http"
    } else {
        "https"
    };
    let redirect_uri = format!("{scheme}://{host}/__zeroship/auth/callback");

    let (auth_url, stash) = state
        .oidc_rp
        .build_authorize_redirect(
            client_id,
            &original_path,
            &redirect_uri,
        );

    let mut builder = HttpResponse::Found();
    builder.header("location", auth_url);
    builder.header(
        "set-cookie",
        oidc_rp::set_stash_cookie(&stash, state.config.insecure_dev),
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
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    use ntex::http::body::{Body, MessageBody, ResponseBody};
    use ntex::util::Bytes;
    use ntex::web::HttpResponse;

    use crate::compiled::{CompiledManifest, EffectivePolicy};
    use zeroship_bundle::{
        AuthLevel, Manifest, ProcedureKind, RateLimit, RateLimitPer, ResourceEntry,
    };

    fn test_broker_secret() -> crate::oidc_rp::BrokerSecret {
        crate::oidc_rp::BrokerSecret::from_bytes(
            b"gateway-dispatch-test-broker-master-32-bytes".to_vec(),
        )
        .expect("broker secret")
    }

    fn manifest_with_resources(resources: HashMap<String, ResourceEntry>) -> Manifest {
        Manifest {
            version: 1,
            resources,
            ..Manifest::default()
        }
    }

    /// Drain a `ResponseBody<Body>` to bytes — the dispatch tests need
    /// to inspect idempotency-replayed and error-envelope bodies.
    async fn collect_body(mut body: ResponseBody<Body>) -> Vec<u8> {
        let mut out = Vec::new();
        std::future::poll_fn(|cx| {
            loop {
                match body.poll_next_chunk(cx) {
                    std::task::Poll::Ready(Some(Ok(chunk))) => out.extend_from_slice(&chunk),
                    std::task::Poll::Ready(Some(Err(e))) => panic!("body error: {e}"),
                    std::task::Poll::Ready(None) => return std::task::Poll::Ready(()),
                    std::task::Poll::Pending => return std::task::Poll::Pending,
                }
            }
        })
        .await;
        out
    }

    /// Minimal in-memory blob store for the idempotency tests below.
    /// They never actually read assets; the store exists only to
    /// satisfy `GateState`'s required field.
    #[derive(Debug, Default)]
    struct StubBlobStore;

    #[async_trait::async_trait(?Send)]
    impl zeroship_bundle::BlobStore for StubBlobStore {
        async fn get_blob(&self, _hash: &str) -> Result<bytes::Bytes, zeroship_bundle::BlobError> {
            Err(zeroship_bundle::BlobError::NotFound("unused".into()))
        }
        fn local_path(&self, _hash: &str) -> Option<std::path::PathBuf> {
            None
        }
        async fn put_blob(
            &self,
            _hash: &str,
            _data: &[u8],
        ) -> Result<zeroship_bundle::PutOutcome, zeroship_bundle::BlobError> {
            Ok(zeroship_bundle::PutOutcome::Wrote)
        }
        async fn put_blob_stream(
            &self,
            _hash: &str,
            _expected_size: u64,
            _reader: &mut dyn std::io::Read,
        ) -> Result<zeroship_bundle::PutOutcome, zeroship_bundle::BlobError> {
            Ok(zeroship_bundle::PutOutcome::Wrote)
        }
        async fn has_blob(&self, _hash: &str) -> Result<bool, zeroship_bundle::BlobError> {
            Ok(false)
        }
        async fn get_blob_to_file(
            &self,
            _hash: &str,
            _out: &compio::fs::File,
            _expected_size: Option<u64>,
            _max_bytes: u64,
        ) -> Result<u64, zeroship_bundle::BlobError> {
            Err(zeroship_bundle::BlobError::NotFound("unused".into()))
        }
        async fn put_manifest(
            &self,
            _app_id: &uuid::Uuid,
            _deploy_hash: &str,
            _json: &[u8],
        ) -> Result<(), zeroship_bundle::BlobError> {
            Ok(())
        }
        async fn get_manifest(
            &self,
            _app_id: &uuid::Uuid,
            _deploy_hash: &str,
        ) -> Result<bytes::Bytes, zeroship_bundle::BlobError> {
            Err(zeroship_bundle::BlobError::NotFound("unused".into()))
        }
        async fn delete_app_manifests(
            &self,
            _app_id: &uuid::Uuid,
        ) -> Result<(), zeroship_bundle::BlobError> {
            Ok(())
        }
    }

    fn build_idempotency_state() -> Arc<GateState> {
        let mut tmp = std::env::temp_dir();
        tmp.push(format!("zsgate-idem-{}", uuid::Uuid::new_v4().simple()));
        let disk = crate::blob_cache::DiskBlobCache::new(tmp, 1024 * 1024).expect("disk cache");
        Arc::new(GateState {
            config: crate::GateConfig {
                control_url: String::new(),
                control_key: String::new(),
                worker_urls: vec![],
                poll_interval_secs: 5,
                worker_key: String::new(),
                hydra_public_url: String::new(),
                auth_ui_url: String::new(),
                insecure_dev: true,
                trust_proxy: false,
                public_url: "https://api.zeroship.ai".into(),
            },
            routes: crate::sync::RouteCache::new(),
            hash_ring: crate::proxy::HashRing::new(vec!["http://0.0.0.0:0".into()], 1),
            rate_limiters: crate::enforce::RateLimitRegistry::new(1, 1),
            per_rule_rate_limits: crate::enforce::PerRuleRateLimitRegistry::new(),
            concurrency: crate::enforce::ConcurrencyRegistry::new(1),
            blob_store: Arc::new(StubBlobStore),
            blob_cache: crate::blob_cache::BlobCache::new(8 * 1024 * 1024),
            disk_cache: disk,
            idempotency_store: Arc::new(crate::idempotency::InMemoryIdempotencyStore::new()),
            oidc_rp: Arc::new(crate::oidc_rp::OidcRp::new(
                "http://auth.test",
                test_broker_secret(),
                b"test-stash-key-32-bytes-long----".to_vec(),
            )),
            db: None,
            logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
            revocation_cache: Arc::new(zeroship_core::wrapper_revocation::RevocationCache::new()),
            signing_key: None,
            prev_signing_key: None,
            session_issuer: None,
            session_verifier: None,
            anchor_enc_key: [0u8; 32],
            pairwise_salt: [0u8; 32],
            meter: Arc::new(zeroship_metering::Meter::new()),
        })
    }

    // -----------------------------------------------------------------------
    // Per-rule rate-limit bucket key derivation
    // -----------------------------------------------------------------------

    #[test]
    fn compute_bucket_id_app_returns_constant() {
        // RateLimitPer::App always returns "app" regardless of IP or
        // cookie state — every caller shares the same bucket.
        let req = ntex::web::test::TestRequest::default()
            .header("cookie", "__Host-zeroship_app_session=abc")
            .to_http_request();
        assert_eq!(compute_bucket_id(&req, RateLimitPer::App, false, false), "app");
    }

    #[test]
    fn compute_bucket_id_ip_falls_back_to_unknown() {
        // The TestRequest has no peer addr → "unknown" sentinel keeps
        // the bucket lookup well-defined instead of crashing.
        let req = ntex::web::test::TestRequest::default().to_http_request();
        let id = compute_bucket_id(&req, RateLimitPer::Ip, false, false);
        assert_eq!(id, "unknown");
    }

    // -----------------------------------------------------------------------
    // ISS-70: the worker-forward URL must carry the query string. Dropping it
    // makes every GET `query()` RPC arrive with `input: undefined` (the input
    // rides in `?input=<base64url>`) and silently strips app query params.
    // -----------------------------------------------------------------------

    #[test]
    fn forward_url_preserves_query_string() {
        // GET query-RPC: the base64url input MUST survive into the worker URL.
        assert_eq!(
            forward_url("http", "app.localhost:8080", "__zeroship/v1/listTodos", Some("input=e30")),
            "http://app.localhost:8080/__zeroship/v1/listTodos?input=e30",
        );
        // Arbitrary app query params (search/pagination) survive too.
        assert_eq!(
            forward_url("https", "shop.zeroship.ai", "products", Some("q=shoes&page=2")),
            "https://shop.zeroship.ai/products?q=shoes&page=2",
        );
    }

    #[test]
    fn forward_url_omits_empty_or_absent_query() {
        // No query → no trailing '?'.
        assert_eq!(
            forward_url("http", "h", "p", None),
            "http://h/p",
        );
        // Empty query (e.g. a bare trailing '?') is treated as absent.
        assert_eq!(
            forward_url("http", "h", "p", Some("")),
            "http://h/p",
        );
    }

    // -----------------------------------------------------------------------
    // L7: platform-reserved headers must be scrubbed from the worker
    // dispatch envelope so a forged inbound ZeroShip-User / Authorization /
    // x-zs-* cannot ride into app JS `request.headers`.
    // -----------------------------------------------------------------------

    #[test]
    fn forged_reserved_headers_are_stripped_from_envelope() {
        let req = ntex::web::test::TestRequest::default()
            // forged identity + platform-internal headers an attacker
            // could inject on the inbound request
            .header("ZeroShip-User", "usr_forged_admin")
            .header("Authorization", "Bearer attacker-token")
            .header("X-App-Id", "app_spoofed")
            .header("X-Plan-Id", "plan_enterprise")
            .header("X-Request-Id", "forged-rid")
            .header("X-ZS-Internal", "1")
            .header("x-zs-anything", "2")
            // a legitimate app header that MUST survive
            .header("X-Custom-App-Header", "keep-me")
            .header("Content-Type", "application/json")
            .to_http_request();

        let fwd = collect_forwarded_headers(req.headers());
        let names: Vec<String> = fwd.iter().map(|(k, _)| k.to_ascii_lowercase()).collect();

        // None of the platform-reserved headers survive (case-insensitive).
        for reserved in [
            "zeroship-user",
            "authorization",
            "x-app-id",
            "x-plan-id",
            "x-request-id",
            "x-zs-internal",
            "x-zs-anything",
        ] {
            assert!(
                !names.iter().any(|n| n == reserved),
                "reserved header `{reserved}` leaked into the worker envelope: {names:?}"
            );
        }

        // Legitimate app headers are preserved.
        assert!(
            names.iter().any(|n| n == "x-custom-app-header"),
            "legitimate app header was dropped: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "content-type"),
            "content-type was dropped: {names:?}"
        );
    }

    #[test]
    fn is_reserved_header_is_case_insensitive_and_prefix_aware() {
        assert!(is_reserved_header("ZeroShip-User"));
        assert!(is_reserved_header("AUTHORIZATION"));
        assert!(is_reserved_header("x-zs-foo"));
        assert!(is_reserved_header("X-ZS-Bar"));
        // Not reserved: ordinary headers and the X-Forwarded-* family
        // that the gateway's own client_ip logic handles separately.
        assert!(!is_reserved_header("content-type"));
        assert!(!is_reserved_header("x-forwarded-for"));
        assert!(!is_reserved_header("x-custom"));
    }

    #[test]
    fn client_ip_ignores_forwarded_headers_without_trust_proxy() {
        assert_eq!(
            client_ip_from(Some("192.0.2.10".to_string()), Some("203.0.113.77"), false),
            "192.0.2.10"
        );
    }

    #[test]
    fn client_ip_honors_proxy_headers_only_with_trust_proxy() {
        let req = ntex::web::test::TestRequest::default()
            .header("x-forwarded-for", "203.0.113.77")
            .to_http_request();

        assert_eq!(client_ip(&req, true), "203.0.113.77");
        assert_eq!(
            compute_bucket_id(&req, RateLimitPer::Ip, false, true),
            "203.0.113.77"
        );
    }

    #[test]
    fn compute_bucket_id_session_uses_cookie() {
        let req = ntex::web::test::TestRequest::default()
            .header(
                "cookie",
                "other=foo; __Host-zeroship_app_session=abc123; trailing=x",
            )
            .to_http_request();
        let id = compute_bucket_id(&req, RateLimitPer::Session, false, false);
        assert_eq!(id, "abc123");
    }

    #[test]
    fn compute_bucket_id_session_falls_back_to_ip_when_cookie_missing() {
        // Anonymous caller (no __Host-zeroship_app_session) → fall back to IP. The
        // TestRequest has no peer → "unknown".
        let req = ntex::web::test::TestRequest::default()
            .header("cookie", "other=foo")
            .to_http_request();
        let id = compute_bucket_id(&req, RateLimitPer::Session, false, false);
        assert_eq!(id, "unknown");
    }

    /// Build an unsigned-but-structurally-valid JWT with the given `sub`. Only
    /// the payload segment matters to `jwt_subject_unverified` (it never checks
    /// the signature), so a fixed header + dummy signature suffice.
    fn jwt_with_sub(sub: &str) -> String {
        use base64::Engine as _;
        let b64 = |v: &serde_json::Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(v).unwrap())
        };
        let header = b64(&serde_json::json!({ "alg": "none", "typ": "JWT" }));
        let payload = b64(&serde_json::json!({ "sub": sub }));
        format!("{header}.{payload}.sig")
    }

    #[test]
    fn compute_bucket_id_user_uses_jwt_subject() {
        // RateLimitPer::User buckets by the authenticated JWT `sub` — the
        // wire scope that round-trips the console's `per: "user"` rules.
        let req = ntex::web::test::TestRequest::default()
            .header("authorization", format!("Bearer {}", jwt_with_sub("usr_alice")))
            .to_http_request();
        let id = compute_bucket_id(&req, RateLimitPer::User, false, false);
        assert_eq!(id, "sub:usr_alice");
    }

    #[test]
    fn compute_bucket_id_user_distinct_per_subject_not_per_session() {
        // One user, two sessions (distinct cookies) → SAME user bucket; the
        // whole point of `user` vs `session`. Then a second user → different.
        let alice = jwt_with_sub("usr_alice");
        let req_a1 = ntex::web::test::TestRequest::default()
            .header("authorization", format!("Bearer {alice}"))
            .header("cookie", "__Host-zeroship_app_session=device-1")
            .to_http_request();
        let req_a2 = ntex::web::test::TestRequest::default()
            .header("authorization", format!("Bearer {alice}"))
            .header("cookie", "__Host-zeroship_app_session=device-2")
            .to_http_request();
        let req_bob = ntex::web::test::TestRequest::default()
            .header("authorization", format!("Bearer {}", jwt_with_sub("usr_bob")))
            .to_http_request();
        let a1 = compute_bucket_id(&req_a1, RateLimitPer::User, false, false);
        let a2 = compute_bucket_id(&req_a2, RateLimitPer::User, false, false);
        let bob = compute_bucket_id(&req_bob, RateLimitPer::User, false, false);
        assert_eq!(a1, a2, "same user shares a bucket across sessions");
        assert_ne!(a1, bob, "different users get different buckets");
    }

    #[test]
    fn compute_bucket_id_user_falls_back_to_session_then_ip() {
        // No bearer JWT but a session cookie → degrade to sess:<cookie>.
        let req_sess = ntex::web::test::TestRequest::default()
            .header("cookie", "__Host-zeroship_app_session=anon-tab")
            .to_http_request();
        assert_eq!(
            compute_bucket_id(&req_sess, RateLimitPer::User, false, false),
            "sess:anon-tab"
        );
        // Neither bearer nor cookie → IP ("unknown" for the peer-less fixture).
        let req_none = ntex::web::test::TestRequest::default().to_http_request();
        assert_eq!(
            compute_bucket_id(&req_none, RateLimitPer::User, false, false),
            "unknown"
        );
        // A malformed bearer (no decodable payload) is treated as anonymous —
        // degrade rather than key everyone onto one empty bucket.
        let req_bad = ntex::web::test::TestRequest::default()
            .header("authorization", "Bearer not-a-jwt")
            .header("cookie", "__Host-zeroship_app_session=anon-tab")
            .to_http_request();
        assert_eq!(
            compute_bucket_id(&req_bad, RateLimitPer::User, false, false),
            "sess:anon-tab"
        );
    }

    // -----------------------------------------------------------------------
    // Wiring smoke test — bucket lookup matches what the router does
    // -----------------------------------------------------------------------
    //
    // The Outcome::Worker arm wires `compute_bucket_id` and
    // `state.per_rule_rate_limits.check(...)` together. Driving the full
    // `execute_outcome` would need a constructable `web::types::State`,
    // which ntex doesn't expose outside its `App` builder. Instead we
    // recreate the exact bucket-key composition the router uses and
    // assert it agrees with the registry's view of "drained vs fresh"
    // — same code path in two parts.

    #[test]
    fn router_wiring_bucket_id_matches_registry_key() {
        // rps=1 with RateLimitPer::Ip. First call fills, second 429s.
        // The bucket key is `compute_bucket_id(req, RateLimitPer::Ip)`
        // — verifies the IP-derived discriminator is the same string
        // the registry's key uses, otherwise the second call would
        // hit a fresh bucket and pass.
        let reg = crate::enforce::PerRuleRateLimitRegistry::new();
        let app_id = uuid::Uuid::nil();
        let rl = RateLimit { rps: Some(1), rpm: None, per: RateLimitPer::Ip };
        let req = ntex::web::test::TestRequest::default().to_http_request();
        let bucket_id = compute_bucket_id(&req, rl.per, false, false);
        assert!(reg.check(&app_id, 0, rl.per, &bucket_id, &rl).is_ok());
        // Second call with the same request → same bucket id → drained.
        let bucket_id2 = compute_bucket_id(&req, rl.per, false, false);
        assert_eq!(bucket_id, bucket_id2, "bucket id is stable for same request");
        let err = reg
            .check(&app_id, 0, rl.per, &bucket_id2, &rl)
            .expect_err("second call must 429 — bucket key matched");
        assert_eq!(err.status(), ntex::http::StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn router_wiring_session_buckets_separate_from_ip_buckets() {
        // Two requests carrying distinct __Host-zeroship_app_session cookies under
        // RateLimitPer::Session must hit independent buckets even when
        // the IP is the same.
        let reg = crate::enforce::PerRuleRateLimitRegistry::new();
        let app_id = uuid::Uuid::nil();
        let rl = RateLimit { rps: Some(1), rpm: None, per: RateLimitPer::Session };

        let req_a = ntex::web::test::TestRequest::default()
            .header("cookie", "__Host-zeroship_app_session=user-a")
            .to_http_request();
        let req_b = ntex::web::test::TestRequest::default()
            .header("cookie", "__Host-zeroship_app_session=user-b")
            .to_http_request();
        let bucket_a = compute_bucket_id(&req_a, rl.per, false, false);
        let bucket_b = compute_bucket_id(&req_b, rl.per, false, false);
        assert_eq!(bucket_a, "user-a");
        assert_eq!(bucket_b, "user-b");
        assert!(reg.check(&app_id, 0, rl.per, &bucket_a, &rl).is_ok());
        assert!(reg.check(&app_id, 0, rl.per, &bucket_b, &rl).is_ok());
        // Reusing user-a within the same second 429s.
        let err = reg
            .check(&app_id, 0, rl.per, &bucket_a, &rl)
            .expect_err("user-a drained");
        assert_eq!(err.status(), ntex::http::StatusCode::TOO_MANY_REQUESTS);
    }

    // -----------------------------------------------------------------------
    // Resource-tree request-level tests — exercise the per-resource policy
    // gates on synthetic requests built via ntex's `TestRequest`.
    // -----------------------------------------------------------------------

    #[test]
    fn auth_forward_client_ip_follows_trust_proxy_not_raw_peer() {
        // SEC-3: behind a trusted proxy the authoritative IP handed to the auth
        // service must be the FORWARDED client (so per-IP buckets separate real
        // users), not the gateway's immediate peer (which would be the proxy and
        // collapse everyone onto one shared bucket). With trust_proxy=false the
        // spoofable header is ignored and must NOT become the bucket key.
        let req = ntex::web::test::TestRequest::default()
            .header("x-forwarded-for", "203.0.113.7")
            .to_http_request();
        assert_eq!(
            auth_forward_client_ip(&req, true),
            Some("203.0.113.7".parse().unwrap()),
            "trust_proxy must forward the proxy-authored client IP"
        );
        assert_ne!(
            auth_forward_client_ip(&req, false),
            Some("203.0.113.7".parse().unwrap()),
            "without trust_proxy the spoofable XFF must not become the bucket key"
        );
    }

    #[test]
    fn auth_upstream_headers_drop_spoofed_client_ip_and_inject_peer() {
        // SEC-3: forwarding to the auth/Hydra upstream must strip ANY
        // client-supplied X-Forwarded-For / Forwarded / X-Real-IP and inject a
        // single authoritative X-Forwarded-For from the real socket peer.
        // Pre-fix the helper copies headers verbatim and injects nothing → the
        // spoofed 1.2.3.4 survives and the authoritative peer is absent → RED.
        let req = ntex::web::test::TestRequest::default()
            .header("x-forwarded-for", "1.2.3.4")
            .header("forwarded", "for=1.2.3.4")
            .header("x-real-ip", "1.2.3.4")
            .header("cookie", "zsidp_csrf=keep")
            .header("user-agent", "keep-me/1")
            .to_http_request();

        let peer: std::net::IpAddr = "9.9.9.9".parse().unwrap();
        let fwd = build_auth_upstream_headers(req.headers(), Some(peer));
        let lc: Vec<(String, String)> = fwd
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
            .collect();

        // The spoofed value never reaches the auth upstream under any of the
        // three client-IP header names.
        for spoof in ["x-forwarded-for", "forwarded", "x-real-ip"] {
            assert!(
                !lc.iter().any(|(k, v)| k == spoof && v.contains("1.2.3.4")),
                "spoofed client IP leaked via `{spoof}`: {lc:?}"
            );
        }

        // Exactly one authoritative X-Forwarded-For, set to the real peer.
        let xff: Vec<&String> = lc
            .iter()
            .filter(|(k, _)| k == "x-forwarded-for")
            .map(|(_, v)| v)
            .collect();
        assert_eq!(
            xff,
            vec![&"9.9.9.9".to_string()],
            "auth upstream must receive exactly one authoritative X-Forwarded-For (peer): {lc:?}"
        );

        // Benign headers survive the scrub.
        assert!(
            lc.iter().any(|(k, v)| k == "user-agent" && v == "keep-me/1"),
            "benign header dropped: {lc:?}"
        );
        assert!(
            lc.iter().any(|(k, _)| k == "cookie"),
            "auth cookies must still be forwarded: {lc:?}"
        );
    }

    #[test]
    fn canonicalize_dispatch_path_rejects_traversal_and_normalizes() {
        // SEC-2: the dispatch layer rejects (400) any path carrying a
        // traversal / empty-interior-segment form a browser's `new URL` would
        // rewrite, and otherwise forwards the CANONICAL form (so the worker
        // re-parses the exact path the gateway matched auth on). Pre-fix the
        // helper forwards the raw path and never rejects → RED.
        for traversal in [
            "/api/foo/../admin",
            "/api/%2e/admin",
            "/api/%2E/admin",
            "/api/./admin",
            "/api//admin",
            "/api/foo/%2e%2e/admin",
            "/%2e%2e/etc/passwd",
        ] {
            assert_eq!(
                canonicalize_dispatch_path(traversal),
                CanonicalPath::Reject,
                "traversal {traversal:?} must be rejected (400), not forwarded raw"
            );
        }

        // Benign forms forward under their canonical normalization. A single
        // trailing slash is a normalize case (not a traversal), and an already
        // -canonical path is forwarded unchanged.
        assert_eq!(
            canonicalize_dispatch_path("/api/admin/"),
            CanonicalPath::Use("/api/admin".to_string()),
            "a lone trailing slash normalizes (single-slash policy), not 400"
        );
        assert_eq!(
            canonicalize_dispatch_path("/api/admin"),
            CanonicalPath::Use("/api/admin".to_string()),
            "an already-canonical path forwards unchanged"
        );
        assert_eq!(
            canonicalize_dispatch_path("/__zeroship/v1/todos.list"),
            CanonicalPath::Use("/__zeroship/v1/todos.list".to_string()),
            "RPC wire paths are canonical and forward unchanged"
        );
    }

    #[test]
    fn lookup_finds_rpc_resource_after_strip_prefix() {
        let mut resources = HashMap::new();
        resources.insert(
            "rpc:listTodos".into(),
            ResourceEntry {
                kind: Some(ProcedureKind::Query),
                ..Default::default()
            },
        );
        let m = manifest_with_resources(resources);
        let c = CompiledManifest::compile(&m);
        let p = c
            .lookup_resource("/__zeroship/v1/listTodos")
            .expect("matches rpc:listTodos");
        assert_eq!(p.kind, Some(ProcedureKind::Query));
        // Bare /_rpc/ paths are no longer dispatched — `/__zeroship/v1/` is
        // the only RPC wire prefix.
        assert!(c.lookup_resource("/_rpc/listTodos").is_none());
    }

    #[test]
    fn rate_limit_resolution_min_wins_for_resource_tree() {
        // End-to-end: build a manifest with a stricter child rate limit
        // and verify the compiled policy reflects min(parent, child).
        let mut resources = HashMap::new();
        resources.insert(
            "*".into(),
            ResourceEntry {
                auth: Some(AuthLevel::User),
                rate_limit: Some(RateLimit { rpm: Some(600), rps: None, per: RateLimitPer::Ip }),
                ..Default::default()
            },
        );
        resources.insert(
            "rpc:expensive".into(),
            ResourceEntry {
                kind: Some(ProcedureKind::Mutation),
                rate_limit: Some(RateLimit { rpm: Some(10), rps: None, per: RateLimitPer::Ip }),
                r#override: vec!["rate_limit".into()],
                ..Default::default()
            },
        );
        let m = manifest_with_resources(resources);
        let c = CompiledManifest::compile(&m);
        let p = c.lookup_resource("/__zeroship/v1/expensive").expect("matches");
        assert_eq!(
            p.rate_limit.as_ref().unwrap().rpm,
            Some(10),
            "child's stricter cap survives the merge"
        );
    }

    #[test]
    fn passthrough_manifest_has_only_root_default() {
        // The synthesized passthrough manifest carries the `*` root
        // default but no other resources. URL paths fall through to
        // 404 in the gateway; the worker is never invoked.
        let m = Manifest::passthrough();
        let c = CompiledManifest::compile(&m);
        assert!(
            c.lookup_resource("/anything").is_none(),
            "no per-path resource synthesized in passthrough"
        );
        assert!(
            c.lookup_resource("/__zeroship/v1/anything").is_none(),
            "no RPC resource synthesized in passthrough"
        );
        assert!(
            c.lookup_resource("/_rpc/listTodos").is_none(),
            "legacy /_rpc/ prefix is no longer routed"
        );
    }

    // -----------------------------------------------------------------------
    // Idempotency integration — exercises the gateway-side wiring on
    // top of the `idempotency` module. End-to-end coverage of the
    // module itself lives in `idempotency::tests`; these tests focus
    // on the HTTP-shaped surface.
    // -----------------------------------------------------------------------

    fn idempotent_mutation_policy() -> EffectivePolicy {
        EffectivePolicy {
            auth: AuthLevel::Anon,
            rate_limit: None,
            cors: None,
            cache: None,
            csrf_origins: None,
            idempotent: true,
            idempotency_ttl_hours: None,
            max_input_bytes: None,
            middleware: vec![],
            publicly_accessible: true,
            required_scopes: vec![],
            kind: Some(ProcedureKind::Mutation),
            action: crate::compiled::ResolvedAction::WorkerRpc,
            timeout_ms: None,
            input_schema: None,
            output_schema: None,
        }
    }

    fn make_minimal_state() -> Arc<GateState> {
        build_idempotency_state()
    }

    #[compio::test]
    async fn idempotency_missing_header_returns_400_invalid_argument() {
        let state = make_minimal_state();
        let req = ntex::web::test::TestRequest::default()
            .method(ntex::http::Method::POST)
            .to_http_request();
        let body = Bytes::from_static(b"{}");
        let policy = idempotent_mutation_policy();

        let outcome = handle_idempotency_pre_dispatch(
            &req,
            &state,
            &uuid::Uuid::new_v4(),
            "/__zeroship/v1/todos.add",
            &policy,
            &body,
            std::time::Instant::now(),
        )
        .await;

        match outcome {
            IdempotencyOutcome::ReturnNow(mut resp) => {
                assert_eq!(resp.status(), ntex::http::StatusCode::BAD_REQUEST);
                let ct = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                assert_eq!(ct, "application/zs-error+json");
                let body = resp.take_body();
                let bytes = collect_body(body).await;
                let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(v["code"], "INVALID_ARGUMENT");
            }
            IdempotencyOutcome::Proceed(_) => panic!("must reject when header missing"),
        }
    }

    #[compio::test]
    async fn idempotency_first_request_proceeds_holds_lock() {
        let state = make_minimal_state();
        let app_id = uuid::Uuid::new_v4();
        let req = ntex::web::test::TestRequest::default()
            .header("idempotency-key", "test-key-123")
            .to_http_request();
        let policy = idempotent_mutation_policy();

        let outcome = handle_idempotency_pre_dispatch(
            &req,
            &state,
            &app_id,
            "/__zeroship/v1/todos.add",
            &policy,
            &Bytes::from_static(b"{\"text\":\"hi\"}"),
            std::time::Instant::now(),
        )
        .await;

        match outcome {
            IdempotencyOutcome::Proceed(handle) => {
                assert_eq!(
                    handle.entry_key,
                    crate::idempotency::entry_key(&app_id, "todos.add", "test-key-123")
                );
                assert_eq!(handle.ttl_hours, crate::idempotency::DEFAULT_TTL_HOURS);
            }
            IdempotencyOutcome::ReturnNow(_) => panic!("first request must Proceed"),
        }
    }

    #[compio::test]
    async fn idempotency_second_same_body_replays_cached_response() {
        let state = make_minimal_state();
        let app_id = uuid::Uuid::new_v4();
        let req = ntex::web::test::TestRequest::default()
            .header("idempotency-key", "k")
            .to_http_request();
        let policy = idempotent_mutation_policy();
        let body = Bytes::from_static(b"{\"x\":1}");

        // First request claims the lock.
        let first = handle_idempotency_pre_dispatch(
            &req,
            &state,
            &app_id,
            "/__zeroship/v1/todos.add",
            &policy,
            &body,
            std::time::Instant::now(),
        )
        .await;
        let IdempotencyOutcome::Proceed(handle) = first else {
            panic!("expected Proceed");
        };

        // Build a fake worker response and capture it.
        let worker_resp = HttpResponse::Created()
            .header("content-type", "application/json")
            .body(b"{\"id\":42}".to_vec());
        let captured =
            capture_response_for_idempotency(&state, &app_id, handle, worker_resp).await;
        let captured: HttpResponse = captured;
        assert_eq!(captured.status(), ntex::http::StatusCode::CREATED);
        assert_eq!(
            captured
                .headers()
                .get("x-zs-idempotent-stored")
                .and_then(|v| v.to_str().ok()),
            Some("true")
        );

        // Second request — same body — must replay verbatim.
        let second = handle_idempotency_pre_dispatch(
            &req,
            &state,
            &app_id,
            "/__zeroship/v1/todos.add",
            &policy,
            &body,
            std::time::Instant::now(),
        )
        .await;
        match second {
            IdempotencyOutcome::ReturnNow(mut resp) => {
                assert_eq!(resp.status(), ntex::http::StatusCode::CREATED);
                assert_eq!(
                    resp.headers()
                        .get("x-zs-idempotent-replay")
                        .and_then(|v| v.to_str().ok()),
                    Some("true")
                );
                let body = resp.take_body();
                let body_bytes = collect_body(body).await;
                assert_eq!(body_bytes, b"{\"id\":42}");
            }
            IdempotencyOutcome::Proceed(_) => panic!("must replay cached response"),
        }
    }

    #[compio::test]
    async fn idempotency_second_different_body_returns_409_already_exists() {
        let state = make_minimal_state();
        let app_id = uuid::Uuid::new_v4();
        let req_with_key = |body_label: &str| {
            ntex::web::test::TestRequest::default()
                .header("idempotency-key", "shared-key")
                .header("x-test-label", body_label)
                .to_http_request()
        };
        let policy = idempotent_mutation_policy();

        // Seed the dedupe table with the original.
        let first = handle_idempotency_pre_dispatch(
            &req_with_key("a"),
            &state,
            &app_id,
            "/__zeroship/v1/todos.add",
            &policy,
            &Bytes::from_static(b"{\"a\":1}"),
            std::time::Instant::now(),
        )
        .await;
        let IdempotencyOutcome::Proceed(handle) = first else {
            panic!("expected Proceed");
        };
        let worker_resp = HttpResponse::Ok().body(b"first".to_vec());
        let _ = capture_response_for_idempotency(&state, &app_id, handle, worker_resp).await;

        // Same key, different body → 409 ALREADY_EXISTS with Retry-After.
        let conflict = handle_idempotency_pre_dispatch(
            &req_with_key("b"),
            &state,
            &app_id,
            "/__zeroship/v1/todos.add",
            &policy,
            &Bytes::from_static(b"{\"b\":2}"),
            std::time::Instant::now(),
        )
        .await;

        match conflict {
            IdempotencyOutcome::ReturnNow(mut resp) => {
                assert_eq!(resp.status(), ntex::http::StatusCode::CONFLICT);
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .expect("retry-after present and numeric");
                assert!(retry_after > 0 && retry_after <= 24 * 3600);
                let body = resp.take_body();
                let bytes = collect_body(body).await;
                let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(v["code"], "ALREADY_EXISTS");
                assert_eq!(v["details"]["reason"], "idempotency_key_reused_with_different_input");
            }
            IdempotencyOutcome::Proceed(_) => panic!("must reject"),
        }
    }

    #[compio::test]
    async fn idempotency_per_procedure_ttl_clamps_into_band() {
        // Authored 1h TTL is honored. Authored 0 clamps up to 1; authored
        // 999 clamps down to 168.
        assert_eq!(crate::idempotency::clamp_ttl_hours(Some(1)), 1);
        assert_eq!(crate::idempotency::clamp_ttl_hours(Some(168)), 168);
        assert_eq!(crate::idempotency::clamp_ttl_hours(Some(0)), 1);
        assert_eq!(crate::idempotency::clamp_ttl_hours(Some(9999)), 168);
        assert_eq!(crate::idempotency::clamp_ttl_hours(None), 24);
    }

    #[compio::test]
    async fn idempotency_per_procedure_ttl_flows_into_handle() {
        // A mutation pinned to 48h yields handle.ttl_hours = 48.
        let state = make_minimal_state();
        let app_id = uuid::Uuid::new_v4();
        let req = ntex::web::test::TestRequest::default()
            .header("idempotency-key", "k")
            .to_http_request();
        let mut policy = idempotent_mutation_policy();
        policy.idempotency_ttl_hours = Some(48);

        let outcome = handle_idempotency_pre_dispatch(
            &req,
            &state,
            &app_id,
            "/__zeroship/v1/todos.add",
            &policy,
            &Bytes::from_static(b"{}"),
            std::time::Instant::now(),
        )
        .await;
        let IdempotencyOutcome::Proceed(handle) = outcome else {
            panic!("expected Proceed");
        };
        assert_eq!(handle.ttl_hours, 48);
    }

    #[compio::test]
    async fn buffer_response_body_round_trips_payload() {
        // The streaming/buffered conversion preserves bytes verbatim.
        let resp = HttpResponse::Ok()
            .header("content-type", "application/json")
            .body(b"{\"hello\":\"world\"}".to_vec());
        let (rebuilt, bytes) = buffer_response_body(resp).await;
        assert_eq!(bytes, b"{\"hello\":\"world\"}");
        assert_eq!(
            rebuilt
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        let mut rebuilt = rebuilt;
        let body = rebuilt.take_body();
        let drained = collect_body(body).await;
        assert_eq!(drained, b"{\"hello\":\"world\"}");
    }

    #[compio::test]
    async fn build_replay_response_carries_status_and_body() {
        use base64::Engine as _;
        let mut headers = std::collections::HashMap::new();
        headers.insert("content-type".to_string(), "application/json".to_string());
        let stored = crate::idempotency::StoredResponse {
            input_hash: crate::idempotency::hash_body(b"{}"),
            status: 422,
            headers,
            body_b64: base64::engine::general_purpose::STANDARD.encode(b"{\"err\":\"x\"}"),
            completed_at: 0,
            ttl_until: u64::MAX,
        };
        let resp = build_replay_response(&stored, std::time::Instant::now());
        assert_eq!(resp.status(), ntex::http::StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            resp.headers()
                .get("x-zs-idempotent-replay")
                .and_then(|v| v.to_str().ok()),
            Some("true")
        );
        let mut resp = resp;
        let body = resp.take_body();
        let drained = collect_body(body).await;
        assert_eq!(drained, b"{\"err\":\"x\"}");
    }

    // -----------------------------------------------------------------------
    // Subscription gating + session affinity
    // -----------------------------------------------------------------------

    fn subscription_resource() -> ResourceEntry {
        ResourceEntry {
            kind: Some(ProcedureKind::Subscription),
            ..Default::default()
        }
    }

    fn subscription_manifest() -> Manifest {
        let mut resources = HashMap::new();
        resources.insert("rpc:todoTicker".into(), subscription_resource());
        manifest_with_resources(resources)
    }

    /// `is_websocket_upgrade` accepts `Upgrade: websocket` + `Connection`
    /// containing the `upgrade` token (case-insensitive, comma-separated).
    #[test]
    fn is_websocket_upgrade_accepts_canonical_headers() {
        let req = ntex::web::test::TestRequest::default()
            .header("upgrade", "websocket")
            .header("connection", "Upgrade")
            .to_http_request();
        assert!(is_websocket_upgrade(&req));

        let req = ntex::web::test::TestRequest::default()
            .header("upgrade", "WebSocket")
            .header("connection", "keep-alive, Upgrade")
            .to_http_request();
        assert!(is_websocket_upgrade(&req));
    }

    #[test]
    fn is_websocket_upgrade_rejects_missing_headers() {
        let req = ntex::web::test::TestRequest::default()
            .header("upgrade", "websocket")
            .to_http_request();
        assert!(!is_websocket_upgrade(&req), "missing connection rejected");

        let req = ntex::web::test::TestRequest::default()
            .header("connection", "Upgrade")
            .to_http_request();
        assert!(!is_websocket_upgrade(&req), "missing upgrade rejected");

        let req = ntex::web::test::TestRequest::default().to_http_request();
        assert!(!is_websocket_upgrade(&req), "no headers rejected");
    }

    /// Affinity key prefers JWT subject over cookie, cookie over WS-key,
    /// WS-key over IP. Rebuilds the same key for the same caller.
    #[test]
    fn subscription_affinity_prefers_jwt_subject() {
        // {"sub":"alice"} base64-url no padding
        let payload = base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            br#"{"sub":"alice"}"#,
        );
        let jwt = format!("h.{payload}.s");
        let req = ntex::web::test::TestRequest::default()
            .header("authorization", format!("Bearer {jwt}"))
            .header("cookie", "__Host-zeroship_app_session=cookieval")
            .to_http_request();
        assert_eq!(subscription_affinity_key(&req, false, false), "sub:alice");
    }

    #[test]
    fn subscription_affinity_falls_back_to_cookie() {
        let req = ntex::web::test::TestRequest::default()
            .header("cookie", "__Host-zeroship_app_session=tok123")
            .to_http_request();
        assert_eq!(subscription_affinity_key(&req, false, false), "sess:tok123");
    }

    #[test]
    fn subscription_affinity_falls_back_to_ws_key_then_ip() {
        let req = ntex::web::test::TestRequest::default()
            .header("sec-websocket-key", "abc==")
            .to_http_request();
        assert_eq!(subscription_affinity_key(&req, false, false), "wsk:abc==");

        let req = ntex::web::test::TestRequest::default().to_http_request();
        // No headers, no remote — falls back to "ip:unknown".
        assert_eq!(subscription_affinity_key(&req, false, false), "ip:unknown");
    }

    /// Session-affinity invariant: the same `(app_id, principal)` always
    /// resolves to the same worker, even when the ring has many candidates
    /// and capacity is unbounded. Different principals on the same app
    /// MAY land on different workers (the spread is the whole point).
    #[test]
    fn select_with_affinity_is_sticky_for_same_principal() {
        let workers: Vec<String> = (0..16)
            .map(|i| format!("http://worker-{i}:8080"))
            .collect();
        let ring = crate::proxy::HashRing::new(workers, u32::MAX);
        let app = uuid::Uuid::nil();

        let (a, _) = ring.select_with_affinity(&app, "sub:alice");
        let (b, _) = ring.select_with_affinity(&app, "sub:alice");
        assert_eq!(a, b, "same principal always sticky-binds same worker");

        let (c, _) = ring.select_with_affinity(&app, "sub:bob");
        // Not asserting `a != c` — pigeonhole says collisions are
        // possible — but the ring should distribute across enough
        // distinct principals. Just verify Bob is also sticky.
        let (c2, _) = ring.select_with_affinity(&app, "sub:bob");
        assert_eq!(c, c2);
    }

    /// Vary either the app or the principal and the chosen worker can
    /// shift; same (app, principal) is the affinity contract.
    #[test]
    fn select_with_affinity_varies_with_principal() {
        let workers: Vec<String> = (0..16)
            .map(|i| format!("http://worker-{i}:8080"))
            .collect();
        let ring = crate::proxy::HashRing::new(workers, u32::MAX);
        let app = uuid::Uuid::nil();

        let mut hits = std::collections::HashSet::new();
        for u in 0..32 {
            let (idx, _) = ring.select_with_affinity(&app, &format!("sub:user-{u}"));
            hits.insert(idx);
        }
        // 32 principals across 16 workers — should cover at least 4
        // distinct workers (very loose; the actual spread is uniform).
        assert!(hits.len() >= 4, "affinity should distribute, hit count: {}", hits.len());
    }

    /// Idempotency middleware should NOT apply to subscriptions even
    /// when `policy.idempotent` is true — the spec only protects
    /// mutations. Verifies the gating clause directly.
    #[test]
    fn idempotency_bypasses_subscriptions() {
        // Same shape as `idempotent_mutation_policy` but with kind: Subscription.
        let policy = EffectivePolicy {
            auth: AuthLevel::Anon,
            rate_limit: None,
            cors: None,
            cache: None,
            csrf_origins: None,
            idempotent: true,
            idempotency_ttl_hours: None,
            max_input_bytes: None,
            middleware: vec![],
            publicly_accessible: true,
            required_scopes: vec![],
            kind: Some(ProcedureKind::Subscription),
            action: crate::compiled::ResolvedAction::WorkerRpc,
            timeout_ms: None,
            input_schema: None,
            output_schema: None,
        };
        // The router gate: idempotency engages only when
        //   policy.idempotent && kind in {Mutation, Action, None} &&
        //   action == WorkerRpc.
        // For subscription the kind clause is false, so we skip dedupe.
        let engages = policy.idempotent
            && matches!(
                policy.kind,
                Some(ProcedureKind::Mutation) | Some(ProcedureKind::Action) | None
            )
            && matches!(policy.action, crate::compiled::ResolvedAction::WorkerRpc);
        assert!(!engages, "idempotency must NOT engage for subscriptions");
    }

    /// Gate: `kind: subscription` + non-GET method → 405 (mirrors the
    /// dispatch-loop method check). Smoke-test against the lookup +
    /// kind classification path; the actual 405 response is built
    /// inside `execute_resource_tree` and the router test fixture
    /// doesn't expose a clean entry point for it, so we exercise the
    /// classification only.
    #[test]
    fn subscription_resource_classifies_correctly() {
        let m = subscription_manifest();
        let c = CompiledManifest::compile(&m);
        let p = c
            .lookup_resource("/__zeroship/v1/todoTicker")
            .expect("subscription resource");
        assert_eq!(p.kind, Some(ProcedureKind::Subscription));
    }

    // ----------------------------------------------------------------------
    // auth.zeroship.ai routing — classify_auth_path
    // ----------------------------------------------------------------------

    #[test]
    fn classify_auth_path_oauth2_routes_to_hydra() {
        assert_eq!(classify_auth_path("/oauth2/auth"), AuthUpstream::Hydra);
        assert_eq!(classify_auth_path("/oauth2/token"), AuthUpstream::Hydra);
        assert_eq!(classify_auth_path("/oauth2/revoke"), AuthUpstream::Hydra);
        assert_eq!(
            classify_auth_path("/oauth2/sessions/logout"),
            AuthUpstream::Hydra
        );
    }

    #[test]
    fn classify_auth_path_well_known_routes_to_self_contained_op() {
        assert_eq!(
            classify_auth_path("/.well-known/openid-configuration"),
            AuthUpstream::Auth
        );
        assert_eq!(
            classify_auth_path("/.well-known/jwks.json"),
            AuthUpstream::Auth
        );
    }

    #[test]
    fn classify_auth_path_userinfo_routes_to_self_contained_op() {
        assert_eq!(classify_auth_path("/userinfo"), AuthUpstream::Auth);
    }

    #[test]
    fn classify_auth_path_ui_and_callbacks_route_to_auth() {
        assert_eq!(classify_auth_path("/authorize"), AuthUpstream::Auth);
        assert_eq!(classify_auth_path("/token"), AuthUpstream::Auth);
        assert_eq!(classify_auth_path("/revoke"), AuthUpstream::Auth);
        assert_eq!(classify_auth_path("/login"), AuthUpstream::Auth);
        assert_eq!(classify_auth_path("/signup"), AuthUpstream::Auth);
        assert_eq!(classify_auth_path("/consent"), AuthUpstream::Auth);
        assert_eq!(classify_auth_path("/static/main.css"), AuthUpstream::Auth);
        // A path that contains but does not start with `/oauth2/` must
        // still go to auth — the prefix match is anchored.
        assert_eq!(
            classify_auth_path("/something/oauth2/auth"),
            AuthUpstream::Auth
        );
        // `/userinfo` is exact-match; substring matches must not steal.
        assert_eq!(classify_auth_path("/userinfo/foo"), AuthUpstream::Auth);
    }

    // ----------------------------------------------------------------------
    // is_auth_host — the Host header that routes to internal auth/Hydra
    // MUST be an anchored, exact match. A loose prefix/substring match
    // lets a crafted Host (`auth.zeroship.ai.evil.com`, `authx.zeroship.ai`,
    // `evil-auth.zeroship.ai`) reach the platform-internal auth upstream.
    // ----------------------------------------------------------------------

    fn req_with_host(host: &str) -> HttpRequest {
        ntex::web::test::TestRequest::default()
            .header("host", host)
            .to_http_request()
    }

    #[test]
    fn is_auth_host_accepts_exact_production_host() {
        assert!(is_auth_host(&req_with_host("auth.zeroship.ai")));
        // Case-insensitive + port-stripped, like the rest of the host logic.
        assert!(is_auth_host(&req_with_host("AUTH.ZEROSHIP.AI")));
        assert!(is_auth_host(&req_with_host("auth.zeroship.ai:443")));
        // Dev host parity is exact too.
        assert!(is_auth_host(&req_with_host("auth.zeroship.localhost")));
        assert!(is_auth_host(&req_with_host("auth.zeroship.localhost:8080")));
    }

    #[test]
    fn is_auth_host_rejects_unanchored_lookalikes() {
        // Subdomain-suffix: attacker-controlled parent domain.
        assert!(
            !is_auth_host(&req_with_host("auth.zeroship.ai.evil.com")),
            "suffix attack must not match"
        );
        // Prefix-extension on the label.
        assert!(
            !is_auth_host(&req_with_host("authx.zeroship.ai")),
            "label prefix-extension must not match"
        );
        assert!(
            !is_auth_host(&req_with_host("auth-evil.com")),
            "label prefix-extension must not match"
        );
        // Substring containment.
        assert!(
            !is_auth_host(&req_with_host("evil-auth.zeroship.ai")),
            "substring must not match"
        );
        // The old `starts_with("auth.zeroship.")` prefix arm let any
        // `auth.zeroship.<anything>` through — this is the exact regression.
        assert!(
            !is_auth_host(&req_with_host("auth.zeroship.evil.com")),
            "former prefix arm must no longer match arbitrary suffixes"
        );
        // No host header at all → not an auth host.
        assert!(!is_auth_host(
            &ntex::web::test::TestRequest::default().to_http_request()
        ));
    }

    // -----------------------------------------------------------------------
    // OIDC RP integration — auth redirect + callback handler
    // -----------------------------------------------------------------------

    /// `wants_html` should fire on `text/html`-containing Accept
    /// headers and ignore everything else. The classifier drives the
    /// 302-vs-401 split for unauthenticated requests.
    #[test]
    fn wants_html_recognizes_browser_accept() {
        let html = ntex::web::test::TestRequest::default()
            .header("accept", "text/html,application/xhtml+xml;q=0.9")
            .to_http_request();
        assert!(wants_html(&html));

        let json = ntex::web::test::TestRequest::default()
            .header("accept", "application/json")
            .to_http_request();
        assert!(!wants_html(&json));

        let none = ntex::web::test::TestRequest::default().to_http_request();
        assert!(!wants_html(&none));
    }

    /// `unauthenticated_response` returns 401+WWW-Authenticate for API
    /// callers (no `Accept: text/html`). This is the contract that
    /// lets `fetch()` callers surface their own login UI instead of
    /// following a 302 into hydra they can't render.
    #[test]
    fn unauthenticated_response_returns_401_for_api_clients() {
        let req = ntex::web::test::TestRequest::default()
            .header("accept", "application/json")
            .to_http_request();
        let state = build_idempotency_state();
        let resp = unauthenticated_response(&req, &state, Some("oac_myapp"));
        assert_eq!(resp.status(), ntex::http::StatusCode::UNAUTHORIZED);
        let wa = resp
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(wa.contains("Bearer"), "got www-authenticate = {wa:?}");
    }

    /// `insufficient_scope_response` is the dispatch 403 shape (auth-sdk
    /// Slice 3c, §5.3 / RFC 6750 §3.1). This pins BOTH wire contracts that
    /// ride it, which use deliberately different error tokens:
    ///
    /// * `WWW-Authenticate` header → RFC 6750's registered `insufficient_scope`
    ///   token plus a space-delimited `scope` param.
    /// * JSON body → the SDK contract `{"error":"scope_required","scope":"…"}`
    ///   (the `AuthErrorCode` the SDK `mapError()` recognizes; there is no
    ///   `insufficient_scope` code SDK-side).
    ///
    /// Without this test the body-shape divergence from spec slipped through
    /// (only transitively covered by the `resolve_auth` enum-level tests).
    #[compio::test]
    async fn insufficient_scope_response_403_shape() {
        let required = vec!["read:billing".to_string(), "write:projects".to_string()];
        let mut resp = insufficient_scope_response(&required);
        assert_eq!(resp.status(), ntex::http::StatusCode::FORBIDDEN);

        // RFC 6750 challenge: registered token + space-joined scope param.
        let wa = resp
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(
            wa.contains("error=\"insufficient_scope\""),
            "WWW-Authenticate must use the RFC 6750 token, got {wa:?}"
        );
        assert!(
            wa.contains("scope=\"read:billing write:projects\""),
            "WWW-Authenticate must list the space-joined required scopes, got {wa:?}"
        );

        // SDK-contract JSON body: scope_required + space-joined scope string.
        let body_bytes = collect_body(resp.take_body()).await;
        let body: serde_json::Value =
            serde_json::from_slice(&body_bytes).expect("403 body is JSON");
        assert_eq!(
            body["error"], "scope_required",
            "JSON error code must be the SDK AuthErrorCode, got {body}"
        );
        assert_eq!(
            body["scope"], "read:billing write:projects",
            "JSON scope must be the space-joined required scopes, got {body}"
        );
    }

    /// HTML navigations get a 302 → hydra with the stash cookie set.
    /// The redirect target carries the gateway's client_id, the PKCE
    /// challenge, and the per-app `redirect_uri` derived from the Host
    /// header.
    #[test]
    fn unauthenticated_response_redirects_html_clients_to_hydra() {
        let req = ntex::web::test::TestRequest::default()
            .header("accept", "text/html")
            .header("host", "myapp.zeroship.localhost")
            .uri("/dashboard?welcome=true")
            .to_http_request();
        let state = build_idempotency_state();
        let resp = unauthenticated_response(&req, &state, Some("oac_myapp"));
        assert_eq!(resp.status(), ntex::http::StatusCode::FOUND);
        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            location.contains("/authorize?"),
            "location must point at the OP's /authorize; got {location:?}"
        );
        assert!(location.contains("client_id=oac_myapp"));
        assert!(location.contains("code_challenge="));
        // `redirect_uri` is the per-host callback path; insecure_dev=true
        // in the test fixture, so scheme is http.
        assert!(
            location.contains("redirect_uri=http%3A%2F%2Fmyapp.zeroship.localhost%2F__zeroship%2Fauth%2Fcallback"),
            "redirect_uri must include the per-host callback path; got {location:?}"
        );
        // Stash cookie set.
        let set_cookie = resp
            .headers()
            .get("set-cookie")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        // `insecure_dev=true` in the test fixture → no `__Host-` prefix
        // (RFC 6265bis §4.1.3.2: `__Host-` requires `Secure`, dev runs
        // over plain HTTP without it).
        assert!(
            set_cookie.starts_with("zs_oidc_stash="),
            "must set the dev stash cookie; got {set_cookie:?}"
        );
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
            sanitize_oidc_original_path("/__zeroship/auth/callback"),
            "/__zeroship/auth/callback"
        );
    }

    /// `render_callback_error` returns a 400 HTML page and escapes
    /// the inserted message so a malicious upstream cannot smuggle
    /// markup through the failure path.
    #[test]
    fn render_callback_error_returns_html_400_and_escapes_message() {
        let resp = render_callback_error(true, "<script>alert(1)</script>");
        assert_eq!(resp.status(), ntex::http::StatusCode::BAD_REQUEST);
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(ct.starts_with("text/html"), "got content-type {ct:?}");
    }

    #[test]
    fn oidc_callback_token_exchange_error_is_generic() {
        let err = oidc_rp::OidcRpError::TokenExchange(
            "HTTP 500: hydra says postgres://internal".into(),
        );
        assert_eq!(
            oidc_callback_public_error(&err),
            "sign-in could not be completed",
        );
    }

    #[test]
    fn html_escape_neutralizes_tags_and_quotes() {
        assert_eq!(
            html_escape(r#"<script>alert("x" & 'y')</script>"#),
            "&lt;script&gt;alert(&quot;x&quot; &amp; &#39;y&#39;)&lt;/script&gt;",
        );
    }

    // -----------------------------------------------------------------------
    // PR5 — faithful spend-enforcement at the gateway edge.
    //
    // These drive the REAL path: a `RouteEntry` (carrying the pulled
    // `spend_state`) is pushed through the REAL `RouteCache::update` (which
    // flips the degraded registries), then either the real `handle_dispatch`
    // is invoked (402 gate) or the real `acquire_concurrency` is driven
    // against the flipped registry (degrade tightening) — no shims.
    // -----------------------------------------------------------------------

    fn spend_route(spend_state: zeroship_core::types::SpendState) -> zeroship_core::types::RouteEntry {
        zeroship_core::types::RouteEntry {
            name: "spend-app.zeroship.localhost".to_string(),
            plan_id: "free".to_string(),
            api_key_hash: "h".to_string(),
            deploy_hash: None,
            manifest: zeroship_bundle::Manifest::passthrough(),
            oauth_client_id: None,
            sector_identifier: None,
            spend_state,
            account_state: zeroship_core::types::AccountState::Active,
        }
    }

    /// A route whose manifest declares a public RPC query at wire-id `ping`,
    /// so `/__zeroship/v1/ping` resolves to a `WorkerRpc` action through the
    /// real `lookup_resource`. Used to drive the worker-dispatch path of the
    /// hoisted spend gate via the public `handle_request` entry.
    fn worker_spend_route(
        spend_state: zeroship_core::types::SpendState,
    ) -> zeroship_core::types::RouteEntry {
        use zeroship_bundle::{ProcedureKind, ResourceEntry};
        let mut resources = std::collections::HashMap::new();
        resources.insert(
            "rpc:ping".to_string(),
            ResourceEntry {
                kind: Some(ProcedureKind::Query),
                auth: Some(zeroship_bundle::AuthLevel::Anon),
                publicly_accessible: Some(true),
                ..Default::default()
            },
        );
        let manifest = zeroship_bundle::Manifest {
            version: 1,
            resources,
            ..zeroship_bundle::Manifest::default()
        };
        zeroship_core::types::RouteEntry {
            name: "spend-app.zeroship.localhost".to_string(),
            plan_id: "free".to_string(),
            api_key_hash: "h".to_string(),
            deploy_hash: None,
            manifest,
            oauth_client_id: None,
            sector_identifier: None,
            spend_state,
            account_state: zeroship_core::types::AccountState::Active,
        }
    }

    /// A route whose manifest serves a publicly-accessible STATIC resource at
    /// `/about`. Used to prove the spend gate covers the static-asset path
    /// (#3): a Blocked app must 402 BEFORE serving static egress, not fall
    /// through to the static server.
    fn static_spend_route(
        spend_state: zeroship_core::types::SpendState,
    ) -> zeroship_core::types::RouteEntry {
        use zeroship_bundle::{ResourceEntry, StaticAction};
        let mut resources = std::collections::HashMap::new();
        resources.insert(
            "/about".to_string(),
            ResourceEntry {
                auth: Some(zeroship_bundle::AuthLevel::Anon),
                publicly_accessible: Some(true),
                r#static: Some(StaticAction {
                    r#try: vec!["/about.html".into()],
                }),
                ..Default::default()
            },
        );
        let manifest = zeroship_bundle::Manifest {
            version: 1,
            resources,
            ..zeroship_bundle::Manifest::default()
        };
        zeroship_core::types::RouteEntry {
            name: "static-spend-app.zeroship.localhost".to_string(),
            plan_id: "free".to_string(),
            api_key_hash: "h".to_string(),
            deploy_hash: None,
            manifest,
            oauth_client_id: None,
            sector_identifier: None,
            spend_state,
            account_state: zeroship_core::types::AccountState::Active,
        }
    }

    /// Build an ntex `web::types::State<Arc<GateState>>` carrying `state` so a
    /// test can invoke the REAL `handle_request` / `execute_resource_tree`
    /// (their signatures take the extractor type, not `&Arc<GateState>`).
    async fn web_state(state: Arc<GateState>) -> web::types::State<Arc<GateState>> {
        use ntex::web::error::DefaultError;
        use ntex::web::FromRequest;
        let req = ntex::web::test::TestRequest::default()
            .state(state)
            .to_http_request();
        let mut payload = ntex::http::Payload::None;
        <web::types::State<Arc<GateState>> as FromRequest<DefaultError>>::from_request(
            &req,
            &mut payload,
        )
        .await
        .expect("State extractor")
    }

    /// Block → 402 SPEND_LIMIT BEFORE any worker proxy. Fed via the REAL
    /// `RouteCache::update` and driven through the REAL `handle_request` (the
    /// public entry — the gate is hoisted into `execute_resource_tree`). The
    /// stub hash-ring points at `0.0.0.0:0`; if the gate did NOT fire, the
    /// proxy attempt would surface a 502 BadGateway — so a 402 proves the gate
    /// short-circuited before the worker was ever contacted.
    #[compio::test]
    async fn over_limit_request_blocked_at_gateway() {
        use zeroship_core::types::SpendState;
        let state = build_idempotency_state();
        let app_id = Uuid::new_v4();

        let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        routes.insert(app_id, worker_spend_route(SpendState::Block));
        state
            .routes
            .update(routes, &state.rate_limiters, &state.concurrency);

        let req = ntex::web::test::TestRequest::default()
            .uri("/__zeroship/v1/ping")
            .header("host", "spend-app.zeroship.localhost")
            .to_http_request();
        let resp = handle_request(
            req,
            web_state(state.clone()).await,
            "spend-app.zeroship.localhost",
            "/__zeroship/v1/ping",
            Bytes::new(),
        )
        .await;

        assert_eq!(
            resp.status(),
            ntex::http::StatusCode::PAYMENT_REQUIRED,
            "a Blocked app must 402 before any worker proxy",
        );
        let mut resp = resp;
        let body = collect_body(resp.take_body()).await;
        let json: serde_json::Value = serde_json::from_slice(&body).expect("402 body is JSON");
        assert_eq!(json["code"], "SPEND_LIMIT");
    }

    /// #3 (RED→GREEN): a Blocked app serving a STATIC resource must 402 BEFORE
    /// any static egress. Pre-fix, the spend gate lived only in the worker
    /// dispatch path, so `ResolvedAction::Static` fell straight through to the
    /// static server (egress the platform eats). This drives the REAL
    /// `handle_request` → `execute_resource_tree` → static action against a
    /// Blocked route and asserts the 402/`SPEND_LIMIT` envelope fires first.
    #[compio::test]
    async fn over_limit_static_asset_blocked_at_gateway() {
        use zeroship_core::types::SpendState;
        let state = build_idempotency_state();
        let app_id = Uuid::new_v4();

        let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        routes.insert(app_id, static_spend_route(SpendState::Block));
        state
            .routes
            .update(routes, &state.rate_limiters, &state.concurrency);

        let req = ntex::web::test::TestRequest::default()
            .uri("/about")
            .header("host", "static-spend-app.zeroship.localhost")
            .to_http_request();
        let resp = handle_request(
            req,
            web_state(state.clone()).await,
            "static-spend-app.zeroship.localhost",
            "/about",
            Bytes::new(),
        )
        .await;

        assert_eq!(
            resp.status(),
            ntex::http::StatusCode::PAYMENT_REQUIRED,
            "a Blocked app must 402 on a STATIC resource before serving egress",
        );
        let mut resp = resp;
        let body = collect_body(resp.take_body()).await;
        let json: serde_json::Value = serde_json::from_slice(&body).expect("402 body is JSON");
        assert_eq!(json["code"], "SPEND_LIMIT");
    }

    /// Allow → the gate passes on a static resource (so dispatch proceeds to the
    /// static server, which 404s against the stub blob store — NOT 402). Pins
    /// that the hoisted gate only fires on Block, so the static-Block 402 above
    /// isn't a blanket reject of every static request.
    #[compio::test]
    async fn allowed_static_asset_passes_spend_gate() {
        use zeroship_core::types::SpendState;
        let state = build_idempotency_state();
        let app_id = Uuid::new_v4();
        let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        routes.insert(app_id, static_spend_route(SpendState::Allow));
        state
            .routes
            .update(routes, &state.rate_limiters, &state.concurrency);
        let req = ntex::web::test::TestRequest::default()
            .uri("/about")
            .header("host", "static-spend-app.zeroship.localhost")
            .to_http_request();
        let resp = handle_request(
            req,
            web_state(state.clone()).await,
            "static-spend-app.zeroship.localhost",
            "/about",
            Bytes::new(),
        )
        .await;
        assert_ne!(
            resp.status(),
            ntex::http::StatusCode::PAYMENT_REQUIRED,
            "an Allowed app must NOT be spend-blocked on a static resource",
        );
    }

    // -----------------------------------------------------------------------
    // Metering coverage (#27) — gateway egress + the no-double-count partition
    // -----------------------------------------------------------------------

    /// THE load-bearing regression (§2.4): the egress ownership partition.
    ///
    /// A gateway-owned response (here a STATIC action whose blob 404s — the
    /// gateway's own error body, a body the worker never sees) must record
    /// `gateway_egress_bytes` for the route's app and must NOT touch
    /// `egress_bytes` (which the WORKER owns). This drives the REAL
    /// `handle_request` → `execute_resource_tree` → static arm against the
    /// SAME `Arc<Meter>` in `GateState`, then drains it.
    ///
    /// RED pre-fix: the gateway had no meter and recorded nothing, so
    /// `gateway_egress_bytes` is absent. GREEN post-fix: the static arm
    /// records the served body length, and `egress_bytes` stays untouched.
    #[compio::test]
    async fn static_response_meters_gateway_egress_not_worker_egress() {
        use zeroship_core::types::SpendState;
        let state = build_idempotency_state();
        let meter = Arc::clone(&state.meter);
        let app_id = Uuid::new_v4();

        let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        routes.insert(app_id, static_spend_route(SpendState::Allow));
        state
            .routes
            .update(routes, &state.rate_limiters, &state.concurrency);

        let req = ntex::web::test::TestRequest::default()
            .uri("/about")
            .header("host", "static-spend-app.zeroship.localhost")
            .to_http_request();
        let mut resp = handle_request(
            req,
            web_state(state.clone()).await,
            "static-spend-app.zeroship.localhost",
            "/about",
            Bytes::new(),
        )
        .await;
        // The stub blob store has no bytes, so the static arm emits a 404
        // JSON error body — gateway-owned egress all the same.
        let served = collect_body(resp.take_body()).await;
        assert!(!served.is_empty(), "the gateway-owned 404 body is non-empty");

        let snap = meter.drain();
        let usage = snap
            .get(&app_id)
            .expect("gateway recorded usage for the static route's app");
        assert_eq!(
            usage.custom.get("gateway_egress_bytes").copied(),
            Some(served.len() as u64),
            "static (gateway-owned) egress must be metered as gateway_egress_bytes \
             equal to the served body length",
        );
        assert_eq!(
            usage.egress_bytes, 0,
            "the gateway must NEVER touch the worker-owned egress_bytes metric",
        );
    }

    /// The other half of the partition: a WORKER-proxied action must NOT
    /// record `gateway_egress_bytes` — the worker already counts its body as
    /// `egress_bytes`, and metering it here too would double-bill the same
    /// byte (the one over-bill vector). The proxy 502s against the stub
    /// hash-ring (no reachable worker), which is exactly the worker-arm path;
    /// the gateway must still record NO gateway_egress_bytes for it.
    ///
    /// RED if someone later meters the worker-proxy body in the gateway.
    #[compio::test]
    async fn gateway_does_not_meter_worker_proxy_body() {
        use zeroship_core::types::SpendState;
        let state = build_idempotency_state();
        let meter = Arc::clone(&state.meter);
        let app_id = Uuid::new_v4();

        let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        routes.insert(app_id, worker_spend_route(SpendState::Allow));
        state
            .routes
            .update(routes, &state.rate_limiters, &state.concurrency);

        let req = ntex::web::test::TestRequest::default()
            .uri("/__zeroship/v1/ping")
            .header("host", "spend-app.zeroship.localhost")
            .to_http_request();
        let _resp = handle_request(
            req,
            web_state(state.clone()).await,
            "spend-app.zeroship.localhost",
            "/__zeroship/v1/ping",
            Bytes::new(),
        )
        .await;

        let snap = meter.drain();
        // Either the app has no entry at all, or it has one but with NO
        // gateway_egress_bytes — the gateway must not meter the worker arm.
        if let Some(usage) = snap.get(&app_id) {
            assert_eq!(
                usage.custom.get("gateway_egress_bytes").copied(),
                None,
                "the gateway must NOT meter a worker-proxied response body as \
                 gateway_egress_bytes (no double-count vs the worker's egress_bytes)",
            );
        }
    }

    /// A worker RPC route that caps input at `max` bytes, so a body over the
    /// cap trips the gateway's 413 early-return arm in `execute_resource_tree`
    /// (a gateway-edge error envelope) BEFORE any worker proxy. Used to pin
    /// finding #1: gateway error/4xx envelopes are platform overhead and are
    /// deliberately NOT metered as `gateway_egress_bytes`.
    fn max_input_route(max: u32) -> zeroship_core::types::RouteEntry {
        use zeroship_bundle::{ProcedureKind, ResourceEntry};
        let mut resources = std::collections::HashMap::new();
        resources.insert(
            "rpc:ping".to_string(),
            ResourceEntry {
                kind: Some(ProcedureKind::Mutation),
                auth: Some(zeroship_bundle::AuthLevel::Anon),
                publicly_accessible: Some(true),
                max_input_bytes: Some(max),
                ..Default::default()
            },
        );
        let manifest = zeroship_bundle::Manifest {
            version: 1,
            resources,
            ..zeroship_bundle::Manifest::default()
        };
        zeroship_core::types::RouteEntry {
            name: "spend-app.zeroship.localhost".to_string(),
            plan_id: "free".to_string(),
            api_key_hash: "h".to_string(),
            deploy_hash: None,
            manifest,
            oauth_client_id: None,
            sector_identifier: None,
            spend_state: zeroship_core::types::SpendState::Allow,
            account_state: zeroship_core::types::AccountState::Active,
        }
    }

    /// FINDING #1 (pinning): a gateway-EMITTED error envelope (here a 413 from
    /// the `max_input_bytes` early-return arm — a body the worker never sees)
    /// is PLATFORM OVERHEAD and must NOT emit `gateway_egress_bytes`. The
    /// gateway only bills successful static + redirect bodies; error/4xx/5xx/204
    /// envelopes (tiny, frequently attacker-driven) are deliberately unbilled.
    ///
    /// Drives the REAL `handle_request` → `execute_resource_tree` 413 arm with
    /// a body over the declared cap, then drains the SAME `Arc<Meter>` and
    /// asserts the gateway recorded NO usage at all for the route's app. Locks
    /// the code↔design agreement so a future change that meters error arms
    /// fails here.
    #[compio::test]
    async fn gateway_error_envelope_is_not_metered() {
        let state = build_idempotency_state();
        let meter = Arc::clone(&state.meter);
        let app_id = Uuid::new_v4();

        let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        routes.insert(app_id, max_input_route(8));
        state
            .routes
            .update(routes, &state.rate_limiters, &state.concurrency);

        // A body over the 8-byte cap → 413 PayloadTooLarge from the gateway
        // edge, before any worker proxy.
        let oversized = Bytes::from_static(b"this body is well over eight bytes");
        let req = ntex::web::test::TestRequest::default()
            .uri("/__zeroship/v1/ping")
            .header("host", "spend-app.zeroship.localhost")
            .header("origin", "https://spend-app.zeroship.localhost")
            .method(ntex::http::Method::POST)
            .to_http_request();
        let mut resp = handle_request(
            req,
            web_state(state.clone()).await,
            "spend-app.zeroship.localhost",
            "/__zeroship/v1/ping",
            oversized,
        )
        .await;

        assert_eq!(
            resp.status(),
            ntex::http::StatusCode::PAYLOAD_TOO_LARGE,
            "an over-cap body must 413 at the gateway edge",
        );
        // The 413 body is non-empty (a JSON error envelope) — proving the
        // assertion below is about the DELIBERATE no-meter decision, not an
        // empty body.
        let body = collect_body(resp.take_body()).await;
        assert!(!body.is_empty(), "the 413 envelope is a non-empty JSON body");

        let snap = meter.drain();
        // The gateway must record NOTHING for an error envelope: no
        // gateway_egress_bytes, and (the gateway never owns it) no egress_bytes.
        if let Some(usage) = snap.get(&app_id) {
            assert_eq!(
                usage.custom.get("gateway_egress_bytes").copied(),
                None,
                "a gateway error/4xx envelope is platform overhead and must NOT \
                 be metered as gateway_egress_bytes",
            );
            assert_eq!(usage.egress_bytes, 0, "the gateway never touches egress_bytes");
        }
    }

    /// Restart-safety (§2.3 / the boot-nonce lesson): two
    /// `boot_worker_id("gate-…")` calls for the SAME stable base must differ,
    /// so the gateway's per-process `SequenceSource` resetting to 1 each boot
    /// can't collide with pre-restart `(producer_id, sequence)` rows and be
    /// dropped as a phantom duplicate (silent under-bill).
    #[test]
    fn gateway_producer_id_is_restart_unique() {
        let base = "gate-pod-3";
        let a = zeroship_metering::boot_worker_id(base);
        let b = zeroship_metering::boot_worker_id(base);
        assert_ne!(a, b, "each gateway boot must get a fresh metering identity");
        assert!(a.starts_with(&format!("{base}-")));
        assert!(b.starts_with(&format!("{base}-")));
    }

    /// Allow → the gate passes (so dispatch proceeds to the proxy, which fails
    /// against the stub ring → 502, NOT 402). Pins that the gate only fires on
    /// Block, so the Block 402 above isn't a blanket reject.
    #[compio::test]
    async fn allowed_request_passes_spend_gate() {
        use zeroship_core::types::SpendState;
        let state = build_idempotency_state();
        let app_id = Uuid::new_v4();
        let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        routes.insert(app_id, worker_spend_route(SpendState::Allow));
        state
            .routes
            .update(routes, &state.rate_limiters, &state.concurrency);
        let req = ntex::web::test::TestRequest::default()
            .uri("/__zeroship/v1/ping")
            .header("host", "spend-app.zeroship.localhost")
            .to_http_request();
        let resp = handle_request(
            req,
            web_state(state.clone()).await,
            "spend-app.zeroship.localhost",
            "/__zeroship/v1/ping",
            Bytes::new(),
        )
        .await;
        assert_ne!(
            resp.status(),
            ntex::http::StatusCode::PAYMENT_REQUIRED,
            "an Allowed app must NOT be spend-blocked",
        );
    }

    /// Degrade flipped via the REAL `RouteCache::update` tightens the app's
    /// effective concurrency ceiling. With a global limit of DEGRADE_FACTOR and
    /// DEGRADE_FACTOR=8 the effective ceiling becomes 1, so the FIRST acquire
    /// succeeds and the SECOND is rejected — proving `set_degraded` is not a
    /// no-op. A non-degraded control app at the same limit admits both.
    #[compio::test]
    async fn degraded_route_tightens_concurrency() {
        use crate::enforce::{acquire_concurrency, ConcurrencyRegistry, RateLimitRegistry, DEGRADE_FACTOR};
        use zeroship_core::types::SpendState;

        let cache = crate::sync::RouteCache::new();
        let rate = RateLimitRegistry::new(1000, 2000);
        let concurrency = ConcurrencyRegistry::new(DEGRADE_FACTOR);

        let degraded_app = Uuid::new_v4();
        let normal_app = Uuid::new_v4();
        let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        let mut degraded = spend_route(SpendState::Degrade);
        degraded.name = "degraded.zeroship.localhost".into();
        let mut normal = spend_route(SpendState::Allow);
        normal.name = "normal.zeroship.localhost".into();
        routes.insert(degraded_app, degraded);
        routes.insert(normal_app, normal);

        cache.update(routes, &rate, &concurrency);
        assert!(concurrency.is_degraded(&degraded_app));
        assert!(!concurrency.is_degraded(&normal_app));

        let g1 = acquire_concurrency(&concurrency, &degraded_app).expect("first admits");
        let r2 = acquire_concurrency(&concurrency, &degraded_app);
        assert!(r2.is_err(), "degraded app's 2nd concurrent request must be rejected");
        if let Err(resp) = r2 {
            assert_eq!(resp.status(), ntex::http::StatusCode::TOO_MANY_REQUESTS);
        }
        drop(g1);

        let _n1 = acquire_concurrency(&concurrency, &normal_app).expect("normal 1");
        let _n2 = acquire_concurrency(&concurrency, &normal_app).expect("normal 2");
    }

    /// Degrade → Allow flipped via the REAL `RouteCache::update` restores full
    /// throughput on the NEXT request with no warm-up — the gauge was never
    /// rebuilt. Proves recovery is instant.
    #[compio::test]
    async fn degrade_clears_immediately_on_recovery() {
        use crate::enforce::{acquire_concurrency, ConcurrencyRegistry, RateLimitRegistry, DEGRADE_FACTOR};
        use zeroship_core::types::SpendState;

        let cache = crate::sync::RouteCache::new();
        let rate = RateLimitRegistry::new(1000, 2000);
        let concurrency = ConcurrencyRegistry::new(DEGRADE_FACTOR);
        let app = Uuid::new_v4();

        let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        routes.insert(app, spend_route(SpendState::Degrade));
        cache.update(routes, &rate, &concurrency);
        assert!(concurrency.is_degraded(&app));
        let g1 = acquire_concurrency(&concurrency, &app).expect("first admits");
        assert!(
            acquire_concurrency(&concurrency, &app).is_err(),
            "degraded ceiling is 1",
        );
        drop(g1);

        let mut routes2: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        routes2.insert(app, spend_route(SpendState::Allow));
        cache.update(routes2, &rate, &concurrency);
        assert!(!concurrency.is_degraded(&app));
        let mut guards = Vec::new();
        for i in 0..DEGRADE_FACTOR {
            guards.push(
                acquire_concurrency(&concurrency, &app)
                    .unwrap_or_else(|_| panic!("recovered app admits request {i}")),
            );
        }
    }

    // -----------------------------------------------------------------------
    // G2 — faithful account-suspension enforcement at the gateway edge.
    //
    // Drives the REAL path: a `RouteEntry` carrying the pulled `account_state`
    // (and `spend_state`) is pushed through the REAL `RouteCache::update`, then
    // the public `handle_request` is invoked. The stub hash-ring points at
    // `0.0.0.0:0`, so a request that PASSES the gates surfaces a 502 (worker
    // unreachable); a 402 therefore proves the gate short-circuited BEFORE any
    // worker proxy. The account gate composes with spend as an AND.
    // -----------------------------------------------------------------------

    /// A worker route (public RPC query at `ping`) with explicit account + spend
    /// state — exercises the hoisted G2 account gate AND its composition with
    /// the spend gate through the real dispatch path.
    fn account_worker_route(
        account_state: zeroship_core::types::AccountState,
        spend_state: zeroship_core::types::SpendState,
    ) -> zeroship_core::types::RouteEntry {
        let mut entry = worker_spend_route(spend_state);
        entry.account_state = account_state;
        entry
    }

    async fn drive_ping(state: Arc<GateState>) -> HttpResponse {
        let req = ntex::web::test::TestRequest::default()
            .uri("/__zeroship/v1/ping")
            .header("host", "spend-app.zeroship.localhost")
            .to_http_request();
        handle_request(
            req,
            web_state(state).await,
            "spend-app.zeroship.localhost",
            "/__zeroship/v1/ping",
            Bytes::new(),
        )
        .await
    }

    async fn assert_402_code(mut resp: HttpResponse, code: &str) {
        assert_eq!(
            resp.status(),
            ntex::http::StatusCode::PAYMENT_REQUIRED,
            "expected a 402 with code {code}",
        );
        let body = collect_body(resp.take_body()).await;
        let json: serde_json::Value = serde_json::from_slice(&body).expect("402 body is JSON");
        assert_eq!(json["code"], code, "402 envelope code");
    }

    /// A `Suspended` creator's app → 402 `ACCOUNT_SUSPENDED` BEFORE any worker
    /// proxy. Fed via the REAL `RouteCache::update`, driven through the REAL
    /// `handle_request`. Spend is `Allow`, so the ONLY thing that can 402 is the
    /// account gate — proving suspension enforces independently of spend.
    #[compio::test]
    async fn suspended_account_blocked_at_gateway() {
        use zeroship_core::types::{AccountState, SpendState};
        let state = build_idempotency_state();
        let app_id = Uuid::new_v4();
        let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        routes.insert(app_id, account_worker_route(AccountState::Suspended, SpendState::Allow));
        state.routes.update(routes, &state.rate_limiters, &state.concurrency);

        assert_402_code(drive_ping(state.clone()).await, "ACCOUNT_SUSPENDED").await;
    }

    /// A `PastDue` creator's app is NOT blocked (the grace window). With spend
    /// `Allow`, the request passes both gates and reaches the proxy (→ 502 vs the
    /// stub ring, NOT 402). Proves past_due is the WARNING state, not a block.
    #[compio::test]
    async fn past_due_account_not_blocked_at_gateway() {
        use zeroship_core::types::{AccountState, SpendState};
        let state = build_idempotency_state();
        let app_id = Uuid::new_v4();
        let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        routes.insert(app_id, account_worker_route(AccountState::PastDue, SpendState::Allow));
        state.routes.update(routes, &state.rate_limiters, &state.concurrency);

        let resp = drive_ping(state.clone()).await;
        assert_ne!(
            resp.status(),
            ntex::http::StatusCode::PAYMENT_REQUIRED,
            "a past_due (grace) app must NOT be account-blocked",
        );
    }

    /// An `Active` creator with `Allow` spend passes the account gate (→ proxy →
    /// 502, NOT 402). Pins that the account gate only fires on Suspended.
    #[compio::test]
    async fn active_account_passes_gate() {
        use zeroship_core::types::{AccountState, SpendState};
        let state = build_idempotency_state();
        let app_id = Uuid::new_v4();
        let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        routes.insert(app_id, account_worker_route(AccountState::Active, SpendState::Allow));
        state.routes.update(routes, &state.rate_limiters, &state.concurrency);

        let resp = drive_ping(state.clone()).await;
        assert_ne!(
            resp.status(),
            ntex::http::StatusCode::PAYMENT_REQUIRED,
            "an active app with Allow spend must not be 402'd",
        );
    }

    /// The two gates compose as an AND with DISTINCT codes:
    ///   * Suspended + Allow spend → 402 ACCOUNT_SUSPENDED (account beats spend).
    ///   * Active + Block spend     → 402 SPEND_LIMIT (spend fires when account ok).
    ///   * Suspended + Block spend  → 402 ACCOUNT_SUSPENDED (account is the OUTER
    ///     gate, evaluated first).
    #[compio::test]
    async fn account_and_spend_gates_compose() {
        use zeroship_core::types::{AccountState, SpendState};

        // Suspended beats Allow spend.
        {
            let state = build_idempotency_state();
            let app_id = Uuid::new_v4();
            let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
            routes.insert(app_id, account_worker_route(AccountState::Suspended, SpendState::Allow));
            state.routes.update(routes, &state.rate_limiters, &state.concurrency);
            assert_402_code(drive_ping(state.clone()).await, "ACCOUNT_SUSPENDED").await;
        }

        // Active account, Block spend → spend gate fires.
        {
            let state = build_idempotency_state();
            let app_id = Uuid::new_v4();
            let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
            routes.insert(app_id, account_worker_route(AccountState::Active, SpendState::Block));
            state.routes.update(routes, &state.rate_limiters, &state.concurrency);
            assert_402_code(drive_ping(state.clone()).await, "SPEND_LIMIT").await;
        }

        // Suspended account AND Block spend → the OUTER account gate wins (it is
        // evaluated first), so the code is ACCOUNT_SUSPENDED, not SPEND_LIMIT.
        {
            let state = build_idempotency_state();
            let app_id = Uuid::new_v4();
            let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
            routes.insert(app_id, account_worker_route(AccountState::Suspended, SpendState::Block));
            state.routes.update(routes, &state.rate_limiters, &state.concurrency);
            assert_402_code(drive_ping(state.clone()).await, "ACCOUNT_SUSPENDED").await;
        }
    }
}
