//! zeroship-control — library crate.
//!
//! This lib exists so integration tests under `tests/` can reach the
//! registry + env store + handler types. The `zeroship-control` binary
//! (`src/main.rs`) is a thin wrapper around these modules.

pub mod account_status;
pub mod api;
pub mod app_oauth_client;
pub mod audit;
pub mod auth_audit;
pub mod authz_guard;
pub mod billing_read;
pub mod bootstrap_builder;
pub mod config;
pub mod credit;
pub mod cron;
pub mod deploy;
pub mod deploy_inflight;
pub mod disputes;
pub mod device_handlers;
pub mod env_handlers;
pub mod env_store;
pub mod fee_policy;
pub mod http_util;
pub mod identity_bridge;
pub mod internal;
pub mod invoice_payments;
pub mod metering;
pub mod migrations_api;
pub mod egress_rules;
pub mod notify;
pub mod oauth_grants_handlers;
pub mod oauth_clients;
pub mod openmeter_client;
pub mod plan_catalog;
pub mod pricing;
pub mod pricing_store;
pub mod proration;
pub mod rate_limit;
pub mod refund;
pub mod registry;
pub mod spend;
pub mod stripe_client;
pub mod stripe_handlers;
pub mod stripe_store;
pub mod tax;
pub mod void_reissue;
pub mod workflow_instance_api;
pub(crate) mod workflow_limits;
pub(crate) mod workflow_rollout;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use zeroize::Zeroizing;
use zeroship_bundle::{BlobStore, WorkflowBlobStore};
use zeroship_stream::{StreamConfig, StreamError, StreamRegistry, StreamTransport};

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

pub fn platform_auth_provider(
    issuer: impl Into<String>,
    jwks_url: Option<String>,
) -> Arc<zeroship_core::auth_provider::AuthProvider> {
    let config = zeroship_core::auth_provider::PlatformConfig::new(issuer, jwks_url)
        .expect("valid platform auth provider config");
    Arc::new(zeroship_core::auth_provider::AuthProvider::platform(
        zeroship_core::auth_provider::PlatformProvider::new(config),
    ))
}

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

pub const DEFAULT_BILLING_FORWARDER_GROUP_ID: &str = "billing-forwarder";
pub const DEFAULT_SPEND_RECOMPUTE_GROUP_ID: &str = "spend-recompute-witness";
pub const DEFAULT_CONTROL_USAGE_PRODUCER_GROUP_ID: &str = "control-usage-producer";
pub const DEFAULT_CONTROL_USAGE_OUTBOX_WAL_PATH: &str =
    ".zeroship/usage-outbox-zeroship-control.redb";

#[derive(Default)]
struct ControlUsageOutboxState {
    outbox: Option<zeroship_metering::UsageOutbox>,
    flusher_started: bool,
}

#[derive(Clone)]
pub struct BillingStreamConfig {
    registry: Arc<StreamRegistry>,
    transport_id: String,
    base_config: StreamConfig,
    forwarder_group_id: String,
    recompute_group_id: String,
    topic: String,
    control_usage_outbox_wal_path: PathBuf,
    control_usage_outbox: Arc<Mutex<ControlUsageOutboxState>>,
}

impl std::fmt::Debug for BillingStreamConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BillingStreamConfig")
            .field("transport_id", &self.transport_id)
            .field("forwarder_group_id", &self.forwarder_group_id)
            .field("recompute_group_id", &self.recompute_group_id)
            .field(
                "control_usage_producer_group_id",
                &DEFAULT_CONTROL_USAGE_PRODUCER_GROUP_ID,
            )
            .field("topic", &self.topic)
            .finish_non_exhaustive()
    }
}

