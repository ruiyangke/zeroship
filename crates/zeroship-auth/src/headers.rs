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

use std::net::IpAddr;

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

/// Per-response marker naming the one origin a form on the rendered page may
/// reach after submission, so [`apply`] can widen that page's `form-action`.
///
/// Chromium enforces `form-action` across the WHOLE form-submission redirect
/// chain: a consent decision is a form POST whose server response 303s to the
/// relying party's registered `redirect_uri`, and the cross-origin hop is
/// blocked unless the FORM DOCUMENT's policy names that origin, so the click
/// silently does nothing. A handler that renders such a form resolves the
/// origin through the single registry-checking helper
/// ([`FormActionOrigin::registered`]), inserts this marker, and the
/// security-header middleware takes it, splices it into whichever CSP the
/// response carries, and drops the marker.
///
/// The wrapped origin is always a tuple (`scheme://host[:port]`); a host-less
/// custom scheme yields no marker, so it cannot widen the directive.
#[derive(Clone, Debug)]
pub(crate) struct FormActionOrigin(url::Origin);

impl FormActionOrigin {
    /// The marker for a parsed native authorize request, only when the client
    /// registry registers its `redirect_uri`.
    ///
    /// The field is private, so this is the only way to produce a marker:
    /// [`crate::oidc::auth_request::AuthRequest::parse_return_to`] only
    /// shape-checks a `return_to` and never consults the registry, and a
    /// crafted `/login?return_to=...` or `/magic/...` must not name an origin.
    /// The named client must exist and list the `redirect_uri` verbatim, the
    /// check consent makes before it renders a decision form. An unknown
    /// client, an unregistered `redirect_uri` or a non-tuple scheme yields
    /// `None`, leaving `form-action` exactly `'self'`.
    pub(crate) async fn registered(
        db: &compio_postgres::Client,
        request: &crate::oidc::auth_request::AuthRequest,
    ) -> Option<Self> {
        let client = crate::ui::consent::load_native_oauth_client(db, &request.client_id)
            .await
            .ok()?;
        if !client
            .redirect_uris
            .iter()
            .any(|registered| registered == &request.redirect_uri)
        {
            return None;
        }
        form_action_origin(&request.redirect_uri).map(Self)
    }

    /// [`Self::registered`] for a call site that holds the raw `return_to`.
    pub(crate) async fn registered_for_return_to(
        db: &compio_postgres::Client,
        return_to: &str,
    ) -> Option<Self> {
        let request = crate::oidc::auth_request::AuthRequest::parse_return_to(return_to).ok()?;
        Self::registered(db, &request).await
    }
}

/// The tuple origin of a redirect URI, or `None`.
///
/// A redirect URI's path, query and fragment do not name a `form-action`
/// source; a default port is dropped; userinfo is not part of an origin. An
/// opaque origin (a host-less custom scheme) has nothing to name and yields
/// `None`. This is a pure conversion: whether the redirect URI is a REGISTERED
/// callback is decided by [`FormActionOrigin::registered`].
#[must_use]
pub(crate) fn form_action_origin(redirect_uri: &str) -> Option<url::Origin> {
    let url = url::Url::parse(redirect_uri).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let origin = url.origin();
    origin.is_tuple().then_some(origin)
}

/// Insert the marker (when there is one) into a rendered form document's
/// response. See [`FormActionOrigin`] for why only form documents carry it.
pub(crate) fn attach_form_action_origin(
    response: &mut ntex::web::HttpResponse,
    origin: Option<FormActionOrigin>,
) {
    if let Some(origin) = origin {
        response.extensions_mut().insert(origin);
    }
}

/// The exact set of request paths whose responses render INSIDE the immersive
/// login iframe and therefore need the relaxed `frame-ancestors` (design §4.3).
/// `/login` + `/signup` cover both their GET render and their POST error
/// re-render (`render_login_error` / the signup error render — same path, same
/// middleware pass). `/consent` covers the interactive consent render, the
/// only way this OP reaches a consent decision. Federated bounces (`/oauth/google`) are
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

