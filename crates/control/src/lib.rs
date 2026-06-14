//! zeroship-control — library crate.
//!
//! This lib exists so integration tests under `tests/` can reach the
//! registry + env store + handler types. The `zeroship-control` binary
//! (`src/main.rs`) is a thin wrapper around these modules.

pub mod api;
pub mod admin_handlers;
pub mod app_oauth_client;
pub mod audit;
pub mod auth_audit;
pub mod authz_guard;
pub mod bootstrap_builder;
pub mod bootstrap_console;
pub mod cron;
pub mod deploy;
pub mod env_handlers;
pub mod env_store;
pub mod http_util;
pub mod internal;
pub mod metering;
pub mod oauth_grants_handlers;
pub mod oauth_handlers;
pub mod plan_catalog;
pub mod pricing;
pub mod pricing_store;
pub mod rate_limit;
pub mod registry;
pub mod spend;
pub mod stripe_client;
pub mod stripe_handlers;
pub mod stripe_store;
pub mod token_handlers;

use std::collections::HashSet;
use std::sync::Arc;

use zeroize::Zeroizing;
use zeroship_bundle::BlobStore;

pub use env_store::EnvStore;
pub use rate_limit::{Quota, RateLimiter};
pub use registry::Registry;
pub use stripe_store::StripeStore;

