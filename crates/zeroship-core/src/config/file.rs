//! File-overlay schema: the TOML sections and their load/parse error type.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use super::topology::{OriginScheme, TrustedOrigin};

/// Error returned while loading an optional zeroship configuration file.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The configuration file could not be read.
    #[error("read {path}: {source}")]
    Io {
        /// Path that failed to read.
        path: PathBuf,
        /// Underlying filesystem error.
        #[source]
        source: std::io::Error,
    },

    /// The configuration file could not be parsed as TOML.
    #[error("parse {path}: {source}")]
    Parse {
        /// Path that failed to parse.
        path: PathBuf,
        /// Underlying TOML parser error.
        #[source]
        source: toml::de::Error,
    },

    /// A generated declaration could not be resolved against its sources.
    #[error(transparent)]
    Resolve(#[from] super::names::ConfigResolveError),
}

/// Optional cross-binary domain configuration loaded from `deploy/ops/zeroship.toml`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    /// Scheme used in browser-visible platform URLs.
    pub origin_scheme: Option<OriginScheme>,
    /// Additional exact origins accepted by the gateway's same-origin guards.
    pub trusted_origins: Option<Vec<TrustedOrigin>>,
    /// Auth-domain configuration shared by platform binaries.
    #[serde(default)]
    pub auth: AuthSection,
    /// Observability configuration shared by platform binaries.
    #[serde(default)]
    pub observability: ObsSection,
    /// Platform-wide admin/control API shared secret.
    ///
    /// Secrets sit WITH their siblings now, not in a `[secrets]` table: location
    /// encodes sharing, so a key at the overlay root is one every consumer
    /// shares and a key under `[control]` is control's alone. The flat table
    /// destroyed exactly that distinction. A literal is permitted here, because
    /// the overlay may itself BE a mounted Kubernetes Secret; the prohibition on
    /// a plaintext secret applies to a TRACKED file, not to this format.
    pub control_key: Option<String>,
    /// Dedicated PERMANENT pairwise-salt secret (auth-sdk 6.2). The per-app
    /// `pws_` identity anchor seed, identical on auth, gateway, and control, never
    /// rotated without a migration.
    pub pairwise_salt: Option<String>,
    /// Usage-metering stream (Redpanda) configuration shared by the usage
    /// producers (worker + gateway) and the control-plane consumers. Absent
    /// fields fall back to env/default.
    #[serde(default)]
    pub metering: MeteringSection,
    /// Root directory or `s3://` URL for content-addressed deploy blobs,
    /// shared by control, gateway and worker.
    pub blob_store: Option<String>,
    /// Control-plane API base URL, shared by gateway and worker.
    pub control_url: Option<String>,
    /// Comma-separated worker base URLs, shared by control and gateway.
    pub worker_urls: Option<String>,
    /// Polling interval in seconds, shared by gateway and worker.
    pub poll_interval: Option<u64>,
    /// Expected OAuth access-token audience, shared by control and migrated.
    pub oauth_audience: Option<String>,
    /// Trust `X-Forwarded-For` from an upstream proxy, shared by control and
    /// gateway.
    pub trust_proxy: Option<bool>,
    /// Control-plane settings.
    #[serde(default)]
    pub control: ControlSection,
    /// Gateway settings.
    #[serde(default)]
    pub gateway: GatewaySection,
    /// Worker settings.
    #[serde(default)]
    pub worker: WorkerSection,
    /// Migration-service settings.
    #[serde(default)]
    pub migrate_server: MigrateServerSection,
    /// Platform-schema migrate one-shot settings.
    #[serde(default)]
    pub platform_migrate: PlatformMigrateSection,
    /// Standalone workflow-scheduler settings.
    #[serde(default)]
    pub workflow_scheduler: SchedulerSection,
    /// CDC relay settings.
    #[serde(default)]
    pub data_cdc_server: CdcServerSection,
}

/// Control-plane operational values supplied by the overlay.
///
/// Like [`ObsSection`], this exists so `deny_unknown_fields` still ACCEPTS a
/// `[control]` table and still rejects a typo inside it; the values come from
/// the generated declarations walking the same canonical paths.
///
/// The two `BootstrapControl` fields on that declaration -
/// `disable_workflow_engine` and `allow_unsupported_billing` - are deliberately
/// ABSENT. A bootstrap control has no overlay tier, so a key here would be
/// accepted by the parser and then ignored by the resolver, which is worse than
/// being rejected.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ControlSection {
    /// `PostgreSQL` DSN for control-plane data. A DSN grammar admits userinfo,
    /// so it is secret-classed regardless of whether a given value carries a
    /// password.
    pub database_url: Option<String>,
    /// Master key used for control-plane encrypted env/secrets.
    pub master_key: Option<String>,
    /// Comma-separated previous master keys accepted during a key rotation.
    pub legacy_master_keys: Option<String>,
    /// Stripe webhook signing secret.
    pub stripe_webhook_secret: Option<String>,
    /// Stripe secret API key (`sk_...`) for outbound calls.
    pub stripe_secret_key: Option<String>,
    /// Billing-notification SMTP password.
    pub smtp_password: Option<String>,
    /// Billing-notification Resend API key.
    pub resend_api_key: Option<String>,
    /// This control plane's OWN ed25519 assertion key FILE. See
    /// `AuthSection::service_key_file` for why the pair lives here.
    pub service_key_file: Option<std::path::PathBuf>,
    /// See `AuthSection::service_key_file`.
    pub service_peers_file: Option<std::path::PathBuf>,
    /// HTTP listen port.
    pub port: Option<u16>,
    /// Bind address.
    pub bind: Option<String>,
    /// Gateway internal base URL for the workflow dispatch seam.
    pub gateway_url: Option<String>,
    /// Usage meter provider.
    pub meter_provider: Option<String>,
    /// Invoicer provider.
    pub invoicer_provider: Option<String>,
    /// Opaque provider JSON config.
    pub provider_config: Option<String>,
    /// Durable usage-event stream transport.
    pub stream_transport: Option<String>,
    /// Opaque stream transport JSON config.
    pub stream_config: Option<String>,
    /// Billing forwarder consumer group.
    pub billing_forwarder_group_id: Option<String>,
    /// Spend recompute consumer group.
    pub spend_recompute_group_id: Option<String>,
    /// Spend recompute interval in seconds.
    pub spend_recompute_interval: Option<u64>,
    /// Tax provider backend.
    pub tax_provider: Option<String>,
    /// Stripe REST API base URL.
    pub stripe_base_url: Option<String>,
    /// Billing-notification mailer driver.
    pub mailer: Option<String>,
    /// SMTP host.
    pub smtp_host: Option<String>,
    /// SMTP port.
    pub smtp_port: Option<u16>,
    /// SMTP username.
    pub smtp_username: Option<String>,
    /// Directory for in-flight deploy bodies.
    pub deploy_tmp_dir: Option<String>,
    /// JWKS URL for asymmetric GoTrue JWT verification.
    pub supabase_jwks_url: Option<String>,
    /// GoTrue JWT issuer pinned during Supabase token verification.
    pub supabase_jwt_issuer: Option<String>,
    /// Apex domain hosted creator apps serve under.
    pub app_base_domain: Option<String>,
    /// Comma-separated CIDRs a worker instance may enrol from.
    pub worker_enrolment_networks: Option<String>,
    /// Listening ports a worker instance may claim, as `<low>-<high>`.
    pub worker_enrolment_ports: Option<String>,
    /// Audit retention horizon in months.
    pub audit_retention_months: Option<u32>,
    /// Audit retention cron tick in seconds.
    pub audit_retention_check_secs: Option<u64>,
}