/// A CSP `frame-ancestors` source MUST be a concrete `scheme://host[:port]` —
/// NO wildcard, NO CSP-list/header-injecting bytes (design §6.2). The deployment
/// config layer (`AuthConfig::resolve`) already drops non-concrete origins, so
/// in practice the live builder only sees clean origins; this is the
/// defense-in-depth guard so even a future code path that fed an unsanitized
/// origin here could not widen the allowlist or break the header value. A `*`
/// (e.g. `https://*.zeroship.ai`, the bare `*`) would re-admit every creator
/// app, so it is rejected outright.
///
/// The verdict comes from PARSING the value and requiring its origin to
/// round-trip byte for byte. A prefix check would accept `https://u@h/p?q#f`
/// (userinfo, path, query, fragment are not origin) and a character denylist
/// would miss the bytes the URL parser strips. Round-tripping admits exactly
/// the strings the browser would treat as that origin and nothing else.
#[must_use]
fn is_concrete_origin(origin: &str) -> bool {
    let o = origin.trim();
    // `https://*.zeroship.ai` is a valid host to the URL parser and round-trips,
    // but the browser reads the `*` as a CSP host wildcard that re-admits every
    // creator app. It is a source, not a concrete origin.
    if o.contains('*') {
        return false;
    }
    let Ok(url) = url::Url::parse(o) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    url.origin().ascii_serialization() == o
}

