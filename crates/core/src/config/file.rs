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
    /// Secret-reference overlay: optional `urn:`/`arn:` references for each
    /// platform secret. Every field is reference-only (a literal is rejected at
    /// resolve); absent fields fall back to CLI/env/default.
    #[serde(default)]
    pub secrets: SecretSection,
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
    pub migrated: MigratedSection,
    /// Standalone workflow-scheduler settings.
    #[serde(default)]
    pub workflow_scheduler: SchedulerSection,
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
    /// Audit retention horizon in months.
    pub audit_retention_months: Option<u32>,
    /// Audit retention cron tick in seconds.
    pub audit_retention_check_secs: Option<u64>,
}

/// Gateway operational values supplied by the overlay. See [`ControlSection`].
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct GatewaySection {
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
pub struct MigratedSection {
    /// HTTP listen port.
    pub port: Option<u16>,
    /// Bind address.
    pub bind: Option<String>,
    /// Directory for staged request migration files.
    pub tmp_dir: Option<std::path::PathBuf>,
    /// Active managed ceiling version stamped into sealed profiles.
    pub policy_ceiling_version: Option<u64>,
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

/// Usage-metering stream configuration supplied by the shared file overlay.
/// These back-fill the corresponding environment variables (env wins) so the
/// billing stream can be configured entirely from `zeroship.toml`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct MeteringSection {
    /// Kafka-wire broker list for the usage-event stream, e.g.
    /// `"redpanda:9092"` (env `REDPANDA_BROKERS`). Unset ⇒ the usage producer is
    /// disabled (drain-and-drop).
    pub redpanda_brokers: Option<String>,
    /// Usage-event topic (env `USAGE_EVENTS_TOPIC`, default `"usage-events"`).
    pub usage_events_topic: Option<String>,
    /// Producer consumer-group id override (env `REDPANDA_PRODUCER_GROUP_ID`).
    pub producer_group_id: Option<String>,
    /// redb WAL path for the outbox (env `USAGE_OUTBOX_WAL_PATH`).
    pub outbox_wal_path: Option<String>,
}

/// Auth-domain values that can be supplied by the shared file overlay.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AuthSection {
    /// Platform auth provider backend (`platform` or `supabase`).
    pub auth_provider: Option<String>,
    /// Supabase Auth / GoTrue base URL used when the auth provider is Supabase.
    pub supabase_url: Option<String>,
    /// Supabase anon API key used by browser-side GoTrue session calls.
    pub supabase_anon_key: Option<String>,
    /// Platform OP issuer accepted for platform-issued access tokens.
    pub platform_issuer: Option<String>,
    /// Platform OP JWKS URL. Defaults to `{platform_issuer}/.well-known/jwks.json`.
    pub platform_jwks_url: Option<String>,
    /// Control-plane base URL used by auth-service browser flows.
    pub control_url: Option<String>,
    /// First-party OAuth client IDs trusted by the platform.
    ///
    /// `None` (key absent) means "use the compiled-in default set"; `Some(vec)`
    /// means exactly that set, where an empty vec is "no trusted clients".
    pub trusted_oauth_clients: Option<Vec<String>>,
    /// Console origin(s) the auth-service login/signup/consent documents admit
    /// via CSP `frame-ancestors` so the console's immersive iframe login can
    /// embed them (design §4.3/§10.1). Deployment-injected, mirroring the
    /// `trusted_oauth_clients` pattern: core has no console host. `None` (key
    /// absent) ⇒ the CLI/env tier (`--frame-ancestor-origin` /
    /// `FRAME_ANCESTOR_ORIGINS`) decides; `Some(vec)` supplies the overlay tier
    /// when the CLI/env is empty. EXACT origins only — NO wildcards.
    pub frame_ancestor_origins: Option<Vec<String>>,
}