/// Gateway operational values supplied by the overlay. See [`ControlSection`].
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct GatewaySection {
    /// `PostgreSQL` DSN for gateway session validation.
    pub database_url: Option<String>,
    /// HMAC key for short-lived OIDC stash cookies.
    pub stash_signing_key: Option<String>,
    /// Broker master-secret source FILE, byte-identical to auth's.
    ///
    /// This section carried `broker_secret: Option<String>` instead until
    /// 2026-08-13, which was wrong in both directions and neither half was
    /// visible from here. Nothing read `broker_secret`, so an operator who set
    /// it configured nothing; and the gateway's actual declaration is
    /// `gateway.broker_secret_file`, which `deny_unknown_fields` REJECTED,
    /// so the overlay tier the contract advertises could not be used at all.
    /// Found by the generated-TOML-path check in `zeroship-config-contract
    /// audit`, which now requires every `ConfigSpec` overlay path to be a leaf
    /// this schema accepts.
    pub broker_secret_file: Option<std::path::PathBuf>,
    /// PEM/PKCS#8 signing key FILE for the gateway-signed session cookie.
    /// PEM/PKCS#8 signing key FILE for the wrapper-token issuer. A path, not
    /// key material: the loader owns the format sniff and the permission
    /// check, so making the contents an in-memory secret would drop both.
    pub signing_key_file: Option<std::path::PathBuf>,
    /// PEM/PKCS#8 PREVIOUS signing key FILE for the rotation overlap.
    pub prev_signing_key_file: Option<std::path::PathBuf>,
    /// This gateway's OWN ed25519 assertion key FILE. See
    /// `AuthSection::service_key_file` for why the pair lives here.
    pub service_key_file: Option<std::path::PathBuf>,
    /// See `AuthSection::service_key_file`.
    pub service_peers_file: Option<std::path::PathBuf>,
    /// HTTP listen port.
    pub port: Option<u16>,
    /// Bind address.
    pub bind: Option<String>,
    /// In-memory blob cache budget in MiB.
    pub blob_cache_mem_mb: Option<usize>,
    /// On-disk blob cache budget in GiB.
    pub blob_cache_disk_gb: Option<u64>,
    /// Root directory for the on-disk blob cache.
    pub blob_cache_disk_root: Option<String>,
    /// Pooled `PostgreSQL` connection ceiling.
    pub db_pool_size: Option<usize>,
    /// Public URL advertised as the session-cookie issuer.
    pub public_url: Option<String>,
    /// Upstream URL for the auth service UI and OAuth surfaces.
    pub auth_ui_url: Option<String>,
}

/// Worker operational values supplied by the overlay. See [`ControlSection`].
///
/// `workflow_advance_unsigned` is deliberately absent for the same reason the
/// control safety controls are: it is a `BootstrapControl` with no overlay tier.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct WorkerSection {
    /// `PostgreSQL` DSN for worker-side lookups.
    pub database_url: Option<String>,
    /// App-runtime KV (Redis) connection URL. May carry credentials.
    pub kv_url: Option<String>,
    /// This worker's OWN ed25519 assertion key FILE. See
    /// `AuthSection::service_key_file` for why the pair lives here.
    pub service_key_file: Option<std::path::PathBuf>,
    /// See `AuthSection::service_key_file`.
    pub service_peers_file: Option<std::path::PathBuf>,
    /// HTTP listen port.
    pub port: Option<u16>,
    /// Bind address.
    pub bind: Option<String>,
    /// Optional Unix domain socket path.
    pub socket: Option<String>,
    /// ntex worker threads.
    pub threads: Option<usize>,
    /// Maximum cached app isolates.
    pub max_isolates: Option<usize>,
    /// Maximum deploy-pinned workflow replay isolates per app.
    pub max_pinned_isolates_per_app: Option<usize>,
    /// Shutdown drain timeout in seconds.
    pub shutdown_timeout: Option<u64>,
    /// Object-store location for the app `env.storage` namespace.
    pub storage_url: Option<String>,
    /// Maximum persisted bytes for one workflow step output blob.
    pub max_step_blob_bytes: Option<u64>,
}