/// Build the CSP for a FRAMED login route: the baseline default-src/script-src/
/// … shape with `frame-ancestors 'none'` replaced by
/// `frame-ancestors 'self' <origins…>`. `'self'` keeps the auth origin's own
/// pages framing each other; each configured origin is one cross-site embedder
/// (the console) the browser will admit. Only CONCRETE origins
/// ([`is_concrete_origin`]) are spliced - a wildcard / poison entry is
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
        if is_concrete_origin(origin) {
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

/// Splice already-validated origins into a baseline CSP's `form-action`
/// directive.
///
/// The directive is located by its NAME from the parsed `;`-separated
/// directive list, not by searching for the substring `form-action` anywhere in
/// the policy: a value such as `frame-ancestors https://form-action.example`
/// contains it, and a substring match would splice into the wrong directive.
/// The rebuilt policy preserves every other directive byte for byte (modulo the
/// `; ` separators). A policy that carries no `form-action` directive is
/// returned unchanged and logs at error level - inventing a directive would be
/// a policy change, not a splice. With no extras the baseline is returned byte
/// for byte, so the common path does not depend on the marker having been
/// absent.
#[must_use]
fn with_form_action(base: String, extras: &[url::Origin]) -> String {
    if extras.is_empty() {
        return base;
    }
    let mut sources = String::new();
    for origin in extras {
        sources.push(' ');
        sources.push_str(&origin.ascii_serialization());
    }
    let mut rebuilt = String::with_capacity(base.len() + sources.len());
    let mut found = false;
    for directive in base.split(';').map(str::trim).filter(|d| !d.is_empty()) {
        if !rebuilt.is_empty() {
            rebuilt.push_str("; ");
        }
        // The directive NAME is the first whitespace-delimited token; matching
        // the whole token is what keeps `frame-ancestors
        // https://form-action.example` from being read as `form-action`.
        if !found && directive.split_whitespace().next() == Some("form-action") {
            rebuilt.push_str("form-action 'self'");
            rebuilt.push_str(&sources);
            found = true;
        } else {
            rebuilt.push_str(directive);
        }
    }
    if !found {
        tracing::error!(
            "CSP carries no form-action directive; refusing to splice form-action origins {sources}"
        );
        return base;
    }
    rebuilt
}

/// Client IP for rate-limiting and audit, as a string.
///
/// The auth service runs behind the edge proxy (`deploy/ops/Caddyfile`, which
/// routes `auth.<domain>` straight to this service), the SOLE trusted hop. We
/// read the RIGHTMOST XFF token, NOT the leftmost, which
/// `connection_info().remote()` returns and which a caller can prepend to spoof
/// a per-IP rate-limit bucket - and REQUIRE it to parse as an IP before it is
/// used as a bucket key (an unvalidated value would let an attacker mint
/// unbounded `zeroship.rate_limits` rows). When the forwarded value is absent
/// or unparseable we fall back to the raw socket peer, then the `"0.0.0.0"`
/// sentinel (e.g. unit tests with neither).
///
/// WHY THE RIGHTMOST TOKEN IS THE PROXY'S, not assumed: Caddy 2.11.4's
/// `reverse_proxy` writes XFF two ways and the rightmost entry is its own
/// under both:
///
///   * with no `servers { trusted_proxies }` - the case here, the repo
///     configures none - it REPLACES the header outright with the peer it
///     accepted the connection from. A forged `X-Forwarded-For: 1.2.3.4`
///     arrives at this service as the caller's real address and nothing else.
///   * with `trusted_proxies` covering the peer it APPENDS, so a forged
///     `1.2.3.4` becomes `1.2.3.4, <caddy's peer>` - caller text on the left,
///     the trusted hop's own entry rightmost.
///
/// Caddy does NOT scrub `X-Real-IP` or `Forwarded`; both reach this service
/// verbatim from the caller. That is safe only because nothing here reads
/// them - [`trusted_client_ip`] consults `x-forwarded-for` and nothing else.
/// Do not start reading either one without re-checking this.
///
/// The resolution is [`zeroship_core::client_ip`], shared with the gateway and
/// the control plane. `trust_proxy` is passed as `true` unconditionally here,
/// and only here: this service is never published to the internet, so the edge
/// proxy is the only writer of the header it reads. The gateway and control
/// take the flag from configuration because they can be fronted or not.
///
/// THAT PREMISE IS UNCHECKED. Publishing this service in
/// `deploy/compose/docker-compose.yml` would make the unconditional `true`
/// above a client-IP spoof, and nothing would say so.
#[must_use]
pub(crate) fn client_ip(req: &HttpRequest) -> String {
    trusted_client_ip(req.headers(), req.peer_addr().map(|addr| addr.ip()))
        .map_or_else(|| "0.0.0.0".to_string(), |ip| ip.to_string())
}

/// The trusted client address: the proxy-authored `X-Forwarded-For` entry,
/// else the socket peer. Delegates to [`zeroship_core::client_ip`], which
/// documents why the rightmost entry is the trusted one.
///
/// `x-forwarded-for` is the ONLY header consulted. See [`client_ip`] for why
/// that is load-bearing and not merely sufficient.
fn trusted_client_ip(headers: &HeaderMap, peer_ip: Option<IpAddr>) -> Option<IpAddr> {
    let forwarded_for = headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok());
    zeroship_core::client_ip::resolve_client_ip(forwarded_for, peer_ip, true)
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

/// Apply the standard security headers to an outgoing response's header map.
///
/// Branches on `req_path` for the immersive-login framed-route relax and
/// splices in the [`FormActionOrigin`] the response carries, if any. All values
/// are static ASCII strings; this is a pure writer with no failure modes.
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
///   (token interstitial, etc.) keeps its own - but its `form-action` is still
///   widened by `form_action_origins` (see [`FormActionOrigin`]).
pub fn apply(
    headers: &mut HeaderMap,
    req_path: &str,
    frame_ancestor_origins: &[String],
    form_action_origins: &[url::Origin],
) {
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
    // `no-store` is the fail-closed DEFAULT, not an override: this middleware
    // runs after the handler, so an unconditional insert would silently replace
    // the deliberate cacheability of the two public metadata documents (JWKS +
    // discovery), which every other handler in the crate never sets to anything
    // but `no-store` anyway.
    static_set_if_absent(headers, "cache-control", "no-store");

    // The relax only kicks in on a framed route that has at least one CONCRETE
    // configured ancestor origin. A route with only poison/empty entries stays
    // on the strict fail-closed default (XFO DENY + frame-ancestors 'none').
    let framed = is_framed_route(req_path)
        && frame_ancestor_origins
            .iter()
            .any(|o| is_concrete_origin(o));

    if framed {
        // Framed login document: relaxed `frame-ancestors`, NO `X-Frame-Options`.
        // Set the CSP UNCONDITIONALLY (these handlers emit no CSP of their own,
        // and the relaxed `frame-ancestors` must beat the strict baseline).
        static_insert(
            headers,
            "content-security-policy",
            &with_form_action(framed_route_csp(frame_ancestor_origins), form_action_origins),
        );
    } else {
        // Fail-closed default: never frameable.
        static_set(headers, "x-frame-options", "DENY");
        apply_default_csp(headers, form_action_origins);
    }
}

/// The strict-branch CSP: the baseline `frame-ancestors 'none'` shape, widened
/// with `form_action_origins` when there are any.
///
/// A handler-set policy (an inline-script nonce) keeps every directive it owns,
/// but its `form-action` is widened too (see [`FormActionOrigin`]).
fn apply_default_csp(headers: &mut HeaderMap, form_action_origins: &[url::Origin]) {
    match headers
        .get("content-security-policy")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
    {
        Some(existing) if !form_action_origins.is_empty() => {
            static_insert(
                headers,
                "content-security-policy",
                &with_form_action(existing, form_action_origins),
            );
        }
        Some(_) => {}
        None if !form_action_origins.is_empty() => {
            static_insert(
                headers,
                "content-security-policy",
                &with_form_action(
                    DEFAULT_CONTENT_SECURITY_POLICY.to_owned(),
                    form_action_origins,
                ),
            );
        }
        None => static_set_if_absent(
            headers,
            "content-security-policy",
            DEFAULT_CONTENT_SECURITY_POLICY,
        ),
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
/// ([`is_concrete_origin`]) reject such origins, so the CSP we build is
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
            ip: trusted_client_ip(req.headers(), req.peer_addr().map(|addr| addr.ip())),
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
            ip: trusted_client_ip(req.headers(), req.peer_addr().map(|addr| addr.ip())),
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
        // Take the handler's `form-action` marker off the response so it never
        // reaches the browser, then splice its origin into whichever CSP the
        // response carries.
        let form_action_origins: Vec<url::Origin> = res
            .response()
            .extensions_mut()
            .remove::<FormActionOrigin>()
            .map(|marker| vec![marker.0])
            .unwrap_or_default();
        apply(
            res.headers_mut(),
            &req_path,
            &self.frame_ancestor_origins,
            &form_action_origins,
        );
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
        apply(&mut headers, path, &origins, &[]);
        headers
    }

    fn origin(uri: &str) -> url::Origin {
        form_action_origin(uri).expect("a tuple origin")
    }

    fn csp_directive(csp: &str, name: &str) -> Option<String> {
        csp.split(';')
            .map(str::trim)
            .find(|directive| directive.starts_with(name))
            .map(str::to_owned)
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
    fn is_concrete_origin_predicate() {
        assert!(is_concrete_origin("https://console.zeroship.ai"));
        assert!(is_concrete_origin(
            "https://console.zeroship.localhost:8443"
        ));
        assert!(is_concrete_origin("http://127.0.0.1:9999"));
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
            // A tab, a raw control byte and a non-ASCII host must not survive
            // the parser's normalization and round-trip as a concrete origin.
            "https://console\t.zeroship.ai",
            "https://console\n.zeroship.ai",
            "https://console.\u{7}zeroship.ai",
            "https://éxample.test",
            // Empty host, a lone directive separator, quotes, userinfo and a
            // path are not one concrete `scheme://host[:port]`.
            "https://",
            ";",
            "\"https://console.zeroship.ai\"",
            "https://user:pass@console.zeroship.ai",
            "https://console.zeroship.ai/path",
        ] {
            assert!(
                !is_concrete_origin(bad),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn form_action_origin_names_only_the_tuple_origin() {
        let named = |uri: &str| form_action_origin(uri).map(|origin| origin.ascii_serialization());
        // Path, query and fragment are not part of an origin.
        assert_eq!(
            named("http://127.0.0.1:9999/native-cb?code=x#frag").as_deref(),
            Some("http://127.0.0.1:9999")
        );
        // A default port is dropped; a non-default one is kept.
        assert_eq!(
            named("https://rp.example:443/cb").as_deref(),
            Some("https://rp.example")
        );
        assert_eq!(
            named("http://rp.example:80/cb").as_deref(),
            Some("http://rp.example")
        );
        assert_eq!(
            named("https://rp.example:8443/cb").as_deref(),
            Some("https://rp.example:8443")
        );
        // Userinfo is not part of an origin.
        assert_eq!(
            named("https://user:pass@rp.example/cb").as_deref(),
            Some("https://rp.example")
        );
        // Opaque and non-browser schemes name no `form-action` destination.
        for opaque in [
            "javascript:alert(1)",
            "data:text/html,<h1>x</h1>",
            "blob:https://rp.example/00000000-0000-0000-0000-000000000000",
            "myapp://callback",
            "file:///tmp/x",
            "not a url",
        ] {
            assert_eq!(
                named(opaque),
                None,
                "{opaque:?} must not become a form-action origin"
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

    /// The consent-page contract: the one registered cross-origin callback is
    /// spliced into `form-action` on the framed consent document, and every
    /// other directive is unchanged.
    #[test]
    fn form_action_extends_a_framed_csp() {
        let mut headers = HeaderMap::new();
        apply(
            &mut headers,
            "/consent",
            &[CONSOLE.to_string()],
            &[origin("http://127.0.0.1:9999/native-cb")],
        );

        let csp = header(&headers, "content-security-policy").expect("csp");
        assert_eq!(
            csp_directive(&csp, "form-action").as_deref(),
            Some("form-action 'self' http://127.0.0.1:9999"),
            "the registered callback origin must extend form-action: {csp}"
        );
        assert!(
            csp.contains(&format!("frame-ancestors 'self' {CONSOLE}")),
            "framed ancestors are unchanged: {csp}"
        );
    }

    /// The same extension must work when no console origin is configured (dev /
    /// single-origin), where the consent page takes the strict CSP branch.
    #[test]
    fn form_action_extends_the_strict_csp_too() {
        let mut headers = HeaderMap::new();
        apply(
            &mut headers,
            "/consent",
            &[],
            &[origin("https://rp.example/cb")],
        );

        let csp = header(&headers, "content-security-policy").expect("csp");
        assert_eq!(
            csp_directive(&csp, "form-action").as_deref(),
            Some("form-action 'self' https://rp.example"),
            "strict-branch form-action must be extended: {csp}"
        );
        assert!(
            csp.contains("frame-ancestors 'none'"),
            "the strict branch stays never-frameable: {csp}"
        );
    }

    /// A handler that rendered its own nonce policy keeps it, but its
    /// `form-action` is widened too: the magic-link interstitial is exactly such
    /// a document, and its form's redirect chain ends at the relying party.
    #[test]
    fn form_action_extends_a_handler_set_csp() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("content-security-policy"),
            HeaderValue::from_static(
                "default-src 'self'; script-src 'self' 'nonce-abc'; \
                 form-action 'self'; frame-ancestors 'none'",
            ),
        );
        apply(
            &mut headers,
            "/magic/verify",
            &[],
            &[origin("https://rp.example/cb")],
        );

        let csp = header(&headers, "content-security-policy").expect("csp");
        assert!(
            csp.contains("script-src 'self' 'nonce-abc'"),
            "the handler's nonce must survive the splice: {csp}"
        );
        assert_eq!(
            csp_directive(&csp, "form-action").as_deref(),
            Some("form-action 'self' https://rp.example"),
            "a handler-set form-action must be widened: {csp}"
        );
    }

    /// With no origins, the builder is byte-identical to the baseline. The
    /// common path must not change shape just because the extension exists.
    #[test]
    fn form_action_without_extras_is_byte_identical() {
        assert_eq!(
            with_form_action(DEFAULT_CONTENT_SECURITY_POLICY.to_owned(), &[]),
            DEFAULT_CONTENT_SECURITY_POLICY
        );
    }

    /// The directive is rebuilt from its parsed extent, so a spelling change in
    /// the baseline cannot make the splice silently no-op. A policy with no
    /// `form-action` directive is refused rather than mutated.
    #[test]
    fn with_form_action_rebuilds_the_directive_rather_than_matching_its_text() {
        let widened = with_form_action(
            "default-src 'self'; form-action 'self' https://stale.example; \
             frame-ancestors 'none'"
                .to_owned(),
            &[origin("https://rp.example/cb")],
        );
        assert_eq!(
            csp_directive(&widened, "form-action").as_deref(),
            Some("form-action 'self' https://rp.example"),
            "an existing widened directive must be replaced, not left stale: {widened}"
        );
        assert!(
            !widened.contains("stale.example"),
            "the old directive must not survive: {widened}"
        );
        assert!(
            widened.contains("frame-ancestors 'none'"),
            "the rest of the policy is unchanged: {widened}"
        );

        let no_directive = with_form_action(
            "default-src 'self'; frame-ancestors 'none'".to_owned(),
            &[origin("https://rp.example/cb")],
        );
        assert_eq!(
            no_directive, "default-src 'self'; frame-ancestors 'none'",
            "a policy without form-action must not be rewritten"
        );
    }

    /// The directive is located by NAME, so a value in an EARLIER directive that
    /// merely contains the substring `form-action` is not mistaken for the
    /// directive. With a substring match, `frame-ancestors
    /// https://form-action-demo.example` would be the match, the real
    /// `form-action` would be left stale, and the ancestor directive would be
    /// corrupted.
    #[test]
    fn with_form_action_matches_the_directive_name_not_a_substring() {
        let widened = with_form_action(
            "frame-ancestors https://form-action-demo.example; \
             form-action 'self'; \
             default-src 'self'"
                .to_owned(),
            &[origin("https://rp.example/cb")],
        );
        assert_eq!(
            csp_directive(&widened, "frame-ancestors").as_deref(),
            Some("frame-ancestors https://form-action-demo.example"),
            "a directive value containing the substring must survive untouched: {widened}"
        );
        assert_eq!(
            csp_directive(&widened, "form-action").as_deref(),
            Some("form-action 'self' https://rp.example"),
            "only the real form-action directive is widened: {widened}"
        );
    }

    /// The middleware is the consumer: it takes the typed marker off the
    /// response, so it can never reach the browser, and splices its origin into
    /// the CSP the handler rendered.
    #[ntex::test]
    async fn middleware_consumes_the_marker_and_widens_the_rendered_csp() {
        let app = test::init_service(
            ntex::web::App::new()
                .middleware(SecurityHeaders::new(vec![CONSOLE.to_string()]))
                .default_service(ntex::web::to(|| async {
                    let response = ntex::web::HttpResponse::Ok().body("form");
                    response.extensions_mut().insert(FormActionOrigin(
                        form_action_origin("https://rp.example/cb").expect("tuple origin"),
                    ));
                    response
                })),
        )
        .await;
        let req = test::TestRequest::with_uri("/consent").to_request();
        let res = test::call_service(&app, req).await;

        assert!(
            res.response()
                .extensions()
                .get::<FormActionOrigin>()
                .is_none(),
            "the marker must be consumed before the response leaves the middleware"
        );
        let csp = header(res.headers(), "content-security-policy").expect("csp");
        assert_eq!(
            csp_directive(&csp, "form-action").as_deref(),
            Some("form-action 'self' https://rp.example"),
            "the rendered CSP must carry the widened directive: {csp}"
        );
    }

    #[test]
    fn client_ip_takes_trusted_rightmost_xff_token_and_validates_ip() {
        // SEC-3: the edge proxy is the only trusted hop, and the entry it
        // authors is the RIGHTMOST. This service must key its per-IP rate-limit
        // buckets on that one, NOT the leftmost (caller-spoofable) token -
        // `connection_info().remote()` takes the leftmost and is unsafe here.
        //
        // This is the ONLY thing standing between a caller and unlimited
        // rate-limit evasion: nothing sanitises the header before this
        // function.
        let req = test::TestRequest::default()
            // Attacker prepends a forged leftmost token; the proxy-authored
            // real peer is the rightmost.
            .header("x-forwarded-for", "1.2.3.4, 203.0.113.7")
            .to_http_request();
        assert_eq!(
            client_ip(&req),
            "203.0.113.7",
            "must use the rightmost (proxy-authored) XFF token, not the spoofable leftmost"
        );
    }

    #[test]
    fn client_ip_ignores_the_forwarding_headers_the_edge_does_not_scrub() {
        // Caddy 2.11.4's `reverse_proxy` takes ownership of
        // `X-Forwarded-For` (replacing it outright when no
        // `trusted_proxies` is configured, as here), but passes `X-Real-IP` and
        // `Forwarded` through VERBATIM from the caller. Both are therefore
        // attacker-controlled by the time they reach this service.
        //
        // Only the fact that this service reads none but `x-forwarded-for`
        // keeps a forged `X-Real-IP` / `Forwarded` out of the rate-limit bucket
        // key. Wire either one into `trusted_client_ip` and this goes RED.
        let req = test::TestRequest::default()
            .header("x-forwarded-for", "203.0.113.7")
            .header("x-real-ip", "1.2.3.4")
            .header("forwarded", "for=1.2.3.4")
            .to_http_request();
        assert_eq!(
            client_ip(&req),
            "203.0.113.7",
            "only the proxy-owned XFF may key the bucket; X-Real-IP/Forwarded are caller text"
        );

        // With no XFF at all the spoofable headers still must not win: the
        // fallback is the socket peer, and in this fixture there is none.
        let req = test::TestRequest::default()
            .header("x-real-ip", "1.2.3.4")
            .header("forwarded", "for=1.2.3.4")
            .to_http_request();
        assert_eq!(
            client_ip(&req),
            "0.0.0.0",
            "absent XFF must fall back to peer/sentinel, never to a caller-supplied header"
        );
    }

    #[test]
    fn client_ip_rejects_non_ip_bucket_key() {
        // SEC-3: a non-IP X-Forwarded-For value must NEVER be used verbatim as
        // a rate-limit bucket key (that lets an attacker mint unbounded
        // `zeroship.rate_limits` rows). With no socket peer in the test
        // fixture, an unparseable value falls back to the `0.0.0.0` sentinel.
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