impl BillingStreamConfig {
    /// Build the billing stream config for THIS replica.
    ///
    /// Resolves the replica identity from `HOSTNAME` and delegates to
    /// [`Self::new_for_replica`]. See there for why the recompute group is
    /// per-replica and the forwarder group is not.
    ///
    /// The fallback when `HOSTNAME` is unset is a single fixed string, which is
    /// correct for the case that produces it (one control process on a box) and
    /// WRONG if someone runs two replicas on one host with no hostname set.
    /// That is a narrow gap and it is named here rather than papered over: the
    /// symptom would be the same partial-snapshot defect this split exists to
    /// close.
    pub fn new(
        registry: Arc<StreamRegistry>,
        transport_id: impl Into<String>,
        base_config: StreamConfig,
        forwarder_group_id: impl Into<String>,
        recompute_group_id: impl Into<String>,
    ) -> Result<Self, StreamError> {
        let replica = zeroship_core::declared_env!(external, "HOSTNAME", crate::config::ControlSettingsConsumer)
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "solo".to_string());
        Self::new_for_replica(
            registry,
            transport_id,
            base_config,
            forwarder_group_id,
            recompute_group_id,
            &replica,
        )
    }

    /// Build the config with an explicit replica identity.
    ///
    /// The two consumer groups here have OPPOSITE requirements, which is the
    /// whole reason this constructor exists:
    ///
    ///   * the FORWARDER is a work queue. Every usage event must be forwarded
    ///     exactly once, so all replicas share one group and Kafka splits the
    ///     partitions between them. A per-replica group would forward each
    ///     event once per replica.
    ///   * the RECOMPUTE is a witness. It rewinds, reads the COMPLETE retained
    ///     stream, and calls `replace_period_snapshot`, which DELETEs the
    ///     period and rewrites it. Two replicas in one group each receive a
    ///     subset of partitions, each computes a partial total, and each
    ///     overwrites the other. The month total silently lands below the
    ///     truth and spend enforcement reads that number.
    ///
    /// So the recompute group is suffixed per replica, making each replica the
    /// sole member of its own group and therefore the owner of every partition.
    ///
    /// Per REPLICA, not per BOOT: the recompute never commits an offset (it
    /// rewinds every cycle), so a fresh group each restart would buy nothing
    /// and leave abandoned group metadata behind for every process that ever
    /// ran.
    pub fn new_for_replica(
        registry: Arc<StreamRegistry>,
        transport_id: impl Into<String>,
        base_config: StreamConfig,
        forwarder_group_id: impl Into<String>,
        recompute_group_id: impl Into<String>,
        replica_id: &str,
    ) -> Result<Self, StreamError> {
        let forwarder_group_id = forwarder_group_id.into().trim().to_string();
        let replica_suffix: String = replica_id
            .trim()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let recompute_group_id = format!(
            "{}-{}",
            recompute_group_id.into().trim(),
            if replica_suffix.is_empty() {
                "solo"
            } else {
                replica_suffix.as_str()
            }
        );
        let topic = stream_topic(&base_config)?;
        let control_usage_outbox_wal_path = zeroship_core::declared_env!(
            platform,
            "CONTROL_USAGE_OUTBOX_WAL_PATH",
            crate::config::ControlSettingsConsumer
        )
        .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONTROL_USAGE_OUTBOX_WAL_PATH));
        let this = Self {
            registry,
            transport_id: transport_id.into(),
            base_config,
            forwarder_group_id,
            recompute_group_id,
            topic,
            control_usage_outbox_wal_path,
            control_usage_outbox: Arc::new(Mutex::new(ControlUsageOutboxState::default())),
        };
        this.validate()?;
        Ok(this)
    }

    #[must_use]
    pub fn transport_id(&self) -> &str {
        &self.transport_id
    }

    #[must_use]
    pub fn forwarder_group_id(&self) -> &str {
        &self.forwarder_group_id
    }

    #[must_use]
    pub fn recompute_group_id(&self) -> &str {
        &self.recompute_group_id
    }

    pub fn build_forwarder(&self) -> Result<Arc<dyn StreamTransport>, StreamError> {
        self.build_for_group(&self.forwarder_group_id)
    }

    pub fn build_recompute(&self) -> Result<Arc<dyn StreamTransport>, StreamError> {
        self.build_for_group(&self.recompute_group_id)
    }

    /// Override the control producer's durable WAL path. Tests should use a
    /// unique temporary path; multi-instance deployments should configure a
    /// stable per-instance path with `CONTROL_USAGE_OUTBOX_WAL_PATH`.
    #[must_use]
    pub fn with_control_usage_outbox_wal_path(mut self, path: impl AsRef<Path>) -> Self {
        self.control_usage_outbox_wal_path = path.as_ref().to_path_buf();
        self.control_usage_outbox = Arc::new(Mutex::new(ControlUsageOutboxState::default()));
        self
    }

    /// Open the control usage WAL and start its single background stream
    /// flusher. Calling this more than once is harmless; clones of this config
    /// share the same outbox and flusher guard.
    ///
    /// The first flush runs immediately so pending events from a previous
    /// process lifetime are retried at startup. Later flushes run at the common
    /// metering outbox interval.
    pub fn start_control_usage_outbox(&self) -> Result<(), String> {
        let outbox = self.control_usage_outbox()?;
        let should_start = {
            let mut state = self
                .control_usage_outbox
                .lock()
                .map_err(|_| "control usage outbox state lock poisoned".to_string())?;
            if state.flusher_started {
                false
            } else {
                state.flusher_started = true;
                true
            }
        };
        if !should_start {
            return Ok(());
        }

        compio::runtime::spawn(async move {
            loop {
                let result = outbox.publish_events(&[]).await;
                if result.failed.is_empty() {
                    if result.published > 0 {
                        tracing::debug!(
                            events = result.published,
                            topic = %outbox.topic(),
                            "control usage outbox published pending events"
                        );
                    }
                } else {
                    tracing::warn!(
                        attempted = result.attempted,
                        published = result.published,
                        failed = result.failed.len(),
                        topic = %outbox.topic(),
                        "control usage outbox flush completed with failures"
                    );
                }
                compio::time::sleep(zeroship_metering::DEFAULT_OUTBOX_INTERVAL).await;
            }
        })
        .detach();
        Ok(())
    }

    pub(crate) fn control_usage_outbox(&self) -> Result<zeroship_metering::UsageOutbox, String> {
        let mut state = self
            .control_usage_outbox
            .lock()
            .map_err(|_| "control usage outbox state lock poisoned".to_string())?;
        if let Some(outbox) = state.outbox.as_ref() {
            return Ok(outbox.clone());
        }

        let stream = self
            .build_for_group(DEFAULT_CONTROL_USAGE_PRODUCER_GROUP_ID)
            .map_err(|error| error.to_string())?;
        let outbox = zeroship_metering::UsageOutbox::new(
            stream,
            self.topic.clone(),
            &self.control_usage_outbox_wal_path,
        )
        .map_err(|error| error.to_string())?;
        state.outbox = Some(outbox.clone());
        Ok(outbox)
    }

    fn validate(&self) -> Result<(), StreamError> {
        require_stream_group("billing forwarder group", &self.forwarder_group_id)?;
        require_stream_group("spend recompute group", &self.recompute_group_id)?;
        require_stream_group(
            "control usage producer group",
            DEFAULT_CONTROL_USAGE_PRODUCER_GROUP_ID,
        )?;
        if self.forwarder_group_id == self.recompute_group_id
            || self.forwarder_group_id == DEFAULT_CONTROL_USAGE_PRODUCER_GROUP_ID
            || self.recompute_group_id == DEFAULT_CONTROL_USAGE_PRODUCER_GROUP_ID
        {
            return Err(StreamError::Config(
                "billing forwarder, spend recompute, and control usage producer stream groups must differ"
                    .to_string(),
            ));
        }
        Ok(())
    }

    fn build_for_group(&self, group_id: &str) -> Result<Arc<dyn StreamTransport>, StreamError> {
        let config = with_stream_group_id(&self.base_config, group_id)?;
        self.registry.build(&self.transport_id, &config)
    }
}