/// Migration-service operational values supplied by the overlay.
/// See [`ControlSection`].
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct MigrateServerSection {
    /// `PostgreSQL` DSN for the migration service.
    pub database_url: Option<String>,
    /// Privileged provisioning DSN (CREATEROLE + CREATE on the database).
    pub provision_database_url: Option<String>,
    /// Key sealing the managed migration policy profile.
    pub policy_seal_key: Option<String>,
    /// HTTP listen port.
    pub port: Option<u16>,
    /// Bind address.
    pub bind: Option<String>,
    /// Directory for staged request migration files.
    pub tmp_dir: Option<std::path::PathBuf>,
    /// Active managed ceiling version stamped into sealed profiles.
    pub policy_ceiling_version: Option<u64>,
    /// Maximum mutating requests one source IP may burst.
    pub mutation_rate_limit_burst: Option<u32>,
    /// Sustained mutating requests per minute for one source IP.
    pub mutation_rate_limit_per_minute: Option<u32>,
}

/// Platform-schema migrate one-shot values supplied by the overlay.
/// See [`ControlSection`].
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PlatformMigrateSection {
    /// Admin `PostgreSQL` DSN for platform DDL.
    pub database_url: Option<String>,
    /// Directory holding the `db/migrations-ts/*.ts` platform migrations.
    pub migrations_dir: Option<std::path::PathBuf>,
    /// The primary platform schema.
    pub project_schema: Option<String>,
    /// The advisory-lock / journal project id.
    pub project_id: Option<String>,
    /// Database on the same cluster that concurrent runs coordinate through.
    pub cluster_lock_database: Option<String>,
}

/// Workflow-scheduler operational values supplied by the overlay.
///
/// Like [`ObsSection`], this exists so `deny_unknown_fields` still ACCEPTS a
/// `[workflow_scheduler]` table and still rejects a typo inside it. The values
/// each binary uses come from the generated declarations, which walk the same
/// overlay by canonical path, so the key spellings here and there are the same
/// by construction.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SchedulerSection {
    /// `PostgreSQL` DSN for the scheduler timer and inflight tables.
    pub database_url: Option<String>,
    /// Schema holding the scheduler timer and inflight tables.
    pub schema: Option<String>,
    /// Gateway internal base URL the dispatch seam posts to.
    pub gateway_url: Option<String>,
    /// Control-plane apply endpoint the scheduler acknowledges through.
    pub control_apply_url: Option<String>,
    /// Timer-wheel tick interval in seconds.
    pub tick_secs: Option<u64>,
    /// Interval in seconds between inflight-lease reaper sweeps.
    pub reaper_interval_secs: Option<u64>,
    /// Horizon in milliseconds within which a timer is loaded into the wheel.
    pub near_horizon_ms: Option<i64>,
    /// Maximum timers held in the in-memory wheel.
    pub max_loaded_timers: Option<i64>,
    /// Maximum due timers claimed per tick.
    pub max_due_per_tick: Option<usize>,
    /// Inflight-lease time-to-live in milliseconds.
    pub inflight_ttl_ms: Option<i64>,
}

/// CDC relay values supplied by the overlay.
///
/// Like [`SchedulerSection`], this exists so `deny_unknown_fields` still ACCEPTS
/// a `[data_cdc_server]` table and still rejects a typo inside it. The value the
/// binary uses comes from its generated declaration, which walks the same
/// overlay by canonical path.
///
/// ONE KEY, and that is the whole relay surface today: it serves no endpoint, so
/// there is no listener to configure. `zeroship-config-contract audit` is what
/// makes this section obligatory rather than optional - a declared overlay path
/// with no field here is accepted by the generator and then REJECTED by
/// `deny_unknown_fields` at parse time, so the operator's TOML would fail on a
/// key the reference documents.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CdcServerSection {
    /// `PostgreSQL` DSN the relay streams logical replication from.
    pub database_url: Option<String>,
}

/// Usage-metering stream configuration supplied by the shared file overlay.
///
/// Like [`ControlSection`], the keys here exist so `deny_unknown_fields` still
/// ACCEPTS a `[metering]` table and still rejects a typo inside it. The
/// producers do NOT read this struct: `metering.brokers` and its three siblings
/// are generated declarations on the worker and the gateway, so each walks this
/// same canonical path itself and gets the flag / `ZEROSHIP_METERING_*` tiers
/// above it. The control plane, which consumes the stream rather than producing
/// into it, does still read the struct - it derives `--stream-transport` /
/// `--stream-config` defaults from the brokers the producers were pointed at.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct MeteringSection {
    /// Kafka-wire broker list for the usage-event stream, e.g.
    /// `"redpanda:9092"`. Unset means the usage producer is disabled
    /// (drain-and-drop).
    pub brokers: Option<String>,
    /// Usage-event topic (default `"usage-events"`).
    pub events_topic: Option<String>,
    /// Producer consumer-group id override.
    pub producer_group_id: Option<String>,
    /// redb WAL path for the outbox. Per-process: two producers on one host
    /// must not name one redb file, which is single-writer.
    pub outbox_wal_path: Option<String>,
}

/// One first-party OAuth client registered against the platform OP.
///
/// The shape mirrors what the deleted `POST /admin/oauth-clients` body carried,
/// minus the two things a config file must not be asked to express: the
/// generated-and-shown-once secret (supply one here, or use a public client),
/// and `skip_consent`, which stays DERIVED from `trusted_oauth_clients` so a
/// registration cannot grant itself consent-free access.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OauthClientRegistration {
    /// Stable client identifier, e.g. `zeroship-console`.
    pub client_id: String,
    /// Human-readable name shown on the consent screen.
    pub client_name: String,
    /// Optional homepage shown on the consent screen.
    pub client_uri: Option<String>,
    /// Optional logo shown on the consent screen.
    pub logo_uri: Option<String>,
    /// Registered redirect URIs. HTTPS, or loopback HTTP.
    pub redirect_uris: Vec<String>,
    /// Scopes from the closed OAuth vocabulary this client may request.
    pub scopes: Vec<String>,
    /// `client_secret_basic` (confidential) or `none` (public).
    pub token_endpoint_auth_method: String,
    /// The confidential client's secret. Required for `client_secret_basic`,
    /// forbidden for `none`. Only its hash is persisted.
    ///
    /// A literal is permitted here for the same reason `control_key` admits
    /// one: the overlay may itself BE a mounted secret. The prohibition on a
    /// plaintext secret applies to a TRACKED file, not to this format.
    pub client_secret: Option<String>,
    /// Whether the token endpoint may issue a refresh token to this client.
    #[serde(default)]
    pub refresh_allowed: bool,
}

