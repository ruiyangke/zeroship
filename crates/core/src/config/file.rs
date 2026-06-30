//! File-overlay schema: the TOML sections and their load/parse error type.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

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

    /// An observability `log_format` value from the file overlay was not a
    /// recognised format.
    #[error("invalid log_format {value:?}; expected one of pretty, compact, json, logfmt, bunyan")]
    InvalidLogFormat {
        /// The offending value as written in the file.
        value: String,
    },
}

/// Optional cross-binary domain configuration loaded from `ops/zeroship.toml`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    /// Auth-domain configuration shared by binaries that integrate with Hydra.
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
}

/// Auth-domain values that can be supplied by the shared file overlay.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AuthSection {
    /// Hydra admin API base URL.
    pub hydra_admin_url: Option<String>,
    /// Hydra public issuer/base URL.
    pub hydra_public_url: Option<String>,
    /// Platform auth provider backend (`hydra` or `supabase`).
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
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ObsSection {
    /// `RUST_LOG` / `EnvFilter` directive.
    #[serde(rename = "rust_log")]
    pub log_filter: Option<String>,
    /// Tracing output format, as a raw string (TOML carries strings; it is
    /// parsed into a `LogFormat` by `resolve_observability`, which errors on
    /// invalid values).
    pub log_format: Option<String>,
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
        let Some(path) = path else {
            return Ok(Self::default());
        };

        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
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

        assert!(config.auth.hydra_admin_url.is_none());
        assert!(config.auth.hydra_public_url.is_none());
        assert!(config.auth.auth_provider.is_none());
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
[auth]
hydra_admin_url = "http://hydra:4445"
hydra_public_url = "https://auth.zeroship.ai"
auth_provider = "supabase"
supabase_url = "https://project.supabase.test"
supabase_anon_key = "anon-test-key"
platform_issuer = "https://auth.zeroship.ai"
platform_jwks_url = "https://auth.zeroship.ai/.well-known/jwks.json"
control_url = "https://control.zeroship.ai"
trusted_oauth_clients = ["zeroship-builder", "zeroship-console"]

[observability]
rust_log = "info,zeroship_=debug"
log_format = "json"
"#,
        );

        let config = FileConfig::load(Some(&file.path)).expect("load config");

        assert_eq!(
            config.auth.hydra_admin_url.as_deref(),
            Some("http://hydra:4445")
        );
        assert_eq!(
            config.auth.hydra_public_url.as_deref(),
            Some("https://auth.zeroship.ai")
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
        assert_eq!(config.observability.log_format.as_deref(), Some("json"));
    }

    #[test]
    fn load_ops_zeroship_toml_parses() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ops/zeroship.toml");

        let config = FileConfig::load(Some(&path)).expect("load ops/zeroship.toml");

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
hydra_admin_url = "http://hydra:4445"
"#,
        );

        let config = FileConfig::load(Some(&file.path)).expect("load config");

        assert_eq!(
            config.auth.hydra_admin_url.as_deref(),
            Some("http://hydra:4445")
        );
        assert!(config.observability.log_filter.is_none());
        assert!(config.observability.log_format.is_none());
    }

    #[test]
    fn load_observability_only_defaults_auth() {
        let file = TempFile::write(
            "observability-only.toml",
            r#"
[observability]
rust_log = "debug"
"#,
        );

        let config = FileConfig::load(Some(&file.path)).expect("load config");

        assert!(config.auth.hydra_admin_url.is_none());
        assert!(config.auth.hydra_public_url.is_none());
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
hydra_pubic_url = "https://typo.example"
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
hydra_admin_url = "http://hydra:4445"
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
}