fn stream_topic(config: &StreamConfig) -> Result<String, StreamError> {
    let raw: serde_json::Value = config.parse()?;
    raw.as_object()
        .and_then(|object| object.get("topic"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|topic| !topic.is_empty())
        .map(str::to_string)
        .ok_or_else(|| StreamError::Config("billing usage stream topic is required".to_string()))
}

fn require_stream_group(name: &str, value: &str) -> Result<(), StreamError> {
    if value.trim().is_empty() {
        Err(StreamError::Config(format!("{name} is required")))
    } else {
        Ok(())
    }
}

fn with_stream_group_id(
    config: &StreamConfig,
    group_id: &str,
) -> Result<StreamConfig, StreamError> {
    config.map_object(|obj| {
        obj.remove("group_id");
        obj.insert(
            "group.id".to_string(),
            serde_json::Value::String(group_id.to_string()),
        );
    })
}

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
    /// Content-addressed workflow output store. This is intentionally separate
    /// from deploy bundle blobs so workflow-output GC can never delete deploy
    /// artifacts.
    pub workflow_blob_store: Arc<dyn WorkflowBlobStore>,
    pub control_key: SecretString,
    pub master_key: SecretString,
    /// Stripe webhook signing secret. An empty value makes every webhook fail
    /// closed; it never disables verification.
    pub stripe_webhook_secret: SecretString,
    /// Stripe secret API key (`sk_...`) for outbound reconciliation and
    /// `billing/setup` calls. Never logged (`SecretString`).
    pub stripe_secret_key: SecretString,
    /// Stripe REST API base URL the outbound client targets. Defaults to
    /// `https://api.stripe.com`; the integration tests override it to point the
    /// REAL `cyper` client at a localhost mock-Stripe server.
    pub stripe_base_url: String,
    /// Gateway internal base URL used by the workflow engine to dispatch
    /// claimed runs through the spend/account-gated edge before they reach a
    /// worker replay host.
    pub gateway_url: String,
    /// Migration-service base URL. `POST /api/apps/{id}/migrations/apply`
    /// forwards the creator's request here after authorizing it. Internal by
    /// construction: `migrated` holds the superuser provisioning DSN and binds
    /// loopback everywhere we ship it, so this hop is the only creator-reachable
    /// route to the migration service.
    pub migrated_url: String,
    /// Worker HTTP base URLs used for admin log fan-out.
    pub worker_urls: Vec<String>,
    /// Shared secret for worker admin endpoints.
    pub worker_key: SecretString,
    /// Per-IP rate limiter for mutating admin endpoints. Burst 30,
    /// 60/min steady — generous for honest tooling, fatal for loops.
    pub admin_limiter: Arc<RateLimiter>,
    /// Per-IP rate limiter for the unauthenticated webhook endpoint.
    /// Burst 50, 600/min — Stripe's healthy rate is ~1/sec; the
    /// burst cushion handles bulk replays.
    pub webhook_limiter: Arc<RateLimiter>,
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
    /// Used by the `AuthzGuard` bearer path (Cedar enforcement against
    /// `zeroship.*`), the audit emitter, and the connected-app OAuth grant
    /// handlers.
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
    /// Apex domain hosted creator apps serve under, e.g. `zeroship.ai`
    /// (prod) or `zeroship.localhost` (dev). An app named `myapp` serves
    /// at `myapp.{app_base_domain}`; the per-app OAuth client's
    /// redirect_uris / sector_identifier are derived from that apex host.
    pub app_base_domain: String,
    /// Scheme used in browser-visible app, auth, and console URLs. This is
    /// independent of backend transport and all security-relaxation flags.
    pub origin_scheme: zeroship_core::config::OriginScheme,
    /// OAuth client IDs that get `skip_consent=true` when registered.
    pub trusted_oauth_clients: HashSet<String>,
    /// Expected audience for OAuth access tokens accepted by the control
    /// plane's bearer-token introspection path.
    pub expected_oauth_audience: String,
    /// Static Cedar policy bundle for control-plane authorization.
    /// Parsed once at boot; per-token policies are loaded by the authz
    /// evaluator only when a token-bearing request needs them.
    pub static_policies: zeroship_authz::PolicySet,
    /// Platform auth-provider token verifier for OAuth bearer access tokens.
    /// The ONLY bearer path: control signs nothing of its own.
    pub auth_provider: Arc<zeroship_core::auth_provider::AuthProvider>,
    /// Provider factories available in this process. Boot registers built-ins
    /// explicitly, then builds the role-addressed billing stack below.
    pub provider_registry: Arc<metering::provider::ProviderRegistry>,
    /// Role-addressed billing stack: one provider for metering, one for rating,
    /// one for invoicing, plus webhook sinks.
    pub billing_stack: Arc<metering::provider::BillingStack>,
    /// Optional durable usage-event stream configuration. Cron builds separate
    /// role-scoped consumers from this spec so forwarder commits and recompute
    /// rewinds never share a consumer group.
    pub billing_stream: Option<BillingStreamConfig>,
    /// The configured tax provider, built once at boot (`--tax-provider`, default
    /// `native`). The billing-reconcile cron calls `compute_tax` at finalize and
    /// freezes the result into `invoices.tax_cents`. `Native` computes `0` (the
    /// USD launch owes no tax); enabling a real `StripeTaxProvider` later is a
    /// provider swap, not a schema change (`tax_cents` already exists). See
    /// [`tax::TaxProvider`].
    pub tax_provider: Arc<dyn tax::TaxProvider>,
    /// The billing notifier, built once at boot. The `cron::billing_notify` sweep
    /// renders a per-kind template and sends it through this seam (over the relocated
    /// `zeroship-mailer` `Mailer`), passing a provider-side `Idempotency-Key =
    /// (creator_id, kind, transition_id)` so a re-driven send is idempotent at the
    /// provider (billing-ops gap #26, PR-6, MAJOR-A). See [`notify::BillingNotifier`].
    pub notifier: Arc<dyn notify::BillingNotifier>,
    /// Platform-wide pairwise salt (auth-sdk §6.2), derived from the SAME
    /// stash signing key the gateway uses via
    /// [`zeroship_core::auth::derive_pairwise_salt`]. Control needs it so a
    /// dashboard "disconnect app" (grant revoke) can derive the per-app
    /// `pws_…` subject and write the `auth.token_revocations` family marker on
    /// the SAME `(client_id, pws_)` key the gateway's wrapper / Bearer / DPoP
    /// arms read — killing the live access token, not just the relay alias
    /// (Batch A fix 4). MUST stay byte-identical to the gateway's salt.
    pub pairwise_salt: [u8; 32],
    /// In-process TTL cache for the creator-facing OPEN-period projected charge
    /// (billing-ops gap #26, PR-7, read API G / MAJOR-5). Memoises
    /// `(app_id, period) → projected_cents` for
    /// [`billing_read::PROJECTED_CHARGE_TTL_SECS`] so polling cannot hammer a
    /// full pricing pass. The value is non-authoritative (only a finalized
    /// invoice bills).
    pub projected_charge_cache: Arc<billing_read::ProjectedChargeCache>,
}