// Trusted first-party OAuth client resolution lives in `zeroship-core` so it
// can be shared without a control → core cycle. Re-exported here so control's
// existing call sites and integration tests keep their `zeroship_control::…`
// import paths.
pub use zeroship_core::auth::trusted_clients::{
    default_trusted_oauth_clients, resolve_trusted_oauth_clients,
};

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
    /// Content-addressed blob store. The SOLE deploy-artifact store: backs
    /// `.zship` ingestion (blobs + manifests) and is the SAME store the
    /// gateway and worker read. App purge deletes the per-app manifest
    /// keyspace via `BlobStore::delete_app_manifests`.
    pub blob_store: Arc<dyn BlobStore>,
    pub control_key: SecretString,
    pub master_key: SecretString,
    /// Stripe webhook signing secret. Required in prod; empty +
    /// `insecure_dev=true` skips verification.
    pub stripe_webhook_secret: SecretString,
    /// Stripe secret API key (`sk_…`) for OUTBOUND calls (the billing PR6
    /// reconciler + `billing/setup`). Required in prod; empty allowed only
    /// under `insecure_dev`. Never logged — `SecretString`.
    pub stripe_secret_key: SecretString,
    /// Stripe REST API base URL the outbound client targets. Defaults to
    /// `https://api.stripe.com`; the integration tests override it to point the
    /// REAL `cyper` client at a localhost mock-Stripe server.
    pub stripe_base_url: String,
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
    /// Shared long-lived Postgres client on the SINGLE physical `zeroship`
    /// database — the same DB the `registry` opens per-query connections on.
    /// Used by the `AuthzGuard` bearer path (`zeroship.permission_tokens`
    /// lookup + Cedar enforcement against `zeroship.*`), the audit emitter,
    /// and the connected-app OAuth grant handlers.
    ///
    /// There is no separate auth database any more: control's former
    /// `--auth-db` was only ever a config capability (compose always pointed
    /// it at the same DB), and every system table lives in the one `zeroship`
    /// schema. Handlers that need a transaction-capable owned connection open a
    /// fresh one via `registry.conn()` (mutable `&mut self`); `control_pg` is
    /// the pipelined shared handle for autocommit reads/writes.
    ///
    /// The console is a regular gateway-fronted app authenticated via
    /// `@zeroship/auth` (BFF); the control plane is a pure API resource
    /// server with NO OIDC RP of its own — the bespoke `ConsoleOidcRp` +
    /// `console_sessions` surface was removed in the R5 cutover.
    pub control_pg: Arc<compio_postgres::Client>,
    /// Hydra admin API base URL. Control uses this for admin-owned OAuth
    /// client registration/deletion; Hydra remains the source of truth for
    /// generated client secrets.
    pub hydra_admin_url: String,
    /// Apex domain hosted creator apps serve under, e.g. `zeroship.ai`
    /// (prod) or `zeroship.localhost` (dev). An app named `myapp` serves
    /// at `myapp.{app_base_domain}`; the per-app OAuth client's
    /// redirect_uris / sector_identifier are derived from that apex host
    /// (Slice 1d, spec §1.1).
    pub app_base_domain: String,
    /// OAuth client IDs that get `skip_consent=true` when registered.
    pub trusted_oauth_clients: HashSet<String>,
    /// Expected audience for OAuth access tokens accepted by the control
    /// plane's bearer-token introspection path.
    pub expected_oauth_audience: String,
    /// Static Cedar policy bundle for control-plane authorization.
    /// Parsed once at boot; per-token policies are loaded by the authz
    /// evaluator only when a token-bearing request needs them.
    pub static_policies: zeroship_authz::PolicySet,
    /// Ed25519 issuer/verifier for first-party Personal Access Tokens.
    /// Control uses the same key material as the gateway's
    /// `--signing-key-file` wrapper-token issuer for P9 v1.
    pub pat_issuer: Arc<token_handlers::PatIssuer>,
    /// Hydra admin introspection client for third-party OAuth bearer
    /// access tokens. Used only after local PAT verification fails.
    pub hydra_introspector: Arc<zeroship_core::hydra::HydraIntrospector>,
    /// In-process replay cache for OIDC Back-Channel Logout
    /// `logout_token.jti` claims. Replays are answered with 200 for
    /// webhook idempotency but do not run session revocation again.
    pub logout_jti_cache: Arc<zeroship_core::logout_token::LogoutJtiCache>,
    /// The configured metering/billing provider, built once at boot
    /// (`--metering-provider`, default `native`). The billing-reconcile cron
    /// drives it; `spend.rs`/`enforce.rs` NEVER touch it (enforcement is the
    /// local ledger, provider-independent). M-Native: only `Native` is
    /// functional — it is the existing Stripe pipeline behind the
    /// [`metering::provider::MeteringProvider`] trait.
    pub metering_provider: Arc<dyn metering::provider::MeteringProvider>,
    /// Platform-wide pairwise salt (auth-sdk §6.2), derived from the SAME
    /// stash signing key the gateway uses via
    /// [`zeroship_core::auth::derive_pairwise_salt`]. Control needs it so a
    /// dashboard "disconnect app" (grant revoke) can derive the per-app
    /// `pws_…` subject and write the `auth.token_revocations` family marker on
    /// the SAME `(client_id, pws_)` key the gateway's wrapper / Bearer / DPoP
    /// arms read — killing the live access token, not just the relay alias
    /// (Batch A fix 4). MUST stay byte-identical to the gateway's salt.
    pub pairwise_salt: [u8; 32],
}

impl AppState {
    /// Return whether `client_id` is configured as a trusted OAuth client.
    #[must_use]
    pub fn is_trusted(&self, client_id: &str) -> bool {
        Self::is_trusted_client_id(&self.trusted_oauth_clients, client_id)
    }

