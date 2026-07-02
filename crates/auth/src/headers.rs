//! Security headers applied to every `crates/auth` response.
//!
//! [`apply`] writes the headers into a `HeaderMap`; [`SecurityHeaders`] is
//! the ntex middleware that wires it onto every outgoing response in
//! [`crate::server::run`].
//!
//! ## Route-aware framing (immersive iframe login, design §4.3/§6.3)
//!
//! Most routes are NEVER frameable: they emit `X-Frame-Options: DENY` +
//! CSP `frame-ancestors 'none'` (fail-closed). The immersive console login,
//! however, embeds `auth.zeroship.ai`'s real `/login` form inside a
//! cross-origin, same-site iframe on `console.zeroship.ai` (the Stripe model).
//! For that to work, the documents that render INSIDE the iframe —
//! `/login`, `/signup`, and the interactive `/consent` render — must carry
//! `frame-ancestors 'self' <console origin(s)>` and DROP `X-Frame-Options`
//! (a legacy UA honoring XFO would refuse the frame the CSP allows; CSP Level
//! 2 says a UA supporting `frame-ancestors` ignores XFO, so the two must not
//! disagree). Because the middleware sets XFO with an UNCONDITIONAL insert and
//! runs AFTER the handler, a handler cannot suppress XFO by leaving it absent —
//! the middleware itself must branch on the request path. That is what
//! [`apply`] does: given the request path + the configured
//! `frame_ancestor_origins`, it emits the relaxed framing headers on the framed
//! routes and the strict default everywhere else. The allowed-ancestor list is
//! deployment config (no console host is compiled in); an empty list keeps the
//! strict default (dev / single-origin), so the relax is a no-op by default.

use std::net::{IpAddr, SocketAddr};

use ntex::http::header::{HeaderName, HeaderValue};
use ntex::http::HeaderMap;
use ntex::service::{cfg::SharedCfg, Middleware, Service, ServiceCtx};
use ntex::web::{HttpRequest, WebRequest, WebResponse};
use uuid::Uuid;

const DEFAULT_CONTENT_SECURITY_POLICY: &str = "default-src 'self'; \
         script-src 'self'; \
         style-src 'self'; \
         img-src 'self' data: https://*.zeroship.ai \
                       https://lh3.googleusercontent.com \
                       https://avatars.githubusercontent.com; \
         connect-src 'self'; \
         form-action 'self'; \
         frame-ancestors 'none'; \
         base-uri 'none'; \
         object-src 'none'; \
         upgrade-insecure-requests";

/// The exact set of request paths whose responses render INSIDE the immersive
/// login iframe and therefore need the relaxed `frame-ancestors` (design §4.3).
/// `/login` + `/signup` cover both their GET render and their POST error
/// re-render (`render_login_error` / the signup error render — same path, same
/// middleware pass). `/consent` covers the interactive consent render; the
/// `skip_consent` silent-accept path returns a body-less 302 for which the
/// `frame-ancestors` is harmless. Federated bounces (`/oauth/google`, …) are
/// deliberately ABSENT — the federated IdP's own page is never framed (it stays
/// a popup), so those keep the strict `XFO: DENY` default.
const FRAMED_ROUTE_PATHS: &[&str] = &["/login", "/signup", "/consent"];

/// True when `req_path` is one of the framed login routes (exact match on the
/// path component — query strings are already stripped by the time ntex hands
/// us `req.path()`). Static subresources (`/static/style.css`) are NEVER framed
/// documents (they are pulled in via `<link>`/`<script>`), so they are not in
/// the set and keep the strict default — `frame-ancestors` has no effect on a
/// subresource anyway.
#[must_use]
fn is_framed_route(req_path: &str) -> bool {
    FRAMED_ROUTE_PATHS.contains(&req_path)
}

/// Integration-test accessor for [`is_framed_route`] (the predicate is private
/// so handlers can't reach it; the `threat_model` e2e asserts the framed-route
/// set against it — design §9(c) "/oauth/google is NOT in the framed set").
#[doc(hidden)]
#[must_use]
pub fn is_framed_route_for_test(req_path: &str) -> bool {
    is_framed_route(req_path)
}