/// Secret references that can be supplied by the shared file overlay.
///
/// Every field is an OPTIONAL secret REFERENCE (`urn:`/`arn:`). Absent => the
/// secret comes from CLI/env/default. A literal value here is rejected at resolve
/// by [`crate::config::secrets::obtain_secret`]: the config file must never carry
/// a plaintext secret.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SecretSection {
    /// Bundle/master encryption key reference.
    pub master_key: Option<String>,
    /// Control-plane shared secret reference.
    pub control_key: Option<String>,
    /// Worker shared secret reference.
    pub worker_key: Option<String>,
    /// Stash signing key reference.
    pub stash_signing_key: Option<String>,
    /// Dedicated pairwise-salt secret reference (auth-sdk §6.2). The PERMANENT
    /// per-app `pws_` identity anchor seed — independent of the stash key,
    /// never rotated without a migration. Must be identical on gateway+control.
    pub pairwise_salt: Option<String>,
    /// Gateway OIDC relying-party client secret reference.
    pub gateway_oidc_secret: Option<String>,
    /// Stripe webhook signing secret reference.
    pub stripe_webhook_secret: Option<String>,
    /// Stripe secret API key (`sk_…`) reference — for OUTBOUND calls (the
    /// billing reconciler + `billing/setup`).
    pub stripe_secret_key: Option<String>,
    /// Primary database URL reference.
    pub database_url: Option<String>,
    /// Privileged provisioning database URL reference (control only). The
    /// CREATEROLE + CREATE-on-db admin role deploy-time migrations use to
    /// create the per-app schema + `migrator_<app_id>` role — SEPARATE from
    /// the least-privilege `database_url` (`zeroship_control`), which has
    /// neither privilege. Resolved into control's `--provision-db`.
    pub provision_db_url: Option<String>,
    /// Auth database URL reference.
    pub auth_db_url: Option<String>,
    /// App-runtime KV (Redis) connection URL reference. Back-fills the
    /// worker's `--kv-url` / `ZEROSHIP_KV_URL` when those are empty; powers
    /// the deployed app `env.kv` namespace. May carry credentials, so it is
    /// a reference here (never a plaintext URL).
    pub kv_url: Option<String>,
    /// Legacy master keys (for key rotation) reference.
    pub legacy_master_keys: Option<String>,
    /// Google OAuth client secret reference.
    pub google_client_secret: Option<String>,
    /// GitHub OAuth client secret reference.
    pub github_client_secret: Option<String>,
    /// SMTP password reference.
    pub smtp_password: Option<String>,
    /// Resend API key reference.
    pub resend_api_key: Option<String>,
    /// Postmark inbound webhook basic-auth password reference.
    pub postmark_webhook_password: Option<String>,
    /// TOTP at-rest encryption key reference (ISS-11). AES-256-GCM key material
    /// for the auth service's `zeroship.totp_credentials.encrypted_secret`;
    /// must decode (hex or base64url) to ≥32 bytes. Dedicated key, independent
    /// of the stash/pairwise secrets.
    pub totp_enc_key: Option<String>,
}

