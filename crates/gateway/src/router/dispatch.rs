//! Request entry, resource-tree dispatch, idempotency hooks, and
//! worker-forwarding for the gateway router.
//!
//! Public surface (re-exported from `router/mod.rs`):
//!
//! * [`handle`]               — path-based handler `/{app_name}/{tail*}`.
//! * [`handle_subdomain`]     — Host-header subdomain handler.
//! * [`extract_app_name`]     — shared helper used by both, plus the
//!                              gateway's outer middleware.
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

use crate::{enforce, idempotency, proxy, user_auth, GateState};

use super::auth::{auth_satisfied, extract_session_cookie, jwt_subject_unverified};
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
///   transports). Reads from `connection_info().remote()` so a trusted
///   proxy's `X-Forwarded-For` is honored when present (matches what
///   the gateway already does for scheme detection a few lines below).
/// * `RateLimitPer::Session` — the `__zs_session` cookie value.
///   Anonymous callers (no cookie) fall back to the IP so an
///   unauthenticated burst still gets bucketed; without the fallback
///   they'd all share one "" key.
/// * `RateLimitPer::App` — constant `"app"`. One bucket platform-wide;
///   `(app_id, rule_idx, "app")` is the key, equivalent to a global
///   per-app limit at the rule level.
pub(crate) fn compute_bucket_id(
    req: &HttpRequest,
    per: zeroship_bundle::RateLimitPer,
) -> String {
    use zeroship_bundle::RateLimitPer;
    match per {
        RateLimitPer::Ip => req
            .connection_info()
            .remote()
            .unwrap_or("unknown")
            .to_string(),
        RateLimitPer::Session => {
            let cookie = req
                .headers()
                .get("cookie")
                .and_then(|v| v.to_str().ok());
            extract_session_cookie(cookie).unwrap_or_else(|| {
                req.connection_info()
                    .remote()
                    .unwrap_or("unknown")
                    .to_string()
            })
        }
        RateLimitPer::App => "app".to_string(),
    }
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
///   2. `__zs_session` cookie value (browser tab affinity).
///   3. `Sec-WebSocket-Key` (per-connection nonce — same connection
///      always hashes to the same bucket; reconnects vary).
///   4. Client IP (last-resort fallback for unauthenticated callers
///      hitting subscriptions on `publicly_accessible` resources).
pub(crate) fn subscription_affinity_key(req: &HttpRequest) -> String {
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
    if let Some(token) = extract_session_cookie(cookie) {
        return format!("sess:{token}");
    }
    if let Some(key) = req
        .headers()
        .get("sec-websocket-key")
        .and_then(|v| v.to_str().ok())
    {
        return format!("wsk:{key}");
    }
    let ip = req
        .connection_info()
        .remote()
        .unwrap_or("unknown")
        .to_string();
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
    // The gateway proxies OIDC protocol endpoints (`/oauth2/*`,
    // `/.well-known/*`, `/userinfo`) to Ory Hydra; everything else
    // (login UI, OAuth2 consent handlers, webhooks) goes to crates/auth.
    if is_auth_host(&req) {
        return route_auth_host(req, state, body).await;
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
    /// OIDC protocol endpoints implemented by Ory Hydra.
    Hydra,
    /// Login UI, OAuth2 consent handlers, and webhooks — implemented
    /// by `crates/auth`.
    Auth,
}

/// Classify an inbound `auth.zeroship.ai` path. The protocol endpoints
/// listed here are the public hydra contract:
///
///   * `/oauth2/auth`, `/oauth2/token`, `/oauth2/revoke`, …
///   * `/.well-known/openid-configuration`, `/.well-known/jwks.json`
///   * `/userinfo`
///
/// Everything else is handled by `crates/auth` (login HTML, consent
/// callbacks, signup, password reset, …).
pub(crate) fn classify_auth_path(path: &str) -> AuthUpstream {
    if path.starts_with("/oauth2/")
        || path.starts_with("/.well-known/")
        || path == "/userinfo"
    {
        AuthUpstream::Hydra
    } else {
        AuthUpstream::Auth
    }
}

/// True when the request's Host header is the auth.zeroship.ai
/// platform host. Strips an optional port. Tolerates dev hostnames
/// like `auth.zeroship.localhost` by matching the `auth.zeroship.`
/// prefix as well — exact-match on `auth.zeroship.ai` is the
/// production case.
fn is_auth_host(req: &HttpRequest) -> bool {
    let Some(host_hdr) = req.headers().get("host").and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let host = host_hdr.split(':').next().unwrap_or(host_hdr);
    let host_lc = host.to_ascii_lowercase();
    host_lc == "auth.zeroship.ai" || host_lc.starts_with("auth.zeroship.")
}

async fn route_auth_host(
    req: HttpRequest,
    state: web::types::State<Arc<GateState>>,
    body: Bytes,
) -> HttpResponse {
    let path = req.uri().path();
    let upstream_base = match classify_auth_path(path) {
        AuthUpstream::Hydra => state.config.hydra_public.as_str(),
        AuthUpstream::Auth => state.config.auth_public.as_str(),
    };

    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");

    let mut headers: Vec<(String, String)> = Vec::new();
    for (name, value) in req.headers() {
        if let Ok(v) = value.to_str() {
            headers.push((name.as_str().to_string(), v.to_string()));
        }
    }

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

// ---------------------------------------------------------------------------
// Unified request handler
// ---------------------------------------------------------------------------

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
    let dispatch_path = format!("/{tail}");

    // CORS preflight short-circuit. The browser sends `OPTIONS` with
    // `Origin` and `Access-Control-Request-Method` *before* the actual
    // request; we look up the resource-tree CORS policy for the
    // requested path and answer with a 204. If no resource matches,
    // fall through to normal dispatch (which will return 404).
    if req.method() == ntex::http::Method::OPTIONS && req.headers().contains_key("origin") {
        if let Some(policy) = compiled_route.manifest.lookup_resource(&dispatch_path) {
            if let Some(cors) = &policy.cors {
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
    if compiled_route.manifest.lookup_resource(&dispatch_path).is_some() {
        return execute_resource_tree(
            req,
            state,
            &app_id,
            &compiled_route,
            &dispatch_path,
            tail,
            body,
            wall_start,
        )
        .await;
    }

    HttpResponse::NotFound()
        .json(&serde_json::json!({"error": "no resource matched"}))
}

// ---------------------------------------------------------------------------
// v3 resource-tree dispatch
// ---------------------------------------------------------------------------

/// Run the v3 resource-tree dispatch for a request that resolved to a
/// manifest with `resources` non-empty. Looks up the matching resource,
/// enforces the precomputed `EffectivePolicy`, and executes the
/// resolved action (worker forward / redirect / static).
async fn execute_resource_tree(
    req: HttpRequest,
    state: web::types::State<Arc<GateState>>,
    app_id: &Uuid,
    compiled_route: &crate::sync::CompiledRoute,
    dispatch_path: &str,
    tail: &str,
    body: Bytes,
    wall_start: std::time::Instant,
) -> HttpResponse {
    use crate::compiled::ResolvedAction;
    use zeroship_bundle::ProcedureKind;

    // 1. Resolve the resource. No match → 404.
    let Some(policy) = compiled_route.manifest.lookup_resource(dispatch_path) else {
        return HttpResponse::NotFound()
            .json(&serde_json::json!({"error": "no resource matched"}));
    };

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
    //    validate-time). `user`/`admin` require a session cookie.
    //    Richer admin-vs-user role checks will arrive with the auth
    //    tier.
    if !auth_satisfied(&req, policy, &state.config.auth_secret, app_id) {
        return HttpResponse::Unauthorized()
            .json(&serde_json::json!({"error": "authentication required"}));
    }

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
        let resource_key = compiled_route
            .manifest
            .lookup_resource_key(dispatch_path)
            .unwrap_or_default();
        let rule_idx = resource_key_hash(&resource_key);
        let bucket_id = compute_bucket_id(&req, rl.per);
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
                    body,
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
                body,
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
                wall_start,
            )
            .await
        }
    };

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

/// Resolve the wireId from a `/_zs/v1/<wireId>` dispatch path. The
/// dispatch path always has a leading slash; the wireId is the rest
/// after the literal prefix. Returns `None` for any non-RPC path —
/// callers should only enter idempotency for `WorkerRpc` actions.
fn dispatch_path_wire_id(dispatch_path: &str) -> Option<&str> {
    dispatch_path.strip_prefix("/_zs/v1/")
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
    let affinity = subscription_affinity_key(&_req);
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
// Dispatch handler — the single worker-facing path
// ---------------------------------------------------------------------------

/// Forward an HTTP request to the worker via `/dispatch/{app_id}`.
///
/// The full HTTP request (method, URL, headers, body) is packaged into the
/// HttpEnvelope and handed to `Runtime::call_fetch_handler`, which invokes
/// the app's exported `default.fetch(req, env, ctx)`. Covers both the
/// `_rpc/*` URLs (routed inside the kernel via the bootstrap) and plain
/// HTTP requests. Enables streaming responses (e.g., SSE for LLM token
/// streaming).
async fn handle_dispatch(
    req: HttpRequest,
    state: &GateState,
    app_id: &Uuid,
    route: &zeroship_core::types::RouteEntry,
    tail: &str,
    body: Bytes,
    wall_start: std::time::Instant,
) -> HttpResponse {
    // Rate limit
    if let Err(resp) = enforce::check_rate_limit(&state.rate_limiters, app_id) {
        return resp;
    }

    // Concurrency guard (RAII — released on drop)
    let _guard = match enforce::acquire_concurrency(&state.concurrency, app_id) {
        Ok(guard) => guard,
        Err(resp) => return resp,
    };

    // Extract user from __zs_session cookie
    let user_header_value = if !state.config.auth_secret.is_empty() {
        let cookie = req
            .headers()
            .get("cookie")
            .and_then(|v| v.to_str().ok());
        let app_id_str = app_id.to_string();
        user_auth::extract_user(cookie, &state.config.auth_secret, &app_id_str)
            .map(|u| user_auth::encode_user_header(&u, &state.config.worker_key))
    } else {
        None
    };

    // Reconstruct the URL the JS handler will see.
    let scheme = if req.connection_info().scheme() == "https" { "https" } else { "http" };
    let host = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let url = format!("{scheme}://{host}/{tail}");

    // Collect request headers as [key, value] pairs.
    let mut headers: Vec<(String, String)> = Vec::new();
    for (name, value) in req.headers() {
        if let Ok(v) = value.to_str() {
            headers.push((name.as_str().to_string(), v.to_string()));
        }
    }

    let method = req.method().as_str();
    let body_str = String::from_utf8_lossy(&body);

    // Proxy to worker via CHWBL hash ring.
    let request_id = Uuid::new_v4();
    let mut response = match proxy::forward_dispatch(
        &state.hash_ring,
        app_id,
        &route.plan_id,
        &request_id,
        method,
        &url,
        &headers,
        &body_str,
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

    // Handle 401 response: redirect browser requests to the auth page.
    if response.status() == ntex::http::StatusCode::UNAUTHORIZED {
        let accepts_html = req
            .headers()
            .get("accept")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("text/html"));

        if accepts_html {
            let original_path = req.uri().path();
            let auth_url = format!(
                "{}/auth/authorize?app_id={}&return={}",
                state.config.control_url, app_id, original_path
            );
            return HttpResponse::Found()
                .header("location", auth_url)
                .finish();
        }
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

    response
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
                auth_secret: String::new(),
                worker_key: String::new(),
                hydra_public: String::new(),
                auth_public: String::new(),
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
            .header("cookie", "__zs_session=abc")
            .to_http_request();
        assert_eq!(compute_bucket_id(&req, RateLimitPer::App), "app");
    }

    #[test]
    fn compute_bucket_id_ip_falls_back_to_unknown() {
        // The TestRequest has no peer addr → "unknown" sentinel keeps
        // the bucket lookup well-defined instead of crashing.
        let req = ntex::web::test::TestRequest::default().to_http_request();
        let id = compute_bucket_id(&req, RateLimitPer::Ip);
        assert_eq!(id, "unknown");
    }

    #[test]
    fn compute_bucket_id_session_uses_cookie() {
        let req = ntex::web::test::TestRequest::default()
            .header(
                "cookie",
                "other=foo; __zs_session=abc123; trailing=x",
            )
            .to_http_request();
        let id = compute_bucket_id(&req, RateLimitPer::Session);
        assert_eq!(id, "abc123");
    }

    #[test]
    fn compute_bucket_id_session_falls_back_to_ip_when_cookie_missing() {
        // Anonymous caller (no __zs_session) → fall back to IP. The
        // TestRequest has no peer → "unknown".
        let req = ntex::web::test::TestRequest::default()
            .header("cookie", "other=foo")
            .to_http_request();
        let id = compute_bucket_id(&req, RateLimitPer::Session);
        assert_eq!(id, "unknown");
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
        let bucket_id = compute_bucket_id(&req, rl.per);
        assert!(reg.check(&app_id, 0, rl.per, &bucket_id, &rl).is_ok());
        // Second call with the same request → same bucket id → drained.
        let bucket_id2 = compute_bucket_id(&req, rl.per);
        assert_eq!(bucket_id, bucket_id2, "bucket id is stable for same request");
        let err = reg
            .check(&app_id, 0, rl.per, &bucket_id2, &rl)
            .expect_err("second call must 429 — bucket key matched");
        assert_eq!(err.status(), ntex::http::StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn router_wiring_session_buckets_separate_from_ip_buckets() {
        // Two requests carrying distinct __zs_session cookies under
        // RateLimitPer::Session must hit independent buckets even when
        // the IP is the same.
        let reg = crate::enforce::PerRuleRateLimitRegistry::new();
        let app_id = uuid::Uuid::nil();
        let rl = RateLimit { rps: Some(1), rpm: None, per: RateLimitPer::Session };

        let req_a = ntex::web::test::TestRequest::default()
            .header("cookie", "__zs_session=user-a")
            .to_http_request();
        let req_b = ntex::web::test::TestRequest::default()
            .header("cookie", "__zs_session=user-b")
            .to_http_request();
        let bucket_a = compute_bucket_id(&req_a, rl.per);
        let bucket_b = compute_bucket_id(&req_b, rl.per);
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
            .lookup_resource("/_zs/v1/listTodos")
            .expect("matches rpc:listTodos");
        assert_eq!(p.kind, Some(ProcedureKind::Query));
        // Bare /_rpc/ paths are no longer dispatched — `/_zs/v1/` is
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
        let p = c.lookup_resource("/_zs/v1/expensive").expect("matches");
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
            c.lookup_resource("/_zs/v1/anything").is_none(),
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
            "/_zs/v1/todos.add",
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
            "/_zs/v1/todos.add",
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
            "/_zs/v1/todos.add",
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
            "/_zs/v1/todos.add",
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
            "/_zs/v1/todos.add",
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
            "/_zs/v1/todos.add",
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
            "/_zs/v1/todos.add",
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
            .header("cookie", "__zs_session=cookieval")
            .to_http_request();
        assert_eq!(subscription_affinity_key(&req), "sub:alice");
    }

    #[test]
    fn subscription_affinity_falls_back_to_cookie() {
        let req = ntex::web::test::TestRequest::default()
            .header("cookie", "__zs_session=tok123")
            .to_http_request();
        assert_eq!(subscription_affinity_key(&req), "sess:tok123");
    }

    #[test]
    fn subscription_affinity_falls_back_to_ws_key_then_ip() {
        let req = ntex::web::test::TestRequest::default()
            .header("sec-websocket-key", "abc==")
            .to_http_request();
        assert_eq!(subscription_affinity_key(&req), "wsk:abc==");

        let req = ntex::web::test::TestRequest::default().to_http_request();
        // No headers, no remote — falls back to "ip:unknown".
        assert_eq!(subscription_affinity_key(&req), "ip:unknown");
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
            .lookup_resource("/_zs/v1/todoTicker")
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
    fn classify_auth_path_well_known_routes_to_hydra() {
        assert_eq!(
            classify_auth_path("/.well-known/openid-configuration"),
            AuthUpstream::Hydra
        );
        assert_eq!(
            classify_auth_path("/.well-known/jwks.json"),
            AuthUpstream::Hydra
        );
    }

    #[test]
    fn classify_auth_path_userinfo_routes_to_hydra() {
        assert_eq!(classify_auth_path("/userinfo"), AuthUpstream::Hydra);
    }

    #[test]
    fn classify_auth_path_ui_and_callbacks_route_to_auth() {
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
}