/// A CSP `frame-ancestors` source MUST be a concrete `scheme://host[:port]` —
/// NO wildcard, NO CSP-list/header-injecting bytes (design §6.2). The deployment
/// config layer (`AuthConfig::resolve`) already drops non-concrete origins, so
/// in practice the live builder only sees clean origins; this is the
/// defense-in-depth guard so even a future code path that fed an unsanitized
/// origin here could not widen the allowlist or break the header value. A `*`
/// (e.g. `https://*.zeroship.ai`, the bare `*`) would re-admit every creator
/// app, so it is rejected outright.
#[must_use]
fn is_concrete_ancestor_source(origin: &str) -> bool {
    let o = origin.trim();
    if o.is_empty() {
        return false;
    }
    if o.strip_prefix("https://")
        .or_else(|| o.strip_prefix("http://"))
        .is_none_or(str::is_empty)
    {
        return false;
    }
    !o.chars().any(|c| {
        c == '*'
            || c == ';'
            || c == ','
            || c == ' '
            || c == '\t'
            || c.is_control()
            || !c.is_ascii()
    })
}

/// Build the CSP for a FRAMED login route: the baseline default-src/script-src/
/// … shape with `frame-ancestors 'none'` replaced by
/// `frame-ancestors 'self' <origins…>`. `'self'` keeps the auth origin's own
/// pages framing each other; each configured origin is one cross-site embedder
/// (the console) the browser will admit. Only CONCRETE origins
/// ([`is_concrete_ancestor_source`]) are spliced — a wildcard / poison entry is
/// dropped (§6.2 defense in depth). With an EMPTY (or all-dropped) `origins`
/// slice this degrades to `frame-ancestors 'self'` (still strictly tighter than
/// `'none'` only by admitting same-origin self-framing) — but the middleware
/// only calls this on a framed route, and a deployment that wants the iframe
/// configures the console origin. The origins are joined with single spaces, the
/// CSP source-list separator.
#[must_use]
fn framed_route_csp(origins: &[String]) -> String {
    let mut ancestors = String::from("'self'");
    for origin in origins {
        if is_concrete_ancestor_source(origin) {
            ancestors.push(' ');
            ancestors.push_str(origin.trim());
        }
    }
    format!(
        "default-src 'self'; \
         script-src 'self'; \
         style-src 'self'; \
         img-src 'self' data: https://*.zeroship.ai \
                       https://lh3.googleusercontent.com \
                       https://avatars.githubusercontent.com; \
         connect-src 'self'; \
         form-action 'self'; \
         frame-ancestors {ancestors}; \
         base-uri 'none'; \
         object-src 'none'; \
         upgrade-insecure-requests"
    )
}

/// Client IP for rate-limiting and audit, as a string.
///
/// The auth service runs behind the gateway, the SOLE trusted hop. The
/// gateway strips any client-supplied `X-Forwarded-For` / `Forwarded` /
/// `X-Real-IP` and re-authors a SINGLE authoritative `X-Forwarded-For` token =
/// the real socket peer (SEC-3, see the gateway's `build_auth_upstream_headers`).
/// We therefore read the RIGHTMOST (closest-hop, gateway-authored) XFF token —
/// NOT the leftmost, which `connection_info().remote()` returns and which a
/// caller can prepend to spoof a per-IP rate-limit bucket — and REQUIRE it to
/// parse as an IP before it is used as a bucket key (an unvalidated value would
/// let an attacker mint unbounded `zeroship.rate_limits` rows). When the
/// forwarded value is absent or unparseable we fall back to the raw socket
/// peer, then the `"0.0.0.0"` sentinel (e.g. unit tests with neither).
#[must_use]
pub(crate) fn client_ip(req: &HttpRequest) -> String {
    trusted_forwarded_ip(req.headers())
        .or_else(|| req.peer_addr().map(|addr| addr.ip()))
        .map_or_else(|| "0.0.0.0".to_string(), |ip| ip.to_string())
}

/// Extract the trusted client IP from `X-Forwarded-For`: the RIGHTMOST
/// non-empty token (the value the closest trusted hop — the gateway — authored)
/// that parses as an [`IpAddr`]. Returns `None` when the header is absent,
/// empty, or its trusted token is not a valid IP.
fn trusted_forwarded_ip(headers: &HeaderMap) -> Option<IpAddr> {
    let value = headers.get("x-forwarded-for")?.to_str().ok()?;
    let token = value.rsplit(',').map(str::trim).find(|t| !t.is_empty())?;
    // A bare IP, or an `ip:port` SocketAddr (some proxies append the port).
    token
        .parse::<IpAddr>()
        .ok()
        .or_else(|| token.parse::<SocketAddr>().ok().map(|addr| addr.ip()))
}