/// Auth-domain values that can be supplied by the shared file overlay.
///
/// Like [`ControlSection`], most of this exists so `deny_unknown_fields` still
/// ACCEPTS an `[auth]` table and still rejects a typo inside it; the values the
/// auth service uses come from its generated declaration walking the same
/// canonical `auth.*` paths.
///
/// `trusted_oauth_clients` is the exception: it is file-and-default ONLY, with
/// no flag and no environment variable, so this field is where it is actually
/// read from.
///
/// The bootstrap and command controls on the auth declaration (`config`,
/// `no_config`, `check_config`, `check_config_format`) are deliberately absent -
/// they are root-scoped names with no overlay tier at all, so a key here would
/// be accepted by the parser and then ignored by the resolver.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AuthSection {
    /// `PostgreSQL` DSN for the auth service.
    pub database_url: Option<String>,
    /// HMAC key for short-lived stash cookies.
    pub stash_signing_key: Option<String>,
    /// Pairwise-salt source FILE for auth-issued subject identifiers.
    ///
    /// A PATH, like every key file below it. The loaders read raw BYTES and
    /// refuse a group- or world-readable file; a string leaf would reject
    /// non-UTF-8 key material and would strip a trailing newline that
    /// `derive_pairwise_salt` currently hashes, silently changing every derived
    /// value. A path to a secret is not itself a secret.
    pub pairwise_salt_file: Option<std::path::PathBuf>,
    /// TOTP at-rest encryption key (AES-256-GCM, >=32 decoded bytes).
    pub totp_enc_key: Option<String>,
    /// Shared platform broker master-secret FILE. Its bytes must be identical
    /// to the gateway's broker secret.
    pub broker_secret_file: Option<std::path::PathBuf>,
    /// PREVIOUS broker master-secret FILE, accepted during a broker-secret roll.
    pub broker_secret_previous_file: Option<std::path::PathBuf>,
    /// Refresh-token HMAC keyring FILE (a multi-line `version:key` document).
    pub refresh_hash_key_file: Option<std::path::PathBuf>,
    /// Refresh-rotation idempotency AEAD key source FILE.
    pub refresh_idem_key_file: Option<std::path::PathBuf>,
    /// Supabase service-role key used for admin lookups.
    pub supabase_service_role_key: Option<String>,
    /// HS256 GoTrue JWT secret. Mutually exclusive with the JWKS URL.
    pub supabase_jwt_secret: Option<String>,
    /// GoTrue send-email hook shared secret.
    pub gotrue_email_hook_secret: Option<String>,
    /// Google OAuth client secret.
    pub google_client_secret: Option<String>,
    /// GitHub OAuth client secret.
    pub github_client_secret: Option<String>,
    /// Transactional Resend API key.
    pub resend_api_key: Option<String>,
    /// Transactional SMTP password.
    pub smtp_password: Option<String>,
    /// Postmark inbound webhook Basic-auth password.
    pub postmark_webhook_password: Option<String>,
    /// Relay inbound webhook Basic-auth password.
    pub relay_inbound_password: Option<String>,
    /// Relay-forward SMTP password.
    pub relay_smtp_password: Option<String>,
    /// PEM/PKCS#8 signing key FILE. See `GatewaySection::signing_key_file`.
    pub signing_key_file: Option<std::path::PathBuf>,
    /// This service's OWN ed25519 assertion key FILE, and the JWKS-shaped
    /// document naming every peer's public key.
    ///
    /// Present on all four platform sections for the same reason
    /// `GatewaySection::broker_secret_file` is: the binaries DECLARE these
    /// overlay paths, so leaving them out of this schema does not make them
    /// optional, it makes `deny_unknown_fields` reject any overlay that uses
    /// them - the tier the contract advertises would not exist. The four
    /// sections' pairs were missing together and `zeroship-config-contract
    /// audit` named all of them.
    pub service_key_file: Option<std::path::PathBuf>,
    /// See `service_key_file`.
    pub service_peers_file: Option<std::path::PathBuf>,
    /// Platform auth provider backend (`native` or `supabase`).
    ///
    /// `provider`, not `auth_provider`: the canonical identity is
    /// `auth.provider`, and the scope segment already says `auth`. This ONE key
    /// governs both the auth service and control; `[control] auth_provider` was
    /// its second half and is gone, so `deny_unknown_fields` now rejects it.
    pub provider: Option<String>,
    /// Supabase Auth / GoTrue base URL used when the auth provider is Supabase.
    pub supabase_url: Option<String>,
    /// Supabase anon API key used by browser-side GoTrue session calls.
    pub supabase_anon_key: Option<String>,
    /// Platform OP issuer accepted for platform-issued access tokens.
    pub platform_issuer: Option<String>,
    /// Platform OP JWKS URL. Defaults to `{platform_issuer}/.well-known/jwks.json`.
    pub platform_jwks_url: Option<String>,
    /// First-party OAuth client IDs trusted by the platform.
    ///
    /// `None` (key absent) means "use the compiled-in default set"; `Some(vec)`
    /// means exactly that set, where an empty vec is "no trusted clients".
    pub trusted_oauth_clients: Option<Vec<String>>,
    /// The first-party relying parties registered against the platform's own
    /// OP, reconciled into `zeroship.oauth_clients` at control boot.
    ///
    /// File-and-default ONLY, like `trusted_oauth_clients`: it is a list of
    /// tables, and the generated declarations carry scalars. Registering an RP
    /// of your own OP is a deployment decision, not a runtime one, which is why
    /// this replaced the three `/admin/oauth-clients` routes rather than moving
    /// them behind a different credential.
    ///
    /// `None` (key absent) leaves the table untouched. `Some(list)` makes this
    /// the AUTHORITATIVE first-party set: entries are upserted, and a
    /// first-party row that is no longer named is de-registered. Per-app
    /// end-user clients (`oac_…`) and the platform CLI client are never touched
    /// by that pruning - they are provisioned by the deploy path and by the
    /// auth service, not from here.
    pub oauth_clients: Option<Vec<OauthClientRegistration>>,
    /// Console origin(s) the auth-service login/signup/consent documents admit
    /// via CSP `frame-ancestors` so the console's immersive iframe login can
    /// embed them (design §4.3/§10.1). Deployment-injected, mirroring the
    /// `trusted_oauth_clients` pattern: core has no console host.
    ///
    /// EXACT origins only - NO wildcards. Whatever lands here is still filtered
    /// fail-closed by the auth service before it can reach a CSP header: a
    /// non-concrete origin, or one that is not same-site with the auth issuer,
    /// is DROPPED.
    pub frame_ancestor_origins: Option<Vec<String>>,
    /// Listen address.
    pub addr: Option<String>,
    /// Google OAuth 2.0 client ID.
    pub google_client_id: Option<String>,
    /// Redirect URI registered with Google.
    pub google_redirect_uri: Option<String>,
    /// Google's authorize endpoint.
    pub google_auth_url: Option<String>,
    /// Google's token endpoint.
    pub google_token_url: Option<String>,
    /// Google's JWKS endpoint.
    pub google_jwks_url: Option<String>,
    /// Expected `iss` claim on Google ID tokens.
    pub google_issuer: Option<String>,
    /// GitHub OAuth App client ID.
    pub github_client_id: Option<String>,
    /// Callback URL registered on the GitHub OAuth App.
    pub github_redirect_uri: Option<String>,
    /// GitHub's authorize endpoint.
    pub github_authorize_url: Option<String>,
    /// GitHub's token endpoint.
    pub github_token_url: Option<String>,
    /// GitHub's `/user` endpoint.
    pub github_user_url: Option<String>,
    /// GitHub's `/user/emails` endpoint.
    pub github_emails_url: Option<String>,
    /// Transactional mailer driver.
    pub mailer: Option<String>,
    /// Transactional SMTP host.
    pub smtp_host: Option<String>,
    /// Transactional SMTP port.
    pub smtp_port: Option<u16>,
    /// Transactional SMTP username.
    pub smtp_username: Option<String>,
    /// Transactional SMTP transport encryption mode.
    pub smtp_tls: Option<String>,
    /// `From` address for transactional mail.
    pub mail_from_email: Option<String>,
    /// `From` display name for transactional mail.
    pub mail_from_name: Option<String>,
    /// Public, externally-reachable origin of the auth server.
    pub public_url: Option<String>,
    /// Refresh-family database session ceiling per auth worker.
    pub refresh_pool_size: Option<usize>,
    /// Relay alias domain.
    pub relay_domain: Option<String>,
    /// Relay inbound webhook Basic-auth username.
    pub relay_inbound_user: Option<String>,
    /// Relay-forward mailer driver.
    pub relay_forward_mailer: Option<String>,
    /// Relay-forward SMTP host.
    pub relay_smtp_host: Option<String>,
    /// Relay-forward SMTP port.
    pub relay_smtp_port: Option<u16>,
    /// Relay-forward SMTP username.
    pub relay_smtp_username: Option<String>,
    /// Relay-forward SMTP transport encryption mode.
    pub relay_smtp_tls: Option<String>,
    /// Postmark webhook Basic-auth username.
    pub postmark_webhook_user: Option<String>,
    /// Auth cron tick interval in seconds.
    pub cron_tick_secs: Option<u64>,
    /// Audit-retention sweeper tick interval in seconds.
    pub audit_retention_check_secs: Option<u64>,
}

