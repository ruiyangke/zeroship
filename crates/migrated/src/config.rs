//! Migrated's command definition plus its generated controls.
//!
//! Both live in the LIBRARY so the compiled configuration checker can link them
//! and invoke clap's `CommandFactory`, matching control, gateway, worker and
//! auth. Until this module existed, migrated was the one platform service whose
//! parser was reachable only from `main.rs`, and the one with no overlay, no
//! `--check-config` and no shared boot dance at all.

use std::path::{Path, PathBuf};

use clap::Parser;
use zeroship_core::config::{
    zeroship_config, BootstrapControl, CheckFormat, CommandControl, ObservabilityControls,
    Operational, OverlaySelector,
};
use zeroship_core::observability::LogFormat;

/// Tracing directive applied when nothing supplies `observability.log_filter`.
pub const DEFAULT_LOG_FILTER: &str = "info,zeroship_migrated=debug";

/// The controls every migration-service launch resolves before anything else.
#[zeroship_config(binary = "zeroship-migrated", scope = "migrated")]
#[derive(Debug)]
pub struct MigratedSettings {
    /// Optional shared config overlay path.
    #[config(shared = CONFIG)]
    pub config: BootstrapControl<Option<PathBuf>>,

    /// Disable auto-discovery of the well-known config overlay (compiled defaults only).
    #[config(shared = NO_CONFIG)]
    pub no_config: BootstrapControl<bool>,

    /// Validate config (CLI + overlay + guards) and print the resolved non-secret
    /// config, then exit without provisioning, connecting or listening.
    #[config(shared = CHECK_CONFIG)]
    pub check_config: CommandControl<bool>,

    /// Output format for `--check-config`.
    #[config(shared = CHECK_CONFIG_FORMAT, default = CheckFormat::Text)]
    pub check_config_format: CommandControl<CheckFormat>,

    /// `EnvFilter` directive for the tracing subscriber.
    #[config(shared = OBSERVABILITY_LOG_FILTER, default = DEFAULT_LOG_FILTER.to_owned())]
    pub log_filter: Operational<String>,

    /// Tracing output format; `auto` picks pretty on a TTY and json otherwise.
    #[config(shared = OBSERVABILITY_LOG_FORMAT, default = LogFormat::Auto)]
    pub log_format: Operational<LogFormat>,

    /// HTTP listen port.
    #[config(name = "migrated.port", default = 9091)]
    pub port: Operational<u16>,

    /// Address to bind.
    #[config(name = "migrated.bind", default = "127.0.0.1".to_owned())]
    pub bind: Operational<String>,

    /// Directory for staged request migration files.
    #[config(name = "migrated.tmp_dir", default = std::env::temp_dir().join("zeroship-migrated"))]
    pub tmp_dir: Operational<PathBuf>,

    /// Active managed ceiling version stamped into sealed migration profiles.
    #[config(name = "migrated.policy_ceiling_version", default = 1)]
    pub policy_ceiling_version: Operational<u64>,

    /// Expected OAuth audience for accepted bearer tokens.
    #[config(shared = OAUTH_AUDIENCE, default = "control.zeroship.ai".to_owned())]
    pub oauth_audience: Operational<String>,

    /// Platform OP issuer for platform-issued migration-service access tokens.
    #[config(shared = AUTH_PLATFORM_ISSUER, default = String::new())]
    pub auth_platform_issuer: Operational<String>,

    /// JWKS URL for the platform OP. Defaults to `{issuer}/.well-known/jwks.json`.
    #[config(shared = AUTH_PLATFORM_JWKS_URL, default = String::new())]
    pub auth_platform_jwks_url: Operational<String>,
}

impl OverlaySelector for MigratedSettingsSources {
    fn overlay_path(&self) -> Option<&Path> {
        self.config.as_deref()
    }

    fn allow_discovery(&self) -> bool {
        !self.no_config
    }
}

impl ObservabilityControls for MigratedSettings {
    fn log_filter(&self) -> &str {
        self.log_filter.get()
    }

    fn log_format(&self) -> LogFormat {
        *self.log_format.get()
    }
}

/// zeroship-migrated startup configuration.
///
/// Only the credential-bearing fields remain; every operational value is
/// generated above. No `#[derive(Debug)]`: what is left IS the raw-secret set.
#[derive(Parser)]
#[command(name = "zeroship-migrated")]
pub struct MigratedCli {
    /// PostgreSQL DSN for control-plane authz data.
    #[arg(
        long = "db",
        env = "DATABASE_URL",
        default_value = "postgres://localhost/zeroship",
        hide_env_values = true
    )]
    pub db: String,

    /// Privileged PostgreSQL DSN used to provision/apply per-app migrations.
    #[arg(
        long = "provision-db",
        env = "PROVISION_DATABASE_URL",
        default_value = "",
        hide_env_values = true
    )]
    pub provision_db: String,

    /// Admin/control API shared secret, reserved for convergence with the service mesh wiring.
    #[arg(long = "control-key", env = "CONTROL_KEY", default_value = "", hide_env_values = true)]
    pub control_key: String,

    /// PEM/PKCS#8 signing key file for PAT verification.
    #[arg(long = "signing-key-file", env = "SIGNING_KEY_FILE", default_value = "")]
    pub signing_key_file: String,

    /// HMAC key used to seal server-composed migration policy profiles.
    #[arg(
        long = "policy-seal-key",
        env = "MIGRATED_POLICY_SEAL_KEY",
        default_value = "",
        hide_env_values = true
    )]
    pub policy_seal_key: String,

    /// Every operational value, generated from one declaration above.
    #[command(flatten)]
    pub settings: MigratedSettingsSources,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{MigratedCli, MigratedSettingsSources};

    #[test]
    fn migrated_now_carries_the_same_bootstrap_controls_as_its_siblings() {
        // Before this conversion migrated had no --config, no --no-config and
        // no --check-config at all, which is why tests/config_check_e2e.sh
        // could not exercise it.
        let cli = MigratedCli::try_parse_from([
            "zeroship-migrated",
            "--check-config",
            "--no-config",
            "--check-config-format",
            "json",
        ])
        .expect("controls parse");
        assert!(cli.settings.check_config);
        assert!(cli.settings.no_config);

        // Does not cover what main.rs then does with them; that is asserted end
        // to end by tests/config_check_e2e.sh, which runs the real binary.
    }

    #[test]
    fn migrated_cli_rejects_deleted_security_relaxation_flag() {
        let error = MigratedSettingsSources::try_parse_from([
            "zeroship-migrated",
            "--dev-insecure",
        ])
        .expect_err("deleted --dev-insecure flag must be rejected");
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }
}
