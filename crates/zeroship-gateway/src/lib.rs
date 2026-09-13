//! `zeroship-gateway` — library crate.
//!
//! The gateway binary (`main.rs`) is a thin entry point: it parses
//! flags, constructs [`GateState`], registers ntex routes, and starts
//! the server. Everything else — routing, auth, proxy, blob cache,
//! sessions, signing, session-cookie issuance — lives in this library

// `target_session_attrs` grew from three variants to six and each connect path
// now carries a recovery probe, which widened the async chain reaching this
// crate past rustc's default layout-query depth of 128. Structural fixes were
// tried first and do not help: the depth is cumulative across the whole
// pool -> connect -> handshake chain, so boxing any single future removes one
// level, not the ~130 reported. `crates/zeroship-data-v8/src/lib.rs` and
// `crates/zeroship-gateway/src/main.rs` already carry this for the same reason. It is a
// compiler resource limit, not a correctness guard.
#![recursion_limit = "256"]
//! so integration tests under `tests/` can drive the handlers without
//! reaching through the binary.
//!
//! [`GateConfig`] and [`GateState`] live here (not in `main.rs`) for
//! the same reason: tests construct fixtures directly.

pub mod anchors;
pub mod auth_token;
pub mod backchannel_logout;
pub mod blob_cache;
pub mod browser_auth;
pub mod config;
pub mod db;
pub mod dispatch;
pub mod enforce;
pub mod error;
pub mod health;
pub mod op_client;
pub mod identities;
pub mod idempotency;
pub mod oidc_rp;
pub mod proxy;
pub mod rls;
pub mod router;
pub mod session_token;
pub mod sessions;
pub mod signing;
pub mod signal_ingress;
pub mod sync;

use std::sync::Arc;

use zeroship_bundle::BlobStore;
use zeroship_core::config::{OriginScheme, TrustedOrigin};

/// Boot-time configuration for the gateway. Populated from CLI flags
/// or environment variables in `main.rs`.
#[allow(missing_debug_implementations)]
pub struct GateConfig {
    pub control_url: String,
    pub control_key: String,
    pub worker_urls: Vec<String>,
    pub poll_interval_secs: u64,
    /// Upstream URL for `crates/auth` — the self-contained OP
    /// (`/oauth2/*`, `/oauth2/.well-known/*`),
    /// login/signup UI, OAuth2 consent handlers, and webhook surfaces.
    pub auth_ui_url: String,
    /// Scheme used in browser-visible app URLs and same-origin comparisons.
    /// This describes the public edge, not the gateway's backend transport.
    pub origin_scheme: OriginScheme,
    /// Additional exact origins accepted by the gateway's same-origin guards.
    /// Each app's own `{origin_scheme}://{request-host}` remains implicitly
    /// trusted so arbitrary creator-app subdomains do not need enumeration.
    pub trusted_origins: Vec<TrustedOrigin>,
    /// Opt-in proxy header trust for client IP derivation. When false
    /// (default), per-IP rate limits and subscription affinity ignore
    /// `Forwarded` / `X-Forwarded-For` / similar headers and use only
    /// the peer socket address. Set true only behind a trusted L7 proxy.
    pub trust_proxy: bool,
    /// Public URL the gateway advertises as its own `iss` in gateway-issued
    /// session tokens. In production this is
    /// `https://api.zeroship.ai`; in dev it can be left at the default.
    /// Verifiers pin `iss` to this string, so the value MUST be stable
    /// across gateway restarts.
    pub public_url: String,
}

/// How a request Origin matched the gateway's configured topology.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OriginMatch {
    /// The app's implicit `{origin_scheme}://{request-host}` origin.
    App,
    /// A distinct, exact entry in `trusted_origins`.
    Trusted,
}

impl OriginMatch {
    /// Return whether browser fetch metadata is consistent with this match.
    #[must_use]
    pub(crate) fn accepts_sec_fetch_site(self, value: &str) -> bool {
        match self {
            Self::App => value == "same-origin",
            Self::Trusted => matches!(value, "same-origin" | "same-site" | "cross-site"),
        }
    }
}