/// Observability values that can be supplied by the shared file overlay.
///
/// These fields exist so `deny_unknown_fields` still accepts an
/// `[observability]` table and rejects a typo inside it. The VALUES each binary
/// uses come from the generated `observability.log_filter` /
/// `observability.log_format` declarations, which walk the same overlay by
/// canonical path; the key spellings here and there are therefore the same by
/// construction, and both parse `log_format` into a
/// [`LogFormat`](crate::observability::LogFormat).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ObsSection {
    /// `EnvFilter` directive.
    pub log_filter: Option<String>,
    /// Tracing output format.
    pub log_format: Option<crate::observability::LogFormat>,
}

impl FileConfig {
    /// Load an optional TOML overlay from `path`.
    ///
    /// This is the explicit-only primitive: passing `None` returns an
    /// all-default configuration and does *not* probe any well-known path.
    /// Passing `Some` reads the file and parses it as TOML. Callers that want
    /// system-path auto-discovery use [`FileConfig::resolve`], which is built
    /// on top of this primitive.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Io`] when the file cannot be read, or
    /// [`ConfigError::Parse`] when the file is not valid TOML for this shape.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        Ok(Self::load_with_raw(path)?.0)
    }

    /// [`FileConfig::load`], also returning the untyped overlay tree.
    ///
    /// Generated declarations walk the raw tree by canonical path, while the
    /// typed shape keeps `deny_unknown_fields` rejecting a misspelled key. Both
    /// come from ONE parse so they cannot describe different bytes.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Io`] when the file cannot be read, or
    /// [`ConfigError::Parse`] when the file is not valid TOML for this shape.
    pub fn load_with_raw(
        path: Option<&Path>,
    ) -> Result<(Self, Option<toml::Value>), ConfigError> {
        let Some(path) = path else {
            return Ok((Self::default(), None));
        };

        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let parse = |source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        };
        let raw: toml::Value = toml::from_str(&text).map_err(parse)?;
        let typed = raw.clone().try_into().map_err(parse)?;
        Ok((typed, Some(raw)))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{ConfigError, FileConfig};

    struct TempFile {
        path: PathBuf,
    }

    impl TempFile {
        fn write(name: &str, contents: &str) -> Self {
            static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

            let path = std::env::temp_dir().join(format!(
                "zeroship-core-config-{name}-{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::write(path.as_path(), contents).expect("write temp config");
            Self { path }
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.path.as_path());
        }
    }

    #[test]
    fn load_none_returns_defaults() {
        let config = FileConfig::load(None).expect("load default config");

        assert!(config.auth.provider.is_none());
        assert!(config.origin_scheme.is_none());
        assert!(config.trusted_origins.is_none());
        assert!(config.auth.supabase_url.is_none());
        assert!(config.auth.supabase_anon_key.is_none());
        assert!(config.auth.platform_issuer.is_none());
        assert!(config.auth.platform_jwks_url.is_none());
        assert!(config.auth.trusted_oauth_clients.is_none());
        assert!(config.auth.frame_ancestor_origins.is_none());
        assert!(config.observability.log_filter.is_none());
        assert!(config.observability.log_format.is_none());
    }

    // Immersive-login pivot (design §4.3/§10.1, §9): `[auth].frame_ancestor_origins`
    // parses into the matching `AuthSection` field so the auth service can admit
    // the console origin via CSP `frame-ancestors`.
    #[test]
    fn frame_ancestor_origins_parses_from_auth_section() {
        let file = TempFile::write(
            "frame-ancestors.toml",
            r#"
[auth]
frame_ancestor_origins = ["https://console.zeroship.ai", "https://staging-console.zeroship.ai"]
"#,
        );

        let config = FileConfig::load(Some(&file.path)).expect("load config");
        assert_eq!(
            config.auth.frame_ancestor_origins.as_deref(),
            Some(
                [
                    "https://console.zeroship.ai".to_string(),
                    "https://staging-console.zeroship.ai".to_string(),
                ]
                .as_slice()
            )
        );
    }

    #[test]
    fn load_full_config_populates_fields() {
        let file = TempFile::write(
            "full.toml",
            r#"
origin_scheme = "http"
trusted_origins = ["https://console.zeroship.ai", "http://localhost:3000"]

[auth]
provider = "supabase"
supabase_url = "https://project.supabase.test"
supabase_anon_key = "anon-test-key"
platform_issuer = "https://auth.zeroship.ai"
platform_jwks_url = "https://auth.zeroship.ai/.well-known/jwks.json"
trusted_oauth_clients = ["zeroship-builder", "zeroship-console"]

[observability]
log_filter = "info,zeroship_=debug"
log_format = "json"
"#,
        );

        let config = FileConfig::load(Some(&file.path)).expect("load config");

        assert_eq!(config.origin_scheme, Some(super::OriginScheme::Http));
        assert_eq!(
            config
                .trusted_origins
                .as_deref()
                .expect("trusted origins")
                .iter()
                .map(super::TrustedOrigin::as_str)
                .collect::<Vec<_>>(),
            vec!["https://console.zeroship.ai", "http://localhost:3000"]
        );

        assert_eq!(config.auth.provider.as_deref(), Some("supabase"));
        assert_eq!(
            config.auth.supabase_url.as_deref(),
            Some("https://project.supabase.test")
        );
        assert_eq!(
            config.auth.supabase_anon_key.as_deref(),
            Some("anon-test-key")
        );
        assert_eq!(
            config.auth.platform_issuer.as_deref(),
            Some("https://auth.zeroship.ai")
        );
        assert_eq!(
            config.auth.platform_jwks_url.as_deref(),
            Some("https://auth.zeroship.ai/.well-known/jwks.json")
        );
        assert_eq!(
            config.auth.trusted_oauth_clients.as_deref(),
            Some(["zeroship-builder".to_string(), "zeroship-console".to_string()].as_slice())
        );
        assert_eq!(
            config.observability.log_filter.as_deref(),
            Some("info,zeroship_=debug")
        );
        assert_eq!(config.observability.log_format, Some(crate::observability::LogFormat::Json));
    }

    #[test]
    fn load_ops_zeroship_toml_parses() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../deploy/ops/zeroship.toml");

        let config = FileConfig::load(Some(&path)).expect("load deploy/ops/zeroship.toml");

        assert_eq!(
            config.observability.log_filter.as_deref(),
            Some("info,zeroship_=debug")
        );
    }

    // The SHIPPED EXAMPLE, which had no parse test at all and was therefore free
    // to drift. It carried Vault and AWS Secrets Manager references that always
    // failed at boot, and env-to-env references for the alias hop, so the file an
    // operator copies documented three sources that do not exist. Every value in
    // it is now a file reference, and every key is a leaf the schema knows -
    // which only a load can establish, because `deny_unknown_fields` is where a
    // stale key surfaces.
    #[test]
    fn the_shipped_example_overlay_parses_and_places_secrets_at_canonical_paths() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../deploy/ops/zeroship.example.toml");

        let config = FileConfig::load(Some(&path)).expect("load zeroship.example.toml");

        // A platform-global secret sits at the ROOT, not inside the first table
        // that happens to precede it. TOML scoping makes that a real hazard: a
        // root key written after a table header silently joins that table.
        assert!(config.control_key.is_some(), "control_key must be a root key");
        assert!(config.pairwise_salt.is_some());
        // A per-binary secret sits in that binary's table.
        assert!(config.control.master_key.is_some());
        assert!(config.gateway.stash_signing_key.is_some());
        assert!(config.migrate_server.policy_seal_key.is_some());

        // Every reference in the tracked example must be a FILE reference: the
        // schemes this step deleted are the ones an operator would otherwise
        // copy, and a literal in a tracked file is the thing 4.7 forbids.
        // VALUE lines only. The prose above the tables names the deleted schemes
        // on purpose, to say they are deleted; a scan that could not tell a
        // comment from a value would forbid explaining the change.
        let raw = std::fs::read_to_string(&path).expect("read example");
        for line in raw
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .filter(|line| line.contains("urn:") || line.contains("arn:"))
        {
            assert!(
                !line.contains("urn:zeroship:env:")
                    && !line.contains("urn:zeroship:vault:")
                    && !line.contains("urn:zeroship:awssm:")
                    && !line.contains("arn:aws:secretsmanager:"),
                "the example still shows a deleted reference scheme: {line}"
            );
        }

        // Does NOT cover deploy/compose/docker-compose.yml, which still carries
        // comments describing the deleted [secrets] table. Rewriting deployment
        // inputs is Step 6 of the proposal and is sequenced separately.
    }

    #[test]
    fn load_auth_only_defaults_observability() {
        let file = TempFile::write(
            "auth-only.toml",
            r#"
[auth]
provider = "native"
"#,
        );

        let config = FileConfig::load(Some(&file.path)).expect("load config");

        assert_eq!(config.auth.provider.as_deref(), Some("native"));
        assert!(config.observability.log_filter.is_none());
        assert!(config.observability.log_format.is_none());
    }

    #[test]
    fn load_observability_only_defaults_auth() {
        let file = TempFile::write(
            "observability-only.toml",
            r#"
[observability]
log_filter = "debug"
"#,
        );

        let config = FileConfig::load(Some(&file.path)).expect("load config");

        assert!(config.auth.provider.is_none());
        assert!(config.auth.supabase_url.is_none());
        assert!(config.auth.supabase_anon_key.is_none());
        assert!(config.auth.platform_issuer.is_none());
        assert!(config.auth.platform_jwks_url.is_none());
        assert!(config.auth.trusted_oauth_clients.is_none());
        assert_eq!(config.observability.log_filter.as_deref(), Some("debug"));
    }

    #[test]
    fn malformed_toml_returns_parse_error() {
        let file = TempFile::write("malformed.toml", "[auth");

        let err = FileConfig::load(Some(&file.path)).expect_err("parse error");

        assert!(matches!(err, ConfigError::Parse { .. }));
        assert!(err.to_string().contains(file.path.to_str().expect("utf-8 path")));
    }

    #[test]
    fn invalid_topology_values_are_parse_errors() {
        for (name, raw) in [
            ("scheme", "origin_scheme = \"ftp\""),
            (
                "origin-path",
                "trusted_origins = [\"https://console.zeroship.ai/path\"]",
            ),
            ("origin-wildcard", "trusted_origins = [\"https://*.zeroship.ai\"]"),
        ] {
            let file = TempFile::write(name, raw);
            let err = FileConfig::load(Some(&file.path)).expect_err("invalid topology rejected");
            assert!(matches!(err, ConfigError::Parse { .. }));
        }
    }

    #[test]
    fn nonexistent_path_returns_io_error() {
        let path = std::env::temp_dir().join(format!(
            "zeroship-core-config-missing-{}",
            std::process::id()
        ));

        let err = FileConfig::load(Some(&path)).expect_err("io error");

        assert!(matches!(err, ConfigError::Io { .. }));
        assert!(err.to_string().contains(path.to_str().expect("utf-8 path")));
    }

    // S7: unknown keys now fail loudly instead of being silently ignored.
    #[test]
    fn deny_unknown_fields_in_auth_section_is_parse_error() {
        let file = TempFile::write(
            "unknown-auth-key.toml",
            r#"
[auth]
platform_issur = "https://typo.example"
"#,
        );

        let err = FileConfig::load(Some(&file.path)).expect_err("unknown key rejected");

        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    // M4: trusted_oauth_clients distinguishes absent / empty / populated.
    #[test]
    fn trusted_oauth_clients_absent_is_none() {
        let file = TempFile::write(
            "tcl-absent.toml",
            r#"
[auth]
provider = "native"
"#,
        );

        let config = FileConfig::load(Some(&file.path)).expect("load config");
        assert!(config.auth.trusted_oauth_clients.is_none());
    }

    #[test]
    fn trusted_oauth_clients_empty_is_some_empty() {
        let file = TempFile::write(
            "tcl-empty.toml",
            r#"
[auth]
trusted_oauth_clients = []
"#,
        );

        let config = FileConfig::load(Some(&file.path)).expect("load config");
        assert_eq!(
            config.auth.trusted_oauth_clients,
            Some(Vec::<String>::new())
        );
    }

    #[test]
    fn trusted_oauth_clients_populated_is_some_vec() {
        let file = TempFile::write(
            "tcl-populated.toml",
            r#"
[auth]
trusted_oauth_clients = ["a", "b"]
"#,
        );

        let config = FileConfig::load(Some(&file.path)).expect("load config");
        assert_eq!(
            config.auth.trusted_oauth_clients,
            Some(vec!["a".to_string(), "b".to_string()])
        );
    }

    // The `[secrets]` table itself is GONE, not merely emptied. Secrets now sit
    // with their siblings, so location encodes sharing; an overlay that still
    // carries the flat table must be told so at load rather than have its
    // credentials silently ignored. `deny_unknown_fields` at the ROOT is what
    // makes that a parse error.
    #[test]
    fn the_deleted_secrets_table_is_rejected_rather_than_ignored() {
        let file = TempFile::write(
            "legacy-secrets-table.toml",
            "[secrets]\nmaster_key = \"literal\"\n",
        );

        let err = FileConfig::load(Some(&file.path))
            .expect_err("a [secrets] table must be rejected, not ignored");
        assert!(matches!(err, ConfigError::Parse { .. }));

        // Does NOT cover a deployment file that still WRITES the table. That is
        // a tracked-tree search, and Step 6 owns it.
    }

    // A secret at its canonical path, as a LITERAL. Permitted on purpose: the
    // overlay may itself be a mounted Kubernetes Secret, and forbidding a
    // literal by file while permitting one by environment had no principled
    // basis. What is forbidden is a plaintext secret in a TRACKED file.
    #[test]
    fn a_secret_parses_at_its_canonical_path_beside_its_siblings() {
        let file = TempFile::write(
            "canonical-secrets.toml",
            r#"
control_key = "shared-literal"

[control]
port = 9090
database_url = "urn:zeroship:file:/run/secrets/control-dsn"
master_key = "literal-master"
"#,
        );

        let config = FileConfig::load(Some(&file.path)).expect("load config");
        assert_eq!(config.control_key.as_deref(), Some("shared-literal"));
        assert_eq!(config.control.port, Some(9090));
        assert_eq!(
            config.control.database_url.as_deref(),
            Some("urn:zeroship:file:/run/secrets/control-dsn")
        );
        assert_eq!(config.control.master_key.as_deref(), Some("literal-master"));

        // Does NOT cover RESOLUTION of either value; this is the parse layer.
        // Whether the file reference is followed is decided by the mode the
        // generated resolver runs in.
    }

    // A typo inside a component table is still a parse error now that secrets
    // live there: adding leaves must not have widened the schema.
    #[test]
    fn a_mistyped_secret_key_inside_a_component_table_is_a_parse_error() {
        let file = TempFile::write(
            "mistyped-secret.toml",
            "[control]\nmaser_key = \"x\"\n",
        );

        let err = FileConfig::load(Some(&file.path)).expect_err("unknown key rejected");
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    // The per-binary sections added for the operational conversion. Without
    // them `deny_unknown_fields` rejects the very overlay the generated
    // resolvers walk, so a canonical `[control] port = 9090` would be a startup
    // ERROR rather than the supported way to set it.
    #[test]
    fn the_per_binary_sections_parse_and_still_reject_a_typo_inside_them() {
        let file = TempFile::write(
            "per-binary-sections.toml",
            r#"
blob_store = "./bundles"
control_url = "http://control:9090"
worker_urls = "http://worker:8080"
poll_interval = 5
oauth_audience = "control.zeroship.ai"
trust_proxy = true

[control]
port = 9090
mailer = "stdout"

[gateway]
port = 80
db_pool_size = 16

[worker]
threads = 4
max_isolates = 200

[migrate_server]
port = 9091
mutation_rate_limit_burst = 2
mutation_rate_limit_per_minute = 3

[workflow_scheduler]
tick_secs = 1

[data_cdc_server]
database_url = "postgres://relay@db/zeroship"

[auth]
addr = "0.0.0.0:9092"
provider = "native"
smtp_port = 587
relay_smtp_tls = "starttls"
"#,
        );
        let config = FileConfig::load(Some(&file.path)).expect("canonical overlay parses");
        assert_eq!(config.control.port, Some(9090));
        assert_eq!(config.gateway.db_pool_size, Some(16));
        assert_eq!(config.worker.threads, Some(4));
        assert_eq!(config.migrate_server.port, Some(9091));
        assert_eq!(config.migrate_server.mutation_rate_limit_burst, Some(2));
        assert_eq!(
            config.migrate_server.mutation_rate_limit_per_minute,
            Some(3)
        );
        assert_eq!(config.workflow_scheduler.tick_secs, Some(1));
        assert_eq!(
            config.data_cdc_server.database_url.as_deref(),
            Some("postgres://relay@db/zeroship")
        );
        assert_eq!(config.auth.addr.as_deref(), Some("0.0.0.0:9092"));
        assert_eq!(config.auth.provider.as_deref(), Some("native"));
        assert_eq!(config.auth.smtp_port, Some(587));
        assert_eq!(config.trust_proxy, Some(true));

        // The one-variable control: the SAME file with one key misspelled must
        // still fail. Adding a section that accepted anything would be worse
        // than not adding it.
        let typo = TempFile::write(
            "per-binary-typo.toml",
            "[worker]\nthredas = 4\n",
        );
        let err = FileConfig::load(Some(&typo.path)).expect_err("a typo inside a section");
        assert!(matches!(err, ConfigError::Parse { .. }));

        // A bootstrap control has no overlay tier, so its key must NOT be
        // accepted here: accepted-then-ignored is the silent failure the
        // section exists to prevent.
        let control = TempFile::write(
            "per-binary-bootstrap.toml",
            "[worker]\nworkflow_advance_unsigned = true\n",
        );
        let err = FileConfig::load(Some(&control.path))
            .expect_err("a bootstrap control has no overlay tier");
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    // Split out from the case above because it asserts a REJECTION per key and a
    // loop of `expect_err` reads badly inline.
    #[test]
    fn an_auth_control_key_has_no_overlay_tier_and_is_refused_in_the_auth_table() {
        // Each value here is the NATURAL type for that control, so a section
        // that actually declared the field would PARSE and this would go red.
        // A deliberately ill-typed value would be rejected either way, which
        // proves nothing - and did not: an earlier draft wrote `= "x"` for all
        // four, and adding `pub check_config: Option<bool>` to the section left
        // the test green.
        for (key, value) in [
            ("config", "\"/etc/zeroship/zeroship.toml\""),
            ("no_config", "true"),
            ("check_config", "true"),
            ("check_config_format", "\"json\""),
        ] {
            let file = TempFile::write(
                &format!("auth-control-{key}"),
                &format!("[auth]\n{key} = {value}\n"),
            );
            let err = FileConfig::load(Some(&file.path))
                .err()
                .unwrap_or_else(|| panic!("[auth].{key} must be rejected"));
            let ConfigError::Parse { source, .. } = &err else {
                panic!("{key}: expected a parse error, got {err}");
            };
            assert!(
                source.to_string().contains("unknown field"),
                "{key} must be rejected AS AN UNKNOWN FIELD, not for its type: {source}"
            );
        }

        // The one-variable control: an ordinary operational key in the SAME
        // table parses. Without it, the loop above would also pass on an
        // `[auth]` section that rejected every key it was given.
        let ok = TempFile::write("auth-control-accepts", "[auth]\naddr = \"0.0.0.0:9092\"\n");
        let config = FileConfig::load(Some(&ok.path)).expect("an operational auth key parses");
        assert_eq!(config.auth.addr.as_deref(), Some("0.0.0.0:9092"));
    }
}