#[must_use]
pub fn content_security_policy_with_script_nonce(nonce: &str) -> String {
    format!(
        "default-src 'self'; \
         script-src 'self' 'nonce-{nonce}'; \
         style-src 'self'; \
         img-src 'self' data: https://*.zeroship.ai \
                       https://lh3.googleusercontent.com \
                       https://avatars.githubusercontent.com; \
         connect-src 'self'; \
         form-action 'self'; \
         frame-ancestors 'none'; \
         base-uri 'none'; \
         object-src 'none'; \
         upgrade-insecure-requests"
    )
}

/// Apply the standard security headers to an outgoing response's header map,
/// branching on `req_path` for the immersive-login framed-route relax.
///
/// All values except the framed-route `frame-ancestors` are static ASCII
/// strings; this is a pure writer with no failure modes.
///
/// - **Framed login routes** (`/login`, `/signup`, interactive `/consent` —
///   [`is_framed_route`]) AND a non-empty `frame_ancestor_origins`: emit
///   `frame-ancestors 'self' <origins>` and SKIP `X-Frame-Options` entirely
///   (a framed document must NOT carry `XFO: DENY`, design §6.3). The CSP is
///   set unconditionally (not `_if_absent`) so it WINS over the strict baseline
///   even though the framed handlers themselves set no CSP — and these handlers
///   render no inline script (no nonce needed), so there is no nonce-CSP to
///   clobber.
/// - **Everything else** (including the framed routes when no console origin is
///   configured — dev / single-origin): the fail-closed default —
///   `X-Frame-Options: DENY` + CSP `frame-ancestors 'none'`. The baseline CSP
///   is `_if_absent` so a handler that DID render an inline-script nonce-CSP
///   (token interstitial, etc.) keeps its own.
pub fn apply(headers: &mut HeaderMap, req_path: &str, frame_ancestor_origins: &[String]) {
    static_set(
        headers,
        "strict-transport-security",
        "max-age=63072000; includeSubDomains; preload",
    );
    static_set(headers, "x-content-type-options", "nosniff");
    static_set(headers, "referrer-policy", "no-referrer");
    static_set(
        headers,
        "permissions-policy",
        "camera=(), microphone=(), geolocation=(), payment=(), \
         publickey-credentials-get=(self), interest-cohort=()",
    );
    // COOP/CORP unchanged — they govern the auth origin's OWN windows /
    // no-cors subresource embedding, NOT being framed (design §4.3/§6.6).
    static_set(headers, "cross-origin-opener-policy", "same-origin");
    static_set(headers, "cross-origin-resource-policy", "same-origin");
    static_set(headers, "cache-control", "no-store");

    // The relax only kicks in on a framed route that has at least one CONCRETE
    // configured ancestor origin. A route with only poison/empty entries stays
    // on the strict fail-closed default (XFO DENY + frame-ancestors 'none').
    let framed = is_framed_route(req_path)
        && frame_ancestor_origins
            .iter()
            .any(|o| is_concrete_ancestor_source(o));

    if framed {
        // Framed login document: relaxed `frame-ancestors`, NO `X-Frame-Options`.
        // Set the CSP UNCONDITIONALLY (these handlers emit no CSP of their own,
        // and the relaxed `frame-ancestors` must beat the strict baseline).
        static_insert(
            headers,
            "content-security-policy",
            &framed_route_csp(frame_ancestor_origins),
        );
    } else {
        // Fail-closed default: never frameable.
        static_set(headers, "x-frame-options", "DENY");
        // CSP — same shape as proposal §14. `'nonce-...'` and per-page hardening
        // are added by handlers that render inline scripts; the baseline blocks
        // everything else (incl. `frame-ancestors 'none'`).
        static_set_if_absent(
            headers,
            "content-security-policy",
            DEFAULT_CONTENT_SECURITY_POLICY,
        );
    }
}