    /// The URL scheme hosted apps serve under: `http` in dev-insecure,
    /// `https` otherwise. Drives per-app OAuth redirect_uri derivation.
    #[must_use]
    pub fn app_scheme(&self) -> &'static str {
        if self.insecure_dev {
            "http"
        } else {
            "https"
        }
    }

    /// The apex host an app named `name` serves at:
    /// `{name}.{app_base_domain}`. The per-app OAuth client's
    /// redirect_uris / sector_identifier anchor here (Slice 1d, §1.1).
    #[must_use]
    pub fn apex_host_for_app(&self, name: &str) -> String {
        format!("{name}.{}", self.app_base_domain)
    }

    /// Idempotently provision (or reconcile) the per-app public PKCE OAuth
    /// client for `app_id` named `name`, using the apex host derived from
    /// `app_base_domain`. Wraps [`app_oauth_client::ensure_app_client`] with
    /// a fresh control-DB connection + a `HydraAdmin` over `hydra_admin_url`.
    /// Slice 1d (spec §1.1). On success returns the per-app `client_id`
    /// (`oac_<base62-app-id>`).
    ///
    /// `declared_scopes` are the app's manifest `auth.scopes` (Slice 3,
    /// spec §5.1): validated + mirrored into both the Hydra client `scope`
    /// allowlist and `control.app_scope_defs` atomically. Pass `&[]` at app
    /// **create** (no manifest yet); the deploy path passes the deployed
    /// manifest's declared scopes.
    ///
    /// # Errors
    /// Surfaces the underlying [`app_oauth_client::AppOauthClientError`] as a
    /// string. Callers log + continue (provisioning is best-effort relative
    /// to the create/deploy response, but the route-sync invariant — every
    /// deployed app's `RouteEntry` carries `Some(oauth_client_id)` — is held
    /// by re-provisioning on deploy).
    pub async fn provision_app_oauth_client(
        &self,
        app_id: &uuid::Uuid,
        name: &str,
        declared_scopes: &[zeroship_bundle::ScopeDef],
    ) -> Result<String, String> {
        let scheme = self.app_scheme();
        let apex = self.apex_host_for_app(name);
        let hosts = vec![apex];
        let hydra = zeroship_auth::hydra_client::HydraAdmin::new(self.hydra_admin_url.clone());
        let mut conn = self
            .registry
            .conn()
            .await
            .map_err(|e| format!("control db conn: {e}"))?;
        app_oauth_client::ensure_app_client(
            // Creator apps are NEVER first-party (spec §5.2): skip_consent=false.
            &mut conn, &hydra, app_id, name, scheme, &hosts, declared_scopes, false,
        )
        .await
        .map_err(|e| e.to_string())
    }

    /// Delete the per-app public PKCE OAuth client from Hydra for `app_id`
    /// (Slice 1d, spec §1.1: "on app delete → DELETE /admin/clients/<id>").
    /// Wraps [`app_oauth_client::delete_app_client`] over a `HydraAdmin` built
    /// from `hydra_admin_url`. The `control.oauth_clients` /
    /// `control.app_oauth_clients` DB rows cascade-delete with the
    /// `control.apps` row, so this only removes the Hydra-side registration.
    ///
    /// Hydra's `DELETE /admin/clients/{id}` is idempotent (404 → Ok), so a
    /// re-run or a never-provisioned app is a clean no-op.
    ///
    /// # Errors
    /// Surfaces the underlying [`app_oauth_client::AppOauthClientError`] as a
    /// string. Callers log + continue: a leaked Hydra client is best-effort
    /// GC, not a delete-blocking failure.
    pub async fn delete_app_oauth_client(&self, app_id: &uuid::Uuid) -> Result<(), String> {
        let hydra = zeroship_auth::hydra_client::HydraAdmin::new(self.hydra_admin_url.clone());
        app_oauth_client::delete_app_client(&hydra, app_id)
            .await
            .map_err(|e| e.to_string())
    }

    /// Return whether `client_id` is present in a trusted-client set.
    ///
    /// Thin delegate to [`zeroship_core::auth::trusted_clients::is_trusted_client_id`];
    /// kept as an associated fn so existing call sites and tests keep the
    /// `AppState::is_trusted_client_id(&set, id)` shape.
    #[must_use]
    pub fn is_trusted_client_id(
        trusted_oauth_clients: &HashSet<String>,
        client_id: &str,
    ) -> bool {
        zeroship_core::auth::trusted_clients::is_trusted_client_id(trusted_oauth_clients, client_id)
    }
}
