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
pub struct MigratedControls {
    /// Optional shared config overlay path.
    #[config(name = "config")]
    pub config: BootstrapControl<Option<PathBuf>>,

    /// Disable auto-discovery of the well-known config overlay (compiled defaults only).
    #[config(name = "no_config")]
    pub no_config: BootstrapControl<bool>,

    /// Validate config (CLI + overlay + guards) and print the resolved non-secret
    /// config, then exit without provisioning, connecting or listening.
    #[config(name = "check_config")]
    pub check_config: CommandControl<bool>,

    /// Output format for `--check-config`.
    #[config(name = "check_config_format", default = CheckFormat::Text)]
    pub check_config_format: CommandControl<CheckFormat>,

    /// `EnvFilter` directive for the tracing subscriber.
    #[config(name = "observability.log_filter", default = DEFAULT_LOG_FILTER.to_owned())]
    pub log_filter: Operational<String>,

    /// Tracing output format; `auto` picks pretty on a TTY and json otherwise.
    #[config(name = "observability.log_format", default = LogFormat::Auto)]
    pub log_format: Operational<LogFormat>,
}

impl OverlaySelector for MigratedControlsSources {
    fn overlay_path(&self) -> Option<&Path> {
        self.config.as_deref()
    }

    fn allow_discovery(&self) -> bool {
        !self.no_config
    }
}

impl ObservabilityControls for MigratedControls {
    fn log_filter(&self) -> &str {
        self.log_filter.get()
    }

    fn log_format(&self) -> LogFormat {
        *self.log_format.get()
    }
}

/// zeroship-migrated startup configuration.
///
/// No `#[derive(Debug)]`: this struct holds raw secrets (`db`, `provision_db`,
/// `control_key`, `policy_seal_key`) before they are consumed.
#[derive(Parser)]
#[command(name = "zeroship-migrated")]
pub struct MigratedCli {
    /// HTTP listen port.
    #[arg(long, env = "MIGRATED_PORT", default_value_t = 9091)]
    pub port: u16,

    /// Address to bind.
    #[arg(long, env = "MIGRATED_BIND", default_value = "127.0.0.1")]
    pub bind: String,

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

    /// Expected OAuth audience for accepted bearer tokens.
    #[arg(
        long = "oauth-audience",
        env = "CONTROL_OAUTH_AUDIENCE",
        default_value = "control.zeroship.ai"
    )]
    pub oauth_audience: String,

    /// Platform OP issuer for platform-issued migration-service access tokens.
    #[arg(
        long = "auth-platform-issuer",
        env = "AUTH_PLATFORM_ISSUER",
        default_value = ""
    )]
    pub auth_platform_issuer: String,

    /// JWKS URL for the platform OP. Defaults to {issuer}/.well-known/jwks.json.
    #[arg(
        long = "auth-platform-jwks-url",
        env = "AUTH_PLATFORM_JWKS_URL",
        default_value = ""
    )]
    pub auth_platform_jwks_url: String,

    /// Directory for staged request migration files.
    #[arg(long = "tmp-dir", env = "MIGRATED_TMP_DIR")]
    pub tmp_dir: Option<PathBuf>,

    /// HMAC key used to seal server-composed migration policy profiles.
    #[arg(
        long = "policy-seal-key",
        env = "MIGRATED_POLICY_SEAL_KEY",
        default_value = "",
        hide_env_values = true
    )]
    pub policy_seal_key: String,

    /// Active managed ceiling version stamped into sealed migration profiles.
    #[arg(
        long = "policy-ceiling-version",
        env = "MIGRATED_POLICY_CEILING_VERSION",
        default_value_t = 1
    )]
    pub policy_ceiling_version: u64,

    /// Bootstrap, command and observability controls, generated from one
    /// declaration above.
    #[command(flatten)]
    pub controls: MigratedControlsSources,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{MigratedCli, MigratedControlsSources};

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
        assert!(cli.controls.check_config);
        assert!(cli.controls.no_config);

        // Does not cover what main.rs then does with them; that is asserted end
        // to end by tests/config_check_e2e.sh, which runs the real binary.
    }

    #[test]
    fn migrated_cli_rejects_deleted_security_relaxation_flag() {
        let error = MigratedControlsSources::try_parse_from([
            "zeroship-migrated",
            "--dev-insecure",
        ])
        .expect_err("deleted --dev-insecure flag must be rejected");
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }
}