fn static_set(headers: &mut HeaderMap, name: &'static str, value: &'static str) {
    headers.insert(
        HeaderName::from_static(name),
        HeaderValue::from_static(value),
    );
}

fn static_set_if_absent(headers: &mut HeaderMap, name: &'static str, value: &'static str) {
    let name = HeaderName::from_static(name);
    if !headers.contains_key(&name) {
        headers.insert(name, HeaderValue::from_static(value));
    }
}

/// Unconditional insert of the RUNTIME-built framed-route CSP, whose
/// `frame-ancestors` carries the configured console origin(s). A header value
/// can only fail construction on a control char / non-visible-ASCII byte; both
/// the config layer ([`AuthConfig::resolve`]) and the builder
/// ([`is_concrete_ancestor_source`]) reject such origins, so the CSP we build is
/// all printable ASCII and the fallback is unreachable in practice. Should a
/// future path ever feed a poison value here, fail-closed FULLY: emit the strict
/// `frame-ancestors 'none'` default AND re-insert `X-Frame-Options: DENY` (which
/// the framed branch had skipped), so the degraded header set exactly matches
/// the strict default rather than leaving a route with neither.
fn static_insert(headers: &mut HeaderMap, name: &'static str, value: &str) {
    match HeaderValue::from_str(value) {
        Ok(header_value) => {
            headers.insert(HeaderName::from_static(name), header_value);
        }
        Err(_) => {
            // Restore the complete strict default, not just the CSP — the framed
            // path skipped XFO, so without this a poison origin would leave the
            // route with NO X-Frame-Options (defense-in-depth consistency).
            static_set(headers, "x-frame-options", "DENY");
            headers.insert(
                HeaderName::from_static(name),
                HeaderValue::from_static(DEFAULT_CONTENT_SECURITY_POLICY),
            );
        }
    }
}

/// Request metadata shared by audit emission and trace correlation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestContext {
    pub request_id: String,
    pub ip: Option<IpAddr>,
    pub user_agent: Option<String>,
}

impl RequestContext {
    #[must_use]
    pub fn from_http_request(req: &HttpRequest) -> Self {
        Self {
            request_id: request_id(req.headers()),
            // SEC-3: the trusted gateway-authored client IP (rightmost
            // validated XFF token), not the spoofable leftmost.
            ip: trusted_forwarded_ip(req.headers())
                .or_else(|| req.peer_addr().map(|addr| addr.ip())),
            user_agent: req
                .headers()
                .get("user-agent")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
        }
    }

    #[must_use]
    fn from_web_request<Err>(req: &WebRequest<Err>) -> Self {
        Self {
            request_id: request_id(req.headers()),
            // SEC-3: trusted gateway-authored client IP, as above.
            ip: trusted_forwarded_ip(req.headers())
                .or_else(|| req.peer_addr().map(|addr| addr.ip())),
            user_agent: req
                .headers()
                .get("user-agent")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
        }
    }
}

fn request_id(headers: &HeaderMap) -> String {
    headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().to_string())
}


/// ntex middleware factory that stows [`RequestContext`] in request extensions.
#[derive(Clone, Copy, Debug, Default)]
pub struct RequestContextMiddleware;

impl<S> Middleware<S, SharedCfg> for RequestContextMiddleware {
    type Service = RequestContextService<S>;

    fn create(&self, service: S, _: SharedCfg) -> Self::Service {
        RequestContextService { service }
    }
}

#[derive(Debug)]
pub struct RequestContextService<S> {
    service: S,
}

#[allow(clippy::future_not_send)]
impl<S, E> Service<WebRequest<E>> for RequestContextService<S>
where
    S: Service<WebRequest<E>, Response = WebResponse>,
{
    type Response = WebResponse;
    type Error = S::Error;

    ntex::forward_poll!(service);
    ntex::forward_ready!(service);
    ntex::forward_shutdown!(service);

    async fn call(
        &self,
        req: WebRequest<E>,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        let request_context = RequestContext::from_web_request(&req);
        req.extensions_mut().insert(request_context);
        ctx.call(&self.service, req).await
    }
}

