//! `zeroship-gateway` — library crate.
//!
//! The gateway binary (`main.rs`) is a thin entry point: it parses
//! flags, constructs [`GateState`], registers ntex routes, and starts
//! the server. Everything else — routing, auth, proxy, blob cache,
//! sessions, signing, wrapper-token issuance — lives in this library
//! so integration tests under `tests/` can drive the handlers without
//! reaching through the binary.
//!
//! [`GateConfig`] and [`GateState`] live here (not in `main.rs`) for
//! the same reason: tests construct fixtures directly.

pub mod auth;
pub mod backchannel_logout;
pub mod blob_cache;
pub mod compiled;
pub mod dispatch;
pub mod dpop_exchange;
pub mod enforce;
pub mod error;
pub mod idempotency;
pub mod oidc_rp;
pub mod proxy;
pub mod router;
pub mod sessions;
pub mod signing;
pub mod sync;
pub mod wrapper_token;

use std::sync::Arc;

use zeroship_bundle::BlobStore;

/// Boot-time configuration for the gateway. Populated from CLI flags
/// or environment variables in `main.rs`.
#[allow(missing_debug_implementations)]
pub struct GateConfig {
    pub control_url: String,
    pub control_key: String,
    pub worker_urls: Vec<String>,
    pub poll_interval_secs: u64,
    pub auth_secret: String,
    /// Shared secret between gateway and workers. Used to bearer-auth the
    /// `/dispatch` endpoints and HMAC-sign the `ZeroShip-User` header so
    /// workers can verify forwarded identity was not forged by an attacker
    /// with direct network access. Empty disables both checks (dev only).
    pub worker_key: String,
    /// Upstream URL for Ory Hydra's public OIDC endpoints. The gateway
    /// forwards `auth.zeroship.ai/{oauth2,.well-known}/*` (plus
    /// `/userinfo`) here. Compose-internal default points at the
    /// `hydra` service on its 4444 port.
    pub hydra_public: String,
    /// Upstream URL for `crates/auth` — the login/signup UI, OAuth2
    /// consent handlers, and webhook surfaces. Everything on the
    /// `auth.zeroship.ai` host that is NOT an OIDC protocol endpoint
    /// is forwarded here. Defaults to the compose-internal `auth`
    /// service; will be wired through once the service joins compose
    /// (Phase 3 Unit U11).
    pub auth_public: String,
    /// Dev-only flag. When true the gateway emits cookies without the
    /// `Secure` attribute so the localhost HTTP flow works in `pnpm dev`
    /// / docker-compose. Production MUST set this to false — the
    /// `__Host-` cookie prefix RFC 6265bis §4.1.3 requires `Secure`.
    pub insecure_dev: bool,
    /// Public URL the gateway advertises as its own `iss` in
    /// gateway-issued wrapper tokens (Phase 8 U3). In production this is
    /// `https://api.zeroship.ai`; in dev it can be left at the default.
    /// Verifiers pin `iss` to this string, so the value MUST be stable
    /// across gateway restarts.
    pub public_url: String,
}

/// Shared application state passed to every handler via
/// `web::types::State<Arc<GateState>>`.
#[allow(missing_debug_implementations)]
pub struct GateState {
    pub config: GateConfig,
    pub routes: sync::RouteCache,
    pub hash_ring: proxy::HashRing,
    pub rate_limiters: enforce::RateLimitRegistry,
    /// Per-rule rate limits declared in `Action::Worker.rate_limit`.
    /// Layered on top of the global per-app `rate_limiters` — runs
    /// FIRST in the request path so a rule that's already saturated
    /// short-circuits without the global bucket lookup.
    pub per_rule_rate_limits: enforce::PerRuleRateLimitRegistry,
    pub concurrency: enforce::ConcurrencyRegistry,
    /// Content-addressed blob store. The gateway fetches asset bytes
    /// here directly instead of round-tripping through the control
    /// plane.
    pub blob_store: Arc<dyn BlobStore>,
    /// In-memory LRU cache in front of `blob_store`.
    pub blob_cache: blob_cache::BlobCache,
    /// On-disk LRU cache underneath `blob_cache`. Large blobs that
    /// do not fit in memory land here, and `serve_static_hit` mmaps
    /// them on serve so the userspace → kernel copy goes away.
    pub disk_cache: blob_cache::DiskBlobCache,
    /// KV-backed dedupe table for idempotent RPC mutations. The
    /// gateway consults this before forwarding `idempotent: true`
    /// mutations to the worker; on a hit it returns the stored
    /// response without touching V8.
    pub idempotency_store: Arc<dyn idempotency::IdempotencyStore>,
    /// OIDC relying-party engine for hosted creator apps. Drives the
    /// per-origin authorize-redirect → callback → session-mint flow
    /// (proposal §2.2 + §10.2). One RP instance services every
    /// `{app}.zeroship.ai` host — the per-app `redirect_uri` is the
    /// only thing that changes per request.
    pub oidc_rp: Arc<oidc_rp::OidcRp>,
    /// Postgres client used by the gateway's per-origin session store
    /// (`auth.gateway_sessions`). `Option` because the binary supports
    /// a dev "no-DB" mode (when `--db` is empty); test fixtures also
    /// rely on `None` to construct `GateState` without a live PG.
    pub db: Option<Arc<compio_postgres::Client>>,
    /// Tiered replay cache for `DPoP` proof `jti` claims (RFC 9449
    /// §11.1). The local tier rejects hot repeats without a DB round-trip;
    /// the PG tier rejects replays that land on a sibling gateway process.
    pub dpop_jti_cache: Arc<zeroship_core::dpop::TieredJtiCache>,
    /// In-process replay cache for OIDC Back-Channel Logout
    /// `logout_token.jti` claims. Replays are answered with 200 for
    /// webhook idempotency but do not run session revocation again.
    pub logout_jti_cache: Arc<zeroship_core::logout_token::LogoutJtiCache>,
    /// Gateway-issued wrapper-token signing key (Phase 8 U1). Loaded
    /// from a PKCS#8 PEM/DER file at boot via `--signing-key-file`.
    /// `None` when the operator runs without the flag — DPoP-exchange
    /// endpoints return 503 in that mode, but every other gateway path
    /// keeps working.
    pub signing_key: Option<Arc<ed25519_dalek::SigningKey>>,
    /// Wrapper-token issuer (Phase 8 U3). Materialised at boot from
    /// `signing_key` + `config.public_url`; `None` exactly when
    /// `signing_key` is `None`. The `/__zs/auth/dpop-exchange` handler
    /// short-circuits to 503 when this is absent so the rest of the
    /// gateway can keep serving traffic during the `DPoP` rollout.
    pub wrapper_issuer: Option<Arc<wrapper_token::Issuer>>,
    /// Wrapper-token verifier (Phase 8 U4). Built in lockstep with
    /// `wrapper_issuer` from the public half of the same signing key.
    /// The dispatch path (`router::auth::resolve_dpop_user_header`)
    /// consults this BEFORE falling back to hydra introspection — a
    /// `DPoP` request whose access token verifies as a wrapper gets
    /// the strict `cnf.jkt ↔ proof jkt` binding check; a request whose
    /// token is a raw hydra opaque token falls through to the P7-U5
    /// introspection path (no binding). `None` means "wrapper-token
    /// verification is disabled" — every DPoP request falls through
    /// to introspection.
    pub wrapper_verifier: Option<Arc<wrapper_token::Verifier>>,
}