/// Observability values that can be supplied by the shared file overlay.
///
/// These fields exist so `deny_unknown_fields` still accepts an
/// `[observability]` table and rejects a typo inside it. The VALUES each binary
/// uses come from the generated `observability.log_filter` /
/// `observability.log_format` declarations, which walk the same overlay by
/// canonical path; the key spellings here and there are therefore the same by
/// construction, and both parse `log_format` into a [`LogFormat`].
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

        assert!(config.auth.auth_provider.is_none());
        assert!(config.origin_scheme.is_none());
        assert!(config.trusted_origins.is_none());
        assert!(config.auth.supabase_url.is_none());
        assert!(config.auth.supabase_anon_key.is_none());
        assert!(config.auth.platform_issuer.is_none());
        assert!(config.auth.platform_jwks_url.is_none());
        assert!(config.auth.control_url.is_none());
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
auth_provider = "supabase"
supabase_url = "https://project.supabase.test"
supabase_anon_key = "anon-test-key"
platform_issuer = "https://auth.zeroship.ai"
platform_jwks_url = "https://auth.zeroship.ai/.well-known/jwks.json"
control_url = "https://control.zeroship.ai"
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

        assert_eq!(config.auth.auth_provider.as_deref(), Some("supabase"));
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
            config.auth.control_url.as_deref(),
            Some("https://control.zeroship.ai")
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

    #[test]
    fn load_auth_only_defaults_observability() {
        let file = TempFile::write(
            "auth-only.toml",
            r#"
[auth]
auth_provider = "platform"
"#,
        );

        let config = FileConfig::load(Some(&file.path)).expect("load config");

        assert_eq!(config.auth.auth_provider.as_deref(), Some("platform"));
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

        assert!(config.auth.auth_provider.is_none());
        assert!(config.auth.supabase_url.is_none());
        assert!(config.auth.supabase_anon_key.is_none());
        assert!(config.auth.platform_issuer.is_none());
        assert!(config.auth.platform_jwks_url.is_none());
        assert!(config.auth.control_url.is_none());
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
auth_provider = "platform"
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

    // [secrets] absent => all-None section (defaults).
    #[test]
    fn secrets_section_absent_is_all_none() {
        let config = FileConfig::load(None).expect("load default config");
        assert!(config.secrets.master_key.is_none());
        assert!(config.secrets.database_url.is_none());
        assert!(config.secrets.resend_api_key.is_none());
    }

    // [secrets] parses a reference value into the matching field.
    #[test]
    fn secrets_section_parses_reference() {
        let file = TempFile::write(
            "secrets.toml",
            r#"
[secrets]
master_key = "urn:zeroship:vault:secret/x"
database_url = "urn:zeroship:env:DATABASE_URL"
stripe_webhook_secret = "arn:aws:secretsmanager:us-east-1:123:secret:whsec"
"#,
        );

        let config = FileConfig::load(Some(&file.path)).expect("load config");
        assert_eq!(
            config.secrets.master_key.as_deref(),
            Some("urn:zeroship:vault:secret/x")
        );
        assert_eq!(
            config.secrets.database_url.as_deref(),
            Some("urn:zeroship:env:DATABASE_URL")
        );
        assert_eq!(
            config.secrets.stripe_webhook_secret.as_deref(),
            Some("arn:aws:secretsmanager:us-east-1:123:secret:whsec")
        );
        // Unmentioned fields stay None.
        assert!(config.secrets.control_key.is_none());
    }

    // deny_unknown_fields on [secrets]: an unknown key is a parse error, not a
    // silent ignore.
    #[test]
    fn deny_unknown_fields_in_secrets_section_is_parse_error() {
        let file = TempFile::write(
            "unknown-secret-key.toml",
            r#"
[secrets]
maser_key = "urn:zeroship:vault:secret/x"
"#,
        );

        let err = FileConfig::load(Some(&file.path)).expect_err("unknown key rejected");
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    // Immersive-login pivot guardrail (design §4.5/§9, no-back-compat): the
    // deleted `auth_internal_key` shared secret. A deployment TOML still
    // carrying `[secrets].auth_internal_key` must now FAIL to parse via
    // `deny_unknown_fields` — there is no silent-ignore arm, exactly so a stale
    // overlay surfaces loudly rather than the operator believing the (gone)
    // credential oracle is still gated. This test would PASS before the field
    // removal (the key parsed) and FAILs to compile/parse-reject only after.
    #[test]
    fn deny_removed_auth_internal_key_in_secrets_section_is_parse_error() {
        let file = TempFile::write(
            "removed-auth-internal-key.toml",
            r#"
[secrets]
auth_internal_key = "urn:zeroship:env:AUTH_INTERNAL_KEY"
"#,
        );

        let err = FileConfig::load(Some(&file.path))
            .expect_err("[secrets].auth_internal_key must be rejected (field deleted)");
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

[migrated]
port = 9091

[workflow_scheduler]
tick_secs = 1
"#,
        );
        let config = FileConfig::load(Some(&file.path)).expect("canonical overlay parses");
        assert_eq!(config.control.port, Some(9090));
        assert_eq!(config.gateway.db_pool_size, Some(16));
        assert_eq!(config.worker.threads, Some(4));
        assert_eq!(config.migrated.port, Some(9091));
        assert_eq!(config.workflow_scheduler.tick_secs, Some(1));
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
}