/// ntex middleware factory that runs [`apply`] on every outgoing response.
///
/// Installed in [`crate::server::run`] via
/// `App::middleware(SecurityHeaders::new(frame_ancestor_origins))`. Carries the
/// console origin allowlist so the route-aware framing relax (design §4.3) can
/// emit `frame-ancestors 'self' <origins>` on the framed login routes.
#[derive(Clone, Debug, Default)]
pub struct SecurityHeaders {
    /// Console origin(s) the framed login routes admit via `frame-ancestors`.
    /// Empty ⇒ the relax is a no-op (strict default everywhere).
    frame_ancestor_origins: std::rc::Rc<Vec<String>>,
}

impl SecurityHeaders {
    /// Construct the middleware with the deployment's console origin allowlist
    /// (`AuthConfig::frame_ancestor_origins`). Pass an empty vec to keep the
    /// strict fail-closed default on every route (dev / single-origin).
    #[must_use]
    pub fn new(frame_ancestor_origins: Vec<String>) -> Self {
        Self {
            frame_ancestor_origins: std::rc::Rc::new(frame_ancestor_origins),
        }
    }
}

impl<S> Middleware<S, SharedCfg> for SecurityHeaders {
    type Service = SecurityHeadersService<S>;

    fn create(&self, service: S, _: SharedCfg) -> Self::Service {
        SecurityHeadersService {
            service,
            frame_ancestor_origins: self.frame_ancestor_origins.clone(),
        }
    }
}

#[derive(Debug)]
pub struct SecurityHeadersService<S> {
    service: S,
    frame_ancestor_origins: std::rc::Rc<Vec<String>>,
}

