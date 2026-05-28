//! zeroship-control — library crate.
//!
//! This lib exists so integration tests under `tests/` can reach the
//! registry + env store + handler types. The `zeroship-control` binary
//! (`src/main.rs`) is a thin wrapper around these modules.

pub mod api;
pub mod audit;
pub mod backchannel_logout;
pub mod console_sessions;
pub mod deploy;
pub mod env_handlers;
pub mod env_store;
pub mod http_util;
pub mod internal;
pub mod metering;
pub mod oidc_rp;
pub mod rate_limit;
pub mod registry;
pub mod stripe_handlers;
pub mod stripe_store;

use std::sync::Arc;

use zeroize::Zeroizing;
use zeroship_bundle::{BlobStore, BundleStore};

pub use env_store::EnvStore;
pub use rate_limit::{Quota, RateLimiter};
pub use registry::Registry;
pub use stripe_store::StripeStore;

/// String that zeroizes its heap buffer on drop AND refuses to leak
/// via `Display` / `Debug` / `serde::Serialize`. Use for any secret
/// that lives in `AppState` or any long-lived struct.
///
/// Designed to be a footgun-resistant replacement for the previous
/// `pub type SecretString = Zeroizing<String>` alias — that alias
/// still exposed `Display`, `to_string()`, and (via Deref) all
/// `String` methods, which made it trivial for a tracing span or
/// JSON serializer to leak the secret. This wrapping struct exposes
/// ONLY `expose_secret()` for intentional access; every accidental
/// path errors at compile time.
pub struct SecretString(Zeroizing<String>);

impl SecretString {
    pub fn new(s: String) -> Self { Self(Zeroizing::new(s)) }

    /// Borrow the underlying string. Name is intentionally noisy —
    /// every call site documents that the caller knows it's holding
    /// secret material.
    pub fn expose_secret(&self) -> &str { &self.0 }

    /// `true` for empty / unset secret. Lets callers gate on
    /// "is this configured" without exposing the value.
    pub fn is_empty(&self) -> bool { self.0.is_empty() }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecretString(<redacted {} bytes>)", self.0.len())
    }
}
// Intentionally NO Display, NO serde::Serialize, NO Deref<Target=String>.
// The only way to read the contents is `.expose_secret()`.

/// Shared application state injected into every handler.
///
/// Sensitive secrets are wrapped in `SecretString` (Zeroizing<String>)
/// so they don't linger in heap for /proc/<pid>/mem or core-dump reads
/// after the process exits or panics.
#[allow(missing_debug_implementations)]
pub struct AppState {
    pub registry: Registry,
    pub env_store: EnvStore,
    pub stripe_store: StripeStore,
    pub vfs: Arc<dyn BundleStore + Send + Sync>,
    /// Content-addressed blob store. Backs `.zship` ingestion. The
    /// gateway reads asset bytes from its own `BlobStore` instance,
    /// so no asset-serving HTTP shim lives here.
    pub blob_store: Arc<dyn BlobStore>,
    pub control_key: SecretString,
    pub master_key: SecretString,
    /// Stripe webhook signing secret. Required in prod; empty +
    /// `insecure_dev=true` skips verification.
    pub stripe_webhook_secret: SecretString,
    /// Worker HTTP base URLs used for admin log fan-out.
    pub worker_urls: Vec<String>,
    /// Shared secret for worker admin endpoints. Empty means dev-only
    /// unauthenticated workers, matching `zeroship-worker`.
    pub worker_key: SecretString,
    /// Per-IP rate limiter for mutating admin endpoints. Burst 30,
    /// 60/min steady — generous for honest tooling, fatal for loops.
    pub admin_limiter: Arc<RateLimiter>,
    /// Per-IP rate limiter for the unauthenticated webhook endpoint.
    /// Burst 50, 600/min — Stripe's healthy rate is ~1/sec; the
    /// burst cushion handles bulk replays.
    pub webhook_limiter: Arc<RateLimiter>,
    /// Set to `true` by the `--dev-insecure` CLI flag (or
    /// `ZEROSHIP_DEV_INSECURE=1` env var). ONLY permits empty admin /
    /// control / webhook secrets when explicitly opted in. Production
    /// must leave this false; the startup guard in `main.rs` refuses
    /// to boot with missing secrets otherwise.
    pub insecure_dev: bool,
    /// Set via `--trust-proxy` (or `TRUST_PROXY=1`). When `false`
    /// (default), `X-Forwarded-For` is ignored — peer_addr is the
    /// only source-IP signal. Set to `true` ONLY when the control
    /// plane is bound behind a trusted load balancer that overwrites
    /// XFF; otherwise an attacker with direct network reach can spoof
    /// audit log IPs and rate-limit buckets.
    pub trust_proxy: bool,
    /// Directory where in-flight `.zship` deploy bodies are streamed
    /// before mmap+ingest. Defaults to `std::env::temp_dir()`. Operators
    /// may pin it to a fast local disk (`--deploy-tmp-dir`) so deploy
    /// throughput isn't bottlenecked by `/tmp` space or filesystem
    /// type. Files are unlinked immediately after ingest (success or
    /// failure).
    pub deploy_tmp_dir: std::path::PathBuf,
    /// OIDC relying-party for `console.zeroship.ai`. Drives the
    /// authorize-redirect → callback → session-mint flow on the
    /// creator dashboard (proposal §2.3). Mandatory now that U8 has
    /// retired the legacy `auth_service` / `auth_handlers` chain —
    /// the OIDC RP is the only console-auth surface.
    pub oidc_rp: Arc<oidc_rp::ConsoleOidcRp>,
    /// Postgres client pointed at the `auth` schema, used by
    /// `console_sessions::{create,validate,revoke}`. Distinct from the
    /// `registry` PG client (which talks to the control schema)
    /// because in multi-DB deployments the auth tables may live in a
    /// separate cluster. Mandatory post-U8.
    pub auth_pg: Arc<compio_postgres::Client>,
    /// In-process replay cache for OIDC Back-Channel Logout
    /// `logout_token.jti` claims. Replays are answered with 200 for
    /// webhook idempotency but do not run session revocation again.
    pub logout_jti_cache: Arc<zeroship_core::logout_token::LogoutJtiCache>,
}