impl AppState {
    /// Build the shared bearer verifier from the only control-plane state the
    /// OAuth bearer path needs.
    #[must_use]
    pub fn bearer_verifier(&self) -> zeroship_authn::BearerVerifier {
        zeroship_authn::BearerVerifier::new(
            Arc::clone(&self.control_pg),
            Arc::clone(&self.auth_provider),
            self.trusted_oauth_clients.clone(),
            self.expected_oauth_audience.clone(),
        )
    }

    /// Return whether `client_id` is configured as a trusted OAuth client.
    #[must_use]
    pub fn is_trusted(&self, client_id: &str) -> bool {
        Self::is_trusted_client_id(&self.trusted_oauth_clients, client_id)
    }

    /// The public URL scheme hosted apps serve under. Drives per-app OAuth
    /// redirect URI derivation and other browser-visible control URLs.
    #[must_use]
    pub fn app_scheme(&self) -> &'static str {
        self.origin_scheme.as_str()
    }

    /// The apex host an app named `name` serves at:
    /// `{name}.{app_base_domain}`. The per-app OAuth client's
    /// redirect_uris / sector_identifier anchor here.
    #[must_use]
    pub fn apex_host_for_app(&self, name: &str) -> String {
        format!("{name}.{}", self.app_base_domain)
    }

    /// Idempotently provision (or reconcile) the per-app OAuth
    /// client for `app_id` named `name`, using the apex host derived from
    /// `app_base_domain`. Wraps [`app_oauth_client::ensure_app_client`] with
    /// a fresh control-DB connection. On success returns the per-app
    /// `client_id` (`oac_<base62-app-id>`).
    ///
    /// `declared_scopes` are the app's manifest `auth.scopes`: validated and
    /// mirrored into `zeroship.oauth_clients.scopes` and
    /// `zeroship.app_scope_defs` atomically. Pass `&[]` at app
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
        let mut conn = self
            .registry
            .conn()
            .await
            .map_err(|e| format!("control db conn: {e}"))?;
        app_oauth_client::ensure_app_client(
            // Creator apps are NEVER first-party (spec §5.2): skip_consent=false.
            &mut conn, app_id, name, scheme, &hosts, declared_scopes, false,
        )
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