// ntex `WebRequest` and `ServiceCtx` are intentionally `!Send` (single-threaded
// executor), so the futures here can't be `Send`; same shape as ntex's own
// `DefaultHeaders` middleware.
#[allow(clippy::future_not_send)]
impl<S, E> Service<WebRequest<E>> for SecurityHeadersService<S>
where
    S: Service<WebRequest<E>, Response = WebResponse>,
{
    type Response = WebResponse;
    type Error = S::Error;

    ntex::forward_poll!(service);
    ntex::forward_ready!(service);
    ntex::forward_shutdown!(service);

    async fn call(
        &self,
        req: WebRequest<E>,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        // Capture the request PATH before dispatch — the middleware runs `apply`
        // AFTER the handler (so it can override XFO, which the handler cannot
        // suppress), but it needs the request path to decide framed-vs-strict.
        let req_path = req.path().to_string();
        let mut res = ctx.call(&self.service, req).await?;
        apply(res.headers_mut(), &req_path, &self.frame_ancestor_origins);
        Ok(res)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ntex::web::test;

    /// Pure-helper exercise of the route-aware framing builder (design §9): the
    /// framing logic is testable without booting the full server: `apply`
    /// works against a bare `HeaderMap`. Given a path + the configured console
    /// origin(s), assert the exact framed-vs-strict header set. This is the
    /// offline regression guard for the iframe pivot.
    const CONSOLE: &str = "https://console.zeroship.ai";

    fn header(headers: &HeaderMap, name: &str) -> Option<String> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }

    fn applied(path: &str, origins: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let origins: Vec<String> = origins.iter().map(|s| (*s).to_string()).collect();
        apply(&mut headers, path, &origins);
        headers
    }

    #[test]
    fn framed_routes_emit_relaxed_frame_ancestors_and_drop_xfo() {
        for path in ["/login", "/signup", "/consent"] {
            let headers = applied(path, &[CONSOLE]);

            // (a) NO X-Frame-Options on a framed route — a legacy UA honoring
            // XFO must not refuse the frame the CSP allows (§6.3).
            assert!(
                header(&headers, "x-frame-options").is_none(),
                "{path}: framed route must NOT set X-Frame-Options"
            );
            // (b) frame-ancestors admits 'self' + the configured console origin.
            let csp = header(&headers, "content-security-policy").expect("csp");
            assert!(
                csp.contains(&format!("frame-ancestors 'self' {CONSOLE}")),
                "{path}: CSP must allow the console origin; got {csp}"
            );
            assert!(
                !csp.contains("frame-ancestors 'none'"),
                "{path}: framed route must NOT keep frame-ancestors 'none'; got {csp}"
            );
            // No wildcard ever (§6.2).
            assert!(!csp.contains('*') || csp.contains("https://*.zeroship.ai"),
                "{path}: the only '*' allowed is the img-src host glob; got {csp}");
        }
    }

    #[test]
    fn non_framed_routes_keep_xfo_deny_and_frame_ancestors_none() {
        // A representative non-framed route AND the federated bounce — both stay
        // strict (the federated IdP page is never framed; it stays a popup).
        for path in ["/me", "/device", "/oauth/google/start", "/static/style.css"] {
            let headers = applied(path, &[CONSOLE]);
            assert_eq!(
                header(&headers, "x-frame-options").as_deref(),
                Some("DENY"),
                "{path}: non-framed route must keep X-Frame-Options: DENY"
            );
            let csp = header(&headers, "content-security-policy").expect("csp");
            assert!(
                csp.contains("frame-ancestors 'none'"),
                "{path}: non-framed route must keep frame-ancestors 'none'; got {csp}"
            );
        }
    }

    #[test]
    fn empty_origins_keeps_strict_default_even_on_framed_path() {
        // No console origin configured (dev / single-origin) ⇒ the relax is a
        // no-op: even `/login` falls back to the fail-closed default.
        let headers = applied("/login", &[]);
        assert_eq!(
            header(&headers, "x-frame-options").as_deref(),
            Some("DENY"),
            "empty allowlist must keep XFO DENY even on a framed path"
        );
        let csp = header(&headers, "content-security-policy").expect("csp");
        assert!(
            csp.contains("frame-ancestors 'none'"),
            "empty allowlist must keep frame-ancestors 'none' even on a framed path; got {csp}"
        );
    }

    #[test]
    fn google_route_is_not_in_the_framed_set() {
        assert!(!is_framed_route("/oauth/google/start"));
        assert!(!is_framed_route("/oauth/google/callback"));
        assert!(is_framed_route("/login"));
        assert!(is_framed_route("/signup"));
        assert!(is_framed_route("/consent"));
    }

    #[test]
    fn framed_csp_joins_multiple_origins_and_skips_empties() {
        // The Vec future-proofs staging/preview origins (§10.1); empties from a
        // trailing comma are skipped.
        let csp = framed_route_csp(&[
            CONSOLE.to_string(),
            String::new(),
            "https://staging-console.zeroship.ai".to_string(),
        ]);
        assert!(csp.contains(
            "frame-ancestors 'self' https://console.zeroship.ai https://staging-console.zeroship.ai;"
        ), "got {csp}");
    }

    #[test]
    fn framed_csp_drops_wildcards_and_poison_origins() {
        // Defense in depth (§6.2): even if a non-concrete origin reached the
        // builder, it must NOT be spliced into `frame-ancestors` — a `*` would
        // re-admit every creator app and defeat the one-embedder property.
        let csp = framed_route_csp(&[
            "https://*.zeroship.ai".to_string(),
            "*".to_string(),
            "'self'".to_string(),
            "data:".to_string(),
            "console.zeroship.ai".to_string(), // no scheme
            "https://a.zeroship.ai;script-src *".to_string(),
            CONSOLE.to_string(), // the only concrete entry
        ]);
        // Only 'self' (builder-added) + the one concrete console origin remain.
        assert!(
            csp.contains("frame-ancestors 'self' https://console.zeroship.ai;"),
            "got {csp}"
        );
        assert!(
            !csp.contains("frame-ancestors 'self' https://*"),
            "a wildcard origin must never reach the frame-ancestors list; got {csp}"
        );
        // The only `*` permitted anywhere is the img-src host glob.
        let ancestors = csp
            .split("frame-ancestors ")
            .nth(1)
            .and_then(|s| s.split(';').next())
            .unwrap_or("");
        assert!(
            !ancestors.contains('*'),
            "frame-ancestors must contain no wildcard; got {ancestors:?}"
        );
    }

    #[test]
    fn is_concrete_ancestor_source_predicate() {
        assert!(is_concrete_ancestor_source("https://console.zeroship.ai"));
        assert!(is_concrete_ancestor_source(
            "https://console.zeroship.localhost:8443"
        ));
        for bad in [
            "",
            "   ",
            "*",
            "https://*.zeroship.ai",
            "'self'",
            "'none'",
            "data:",
            "console.zeroship.ai",
            "ftp://x.zeroship.ai",
            "https://a.zeroship.ai https://b.ai",
            "https://a.zeroship.ai;script-src *",
            "https://a.zeroship.ai,https://b.ai",
        ] {
            assert!(
                !is_concrete_ancestor_source(bad),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn framed_route_with_only_poison_origins_stays_strict() {
        // A framed path configured with ONLY a wildcard ⇒ no concrete origin ⇒
        // the relax does NOT kick in: the route keeps XFO DENY + 'none'.
        let headers = applied("/login", &["https://*.zeroship.ai", "*"]);
        assert_eq!(
            header(&headers, "x-frame-options").as_deref(),
            Some("DENY"),
            "a framed route with only wildcard origins must stay fail-closed"
        );
        let csp = header(&headers, "content-security-policy").expect("csp");
        assert!(
            csp.contains("frame-ancestors 'none'"),
            "no concrete origin ⇒ strict default; got {csp}"
        );
    }

    #[test]
    fn client_ip_takes_trusted_rightmost_xff_token_and_validates_ip() {
        // SEC-3: the gateway is the only trusted hop. It strips any
        // client-supplied X-Forwarded-For and re-authors a single token = the
        // real peer. The auth service must therefore read the RIGHTMOST
        // (closest-hop, gateway-authored) X-Forwarded-For token, NOT the
        // leftmost (client-spoofable) one — `connection_info().remote()` takes
        // the leftmost and is unsafe here.
        //
        // Pre-fix `client_ip` returns the leftmost `1.2.3.4` (the spoofed
        // value an attacker prepends) → RED.
        let req = test::TestRequest::default()
            // Attacker prepends a forged leftmost token; the gateway-authored
            // real peer is the rightmost.
            .header("x-forwarded-for", "1.2.3.4, 203.0.113.7")
            .to_http_request();
        assert_eq!(
            client_ip(&req),
            "203.0.113.7",
            "must use the rightmost (gateway-authored) XFF token, not the spoofable leftmost"
        );
    }

    #[test]
    fn client_ip_rejects_non_ip_bucket_key() {
        // SEC-3: a non-IP X-Forwarded-For value must NEVER be used verbatim as
        // a rate-limit bucket key (that lets an attacker mint unbounded
        // `zeroship.rate_limits` rows). With no socket peer in the test
        // fixture, an unparseable value falls back to the `0.0.0.0` sentinel.
        //
        // Pre-fix `client_ip` returns the raw `not-an-ip` string → RED.
        let req = test::TestRequest::default()
            .header("x-forwarded-for", "not-an-ip")
            .to_http_request();
        assert_eq!(
            client_ip(&req),
            "0.0.0.0",
            "a non-IP forwarded value must be rejected, not used as a bucket key"
        );

        // A garbage token that would balloon the key space is likewise rejected.
        let req2 = test::TestRequest::default()
            .header("x-forwarded-for", "'; DROP TABLE rate_limits; --")
            .to_http_request();
        assert_eq!(client_ip(&req2), "0.0.0.0");
    }

    #[test]
    fn client_ip_accepts_single_valid_token() {
        // The common gateway-authored single-token case still resolves to that
        // IP (unchanged behavior for the trusted path).
        let req = test::TestRequest::default()
            .header("x-forwarded-for", "198.51.100.9")
            .to_http_request();
        assert_eq!(client_ip(&req), "198.51.100.9");
    }

    #[test]
    fn cross_cutting_headers_present_on_both_branches() {
        for (path, origins) in [("/login", &[CONSOLE][..]), ("/me", &[CONSOLE][..])] {
            let headers = applied(path, origins);
            assert_eq!(
                header(&headers, "referrer-policy").as_deref(),
                Some("no-referrer"),
                "{path}: referrer-policy unchanged on both branches"
            );
            assert_eq!(
                header(&headers, "cross-origin-opener-policy").as_deref(),
                Some("same-origin"),
                "{path}: COOP unchanged (governs the auth origin's own windows)"
            );
            assert_eq!(
                header(&headers, "cross-origin-resource-policy").as_deref(),
                Some("same-origin"),
                "{path}: CORP unchanged (not relaxed for top-level framing)"
            );
            assert!(
                header(&headers, "strict-transport-security").is_some(),
                "{path}: HSTS unchanged"
            );
        }
    }
}