impl GateState {
    /// Mint the `Authorization` header value this gateway presents to a worker.
    ///
    /// The TRANSPORT-ONLY profile, and the choice is by RATE: this hop carries
    /// every end-user request, so a single-use `jti` here would put a write
    /// against a table shared by every worker replica on the app data path.
    /// What bounds replay on this hop instead is the identity envelope's own
    /// binding to the dispatch request id and its issuance window.
    ///
    /// `None` when no service key material is configured. The worker then sees
    /// no credential and refuses, which is the whole difference from the shared
    /// secret this replaces: absence used to mean "skip the check".
    #[must_use]
    pub fn worker_authorization(&self) -> Option<String> {
        let worker = zeroship_core::service_peers::service_issuer(
            zeroship_core::service_peers::WORKER_SERVICE_NAME,
        )
        .ok()?;
        self.service_auth.authorization_for(&worker)
    }
}

impl GateConfig {
    /// Classify an exact Origin match, preferring the implicit app origin when
    /// an operator redundantly lists it in `trusted_origins`.
    #[must_use]
    pub(crate) fn classify_origin(&self, origin: &str, host: &str) -> Option<OriginMatch> {
        if origin == format!("{}://{host}", self.origin_scheme) {
            Some(OriginMatch::App)
        } else if self
            .trusted_origins
            .iter()
            .any(|trusted| trusted.as_str() == origin)
        {
            Some(OriginMatch::Trusted)
        } else {
            None
        }
    }
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
    /// Postgres connection-**pool** handle for the gateway's per-origin
    /// session store (`zeroship.gateway_sessions`) and the anchor/revocation
    /// read/write paths.
    ///
    /// The compio-postgres [`Pool`](compio_postgres::Pool) is `!Send`
    /// (single-threaded, `Rc`/`Cell` internals), so it cannot live in the
    /// `Arc<GateState>` shared across ntex's worker arbiter threads.
    /// Instead `GateState.db` carries only the connection *parameters*
    /// (DSN + max size, both `Send + Sync`); the actual `Pool` is built
    /// lazily **per worker thread**, wrapped in an `Rc`, and stashed in a
    /// thread-local — the same pattern the sandbox controller uses for
    /// its per-compio-worker pool. Handlers
    /// reach a checked-out connection via
    /// [`db::checkout`](crate::db::checkout); each checkout covers ONE
    /// operation and releases on drop, so no single shared connection
    /// serializes gateway DB work.
    ///
    /// `Option` because the binary supports a dev "no-DB" mode (when
    /// `--db` is empty); test fixtures also rely on `None` to construct
    /// `GateState` without a live PG.
    pub db: Option<db::DbConfig>,
    /// In-process replay cache for OIDC Back-Channel Logout
    /// `logout_token.jti` claims. Replays are answered with 200 for
    /// webhook idempotency but do not run session revocation again.
    pub logout_jti_cache: Arc<zeroship_core::logout_token::LogoutJtiCache>,
    /// Short-TTL read-through cache for the per-app family-marker revocation
    /// read. The cookie and Bearer arms consult it after local token
    /// verification. A miss performs one `is_family_revoked_since` DB read;
    /// fresh hits are DB-free. It stores the family's latest `revoked_after`
    /// (`Option<i64>` epoch seconds, `None` = no marker — negative caching is
    /// mandatory) and the hot path re-judges `> iat` LOCALLY per request, so
    /// one entry serves every cookie in the family. A cross-node revocation is
    /// honored within `<= REVOCATION_CACHE_TTL_SECS` on a cache-warm node; a
    /// SAME-NODE writer (`/signout`, back-channel logout) busts the entry
    /// immediately via `invalidate`. Fail-closed is preserved: a cache MISS
    /// followed by a DB error rejects, exactly as the un-cached read did.
    pub revocation_cache: Arc<zeroship_authz::wrapper_revocation::RevocationCache>,
    /// Gateway session-cookie signing key. Loaded from a PKCS#8 PEM/DER file
    /// at boot via `--signing-key-file`. `None` when the operator runs without
    /// the flag — the signed session cookie cannot be issued/verified, so the
    /// cookie auth arm fails closed, but every other gateway path keeps working.
    pub signing_key: Option<Arc<ed25519_dalek::SigningKey>>,
    /// Previous session-cookie signing key for rotation overlap. Loaded from
    /// `--prev-signing-key-file` /
    /// `GATEWAY_PREV_SIGNING_KEY_FILE` when an operator is mid-roll. `Some`
    /// only during the overlap window; `None` in steady state. The Issuer
    /// NEVER signs with this — it is for `session_token::Verifier::with_previous`
    /// (so a session cookie minted just before the roll still verifies).
    pub prev_signing_key: Option<Arc<ed25519_dalek::SigningKey>>,
    /// Signed-session-cookie issuer. Mints the
    /// gateway-signed `zeroship-sess+jwt` written into `__Host-zeroship_app_session`,
    /// stamping the `zeroship-sess+jwt` typ. `None` exactly when `signing_key` is
    /// `None` (no signing key ⇒ no signed cookie ⇒ the cookie arm fails closed).
    pub session_issuer: Option<Arc<session_token::Issuer>>,
    /// Signed-session-cookie verifier (BFF redesign slice R1b). The cookie arm
    /// (`router::auth::resolve_app_session_user_header_inner`) verifies the
    /// `__Host-zeroship_app_session` token with this LOCALLY on every request — no
    /// per-request DB/session-store read. Built in lockstep with
    /// `session_issuer` from the same key, with the previous key folded in via
    /// `Verifier::with_previous` during a rotation overlap.
    pub session_verifier: Option<Arc<session_token::Verifier>>,
    /// AES-256-GCM key encrypting the server-held refresh family at rest in
    /// `zeroship.app_session_anchors.refresh_token_enc`. A `[u8; 32]` (so
    /// `Send + Sync`, unlike the
    /// `!Send` pool / single-flight), derived once at boot from a stable
    /// server secret via `zeroship_core::crypto::derive_key`. The refresh
    /// family never leaves the gateway in plaintext — neither to the
    /// browser nor at rest in PG.
    pub anchor_enc_key: [u8; 32],
    /// Platform-wide pairwise salt for the per-app `pws_` subject projection.
    /// The gateway derives browser-session subjects; auth uses the same salt
    /// for access tokens. A `[u8; 32]` (`Send + Sync`),
    /// derived once at boot from a stable server secret via
    /// `zeroship_core::crypto::derive_key`. Rotating it rotates every app's
    /// subjects (a deliberate break-glass).
    pub pairwise_salt: [u8; 32],
    /// Process-wide usage meter — the gateway is a SECOND metering producer
    /// (metering coverage #27). It emits `gateway_egress_bytes` for the
    /// bodies the worker never sees (static assets, redirects, gateway error
    /// pages) attributed to the route's server-resolved `app_id`. The
    /// worker keeps owning `egress_bytes` for its proxied response bodies,
    /// so the two metrics are DISJOINT BY CONSTRUCTION — the gateway never
    /// meters a worker-proxy body, never increments `egress_bytes`. A single
    /// `spawn_flush_task` (wired in `main.rs`) drains this and POSTs to the
    /// same control `/internal/usage` ingest under a restart-unique
    /// `gate-<base>-<nonce>` producer id. Recording is a cheap counter bump
    /// on the response path (an `RwLock` read + a per-app `Mutex` for the
    /// custom metric — uncontended, not literally lock-free); the flush is a
    /// detached background task, so the proxy hot path is not slowed.
    /// This process's service identity: its own ed25519 key for the calls it
    /// MAKES, and the peer bundle plus replay store for the calls it RECEIVES.
    ///
    /// DISTINCT from `signing_key`, which signs an END-USER session cookie. One
    /// key doing both jobs is the shape this change removes.
    ///
    /// Two edges use it, at two profiles. The inbound workflow-advance handler
    /// verifies control under the FULL profile, because that edge fires per
    /// advance. The outbound dispatch hop MINTS under the transport-only
    /// profile, because that hop is the app data path and a single-use claim
    /// there would be a shared-store write per end-user request.
    ///
    /// It also holds the gateway's signer for the `ZeroShip-User` identity
    /// envelope, which is the same ed25519 key under a second use: what the
    /// gateway asserts about a SERVICE (itself) and what it asserts about an
    /// END USER both carry its signature, and the worker checks both under the
    /// one published public half.
    ///
    /// `ServiceAuth::unconfigured()` when no key material was configured; that
    /// state refuses the inbound edge, mints nothing outbound, and signs no
    /// identity - so an unconfigured gateway forwards no user rather than
    /// forwarding one nothing can check.
    pub service_auth: Arc<zeroship_core::service_peers::ServiceAuth>,
    pub meter: Arc<zeroship_metering::Meter>,
}

#[cfg(test)]
mod tests;