#[cfg(test)]
mod billing_stream_group_tests {
    use super::*;

    fn config_for(replica: &str) -> BillingStreamConfig {
        let base = StreamConfig::new(serde_json::json!({
            "topic": "zeroship.usage.events",
            "brokers": "127.0.0.1:9092",
            "group.id": "placeholder",
        }));
        BillingStreamConfig::new_for_replica(
            Arc::new(StreamRegistry::default()),
            "memory",
            base,
            DEFAULT_BILLING_FORWARDER_GROUP_ID,
            DEFAULT_SPEND_RECOMPUTE_GROUP_ID,
            replica,
        )
        .expect("config")
    }

    #[test]
    fn two_replicas_get_separate_recompute_groups_but_share_the_forwarder_group() {
        let a = config_for("control-a");
        let b = config_for("control-b");

        // THE DEFECT. The recompute REBUILDS the whole period snapshot and
        // calls `replace_period_snapshot`, so it must read every partition.
        // Two replicas in ONE consumer group split partitions between them,
        // each computes a partial total, and each overwrites the other - the
        // month total lands somewhere below the truth with nothing logged.
        assert_ne!(
            a.recompute_group_id(),
            b.recompute_group_id(),
            "two control replicas share a recompute group, so each sees only its \
             assigned partitions and writes a partial snapshot over the other's"
        );

        // The OPPOSITE requirement, in the same struct, which is why this is
        // not "make all the groups unique". The forwarder is a WORK QUEUE:
        // every event must be forwarded exactly once, so splitting partitions
        // across replicas is the correct behaviour and a per-replica group
        // would forward each event N times.
        assert_eq!(
            a.forwarder_group_id(),
            b.forwarder_group_id(),
            "the forwarder group must stay shared or every event is forwarded once per replica"
        );
    }

    #[test]
    fn a_replicas_recompute_group_is_stable_across_restarts() {
        // Unique per REPLICA, not per BOOT. A fresh group each restart would
        // leave abandoned group metadata on the broker for every process that
        // ever ran, and buys nothing: the recompute rewinds every cycle and
        // never commits an offset.
        assert_eq!(
            config_for("control-a").recompute_group_id(),
            config_for("control-a").recompute_group_id(),
        );
    }

    #[test]
    fn the_three_groups_still_differ_after_the_suffix() {
        // `validate()` rejects overlap between forwarder / recompute / producer.
        // Suffixing must not accidentally collide any pair.
        let a = config_for("control-a");
        assert_ne!(a.forwarder_group_id(), a.recompute_group_id());
        assert_ne!(a.recompute_group_id(), DEFAULT_CONTROL_USAGE_PRODUCER_GROUP_ID);
        assert_ne!(a.forwarder_group_id(), DEFAULT_CONTROL_USAGE_PRODUCER_GROUP_ID);
    }
}
