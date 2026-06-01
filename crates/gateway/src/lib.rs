//! `zeroship-gateway` — library crate.
//!
//! The gateway binary (`main.rs`) is a thin entry point: it parses
//! flags, constructs [`GateState`], registers ntex routes, and starts
//! the server. Everything else — routing, auth, proxy, blob cache,
//! sessions, signing, session-cookie issuance — lives in this library
//! so integration tests under `tests/` can drive the handlers without
//! reaching through the binary.
//!
//! [`GateConfig`] and [`GateState`] live here (not in `main.rs`) for
//! the same reason: tests construct fixtures directly.

pub mod anchors;
pub mod auth;
pub mod auth_token;
pub mod backchannel_logout;
pub mod blob_cache;
pub mod browser_auth;
pub mod compiled;
pub mod db;
pub mod dispatch;
pub mod enforce;
pub mod error;
pub mod hydra_client;
pub mod identities;
pub mod idempotency;
pub mod oidc_rp;
pub mod proxy;
pub mod rls;
pub mod router;
pub mod session_token;
pub mod sessions;
pub mod signing;
pub mod sync;

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
    /// Shared secret between gateway and workers. Used to bearer-auth the
    /// `/dispatch` endpoints and HMAC-sign the `ZeroShip-User` header so
    /// workers can verify forwarded identity was not forged by an attacker
    /// with direct network access. Empty disables both checks (dev only).
    pub worker_key: String,
    /// Upstream URL for Ory Hydra's public OIDC endpoints. The gateway
    /// forwards `auth.zeroship.ai/{oauth2,.well-known}/*` (plus
    /// `/userinfo`) here. Compose-internal default points at the
    /// `hydra` service on its 4444 port.
    pub hydra_public_url: String,
    /// Upstream URL for `crates/auth` — the login/signup UI, OAuth2
    /// consent handlers, and webhook surfaces. Everything on the
    /// `auth.zeroship.ai` host that is NOT an OIDC protocol endpoint
    /// is forwarded here. Defaults to the compose-internal `auth`
    /// service; will be wired through once the service joins compose
    /// (Phase 3 Unit U11).
    pub auth_ui_url: String,
    /// Dev-only flag. When true the gateway emits cookies without the
    /// `Secure` attribute so the localhost HTTP flow works in `pnpm dev`
    /// / docker-compose. Production MUST set this to false — the
    /// `__Host-` cookie prefix RFC 6265bis §4.1.3 requires `Secure`.
    pub insecure_dev: bool,
    /// Opt-in proxy header trust for client IP derivation. When false
    /// (default), per-IP rate limits and subscription affinity ignore
    /// `Forwarded` / `X-Forwarded-For` / similar headers and use only
    /// the peer socket address. Set true only behind a trusted L7 proxy.
    pub trust_proxy: bool,
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
    /// its per-compio-worker pool (`crates/sandbox/src/db.rs`). Handlers
    /// reach a checked-out connection via
    /// [`db::checkout`](crate::db::checkout); each checkout covers ONE
    /// operation and releases on drop, so no single shared connection
    /// serializes gateway DB work.
    ///
    /// `Option` because the binary supports a dev "no-DB" mode (when
    /// `--db` is empty); test fixtures also rely on `None` to construct
    /// `GateState` without a live PG.
    pub db: Option<db::DbConfig>,
    /// Tiered replay cache for `DPoP` proof `jti` claims (RFC 9449
    /// §11.1). The local tier rejects hot repeats without a DB round-trip;
    /// the PG tier rejects replays that land on a sibling gateway process.
    pub dpop_jti_cache: Arc<zeroship_core::dpop::TieredJtiCache>,
    /// In-process replay cache for OIDC Back-Channel Logout
    /// `logout_token.jti` claims. Replays are answered with 200 for
    /// webhook idempotency but do not run session revocation again.
    pub logout_jti_cache: Arc<zeroship_core::logout_token::LogoutJtiCache>,
    /// Short-TTL read-through cache for the per-app family-marker revocation
    /// read (BFF reshape R1d). The cookie / Bearer / DPoP-introspect arms each
    /// run one per-request `is_family_revoked_since` DB read after the local
    /// token verify; this cache makes the steady-state (no-revocation) hit
    /// fully DB-free. It stores the family's latest `revoked_after`
    /// (`Option<i64>` epoch seconds, `None` = no marker — negative caching is
    /// mandatory) and the hot path re-judges `> iat` LOCALLY per request, so
    /// one entry serves every cookie in the family. A cross-node revocation is
    /// honored within `<= REVOCATION_CACHE_TTL_SECS` on a cache-warm node; a
    /// SAME-NODE writer (`/signout`, back-channel logout) busts the entry
    /// immediately via `invalidate`. Fail-closed is preserved: a cache MISS
    /// followed by a DB error rejects, exactly as the un-cached read did.
    pub revocation_cache: Arc<zeroship_core::wrapper_revocation::RevocationCache>,
    /// Gateway session-cookie signing key. Loaded from a PKCS#8 PEM/DER file
    /// at boot via `--signing-key-file`. `None` when the operator runs without
    /// the flag — the signed session cookie cannot be issued/verified, so the
    /// cookie auth arm fails closed, but every other gateway path keeps working.
    pub signing_key: Option<Arc<ed25519_dalek::SigningKey>>,
    /// PREVIOUS session-cookie signing key (auth-sdk Slice 1b-browser,
    /// rotation overlap §8.5). Loaded from `--prev-signing-key-file` /
    /// `GATEWAY_PREV_SIGNING_KEY_FILE` when an operator is mid-roll. `Some`
    /// only during the overlap window; `None` in steady state. The Issuer
    /// NEVER signs with this — it is for `session_token::Verifier::with_previous`
    /// (so a session cookie minted just before the roll still verifies).
    pub prev_signing_key: Option<Arc<ed25519_dalek::SigningKey>>,
    /// Signed-session-cookie issuer (BFF redesign slice R1b). Mints the
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
    /// `zeroship.app_session_anchors.refresh_token_enc` (auth-sdk Slice
    /// 1b-anchors, §8.1/§8.5). A `[u8; 32]` (so `Send + Sync`, unlike the
    /// `!Send` pool / single-flight), derived once at boot from a stable
    /// server secret via `zeroship_core::crypto::derive_key`. The refresh
    /// family never leaves the gateway in plaintext — neither to the
    /// browser nor at rest in PG.
    pub anchor_enc_key: [u8; 32],
    /// Platform-wide pairwise salt for the per-app `pws_…` subject
    /// projection (auth-sdk §6.2, F4-B). The browser-held wrapper's `sub`
    /// is `derive_pairwise(global_user_id, route.sector_identifier)` keyed
    /// on THIS salt, so app JS decoding its own access token reads a
    /// per-app pseudonym, never the global user UUID (G4). A `[u8; 32]`
    /// (Send + Sync), derived once at boot from a stable server secret via
    /// `zeroship_core::crypto::derive_key`. Rotating it rotates every app's
    /// subjects (a deliberate break-glass).
    pub pairwise_salt: [u8; 32],
    /// First-party OAuth clients that skip Hydra consent (e.g. the builder).
    /// Resolved at boot from the shared `[auth].trusted_oauth_clients` file
    /// overlay via `zeroship_core::auth::resolve_trusted_oauth_clients`,
    /// falling back to the compiled default set when the key is absent. A
    /// later phase gates trusted-client behaviour on membership in this set;
    /// the gateway shares the same byte-identical resolution as the control
    /// plane so the two services never disagree on which clients are trusted.
    pub trusted_oauth_clients: std::collections::HashSet<String>,
}
