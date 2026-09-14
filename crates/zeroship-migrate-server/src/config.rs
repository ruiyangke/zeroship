//! Migrated's command definition plus its generated controls.
//!
//! Both live in the LIBRARY so the compiled configuration checker can link them
//! and invoke clap's `CommandFactory`, matching control, gateway, worker and
//! auth. Until this module existed, migrated was the one platform service whose
//! parser was reachable only from `main.rs`, and the one with no overlay, no
//! `--check-config` and no shared boot dance at all.

use std::path::{Path, PathBuf};

use zeroship_core::config::{
    zeroship_config, BootstrapControl, CheckFormat, CommandControl, ObservabilityControls,
    Operational, OverlaySelector, Secret,
};
use zeroship_core::observability::LogFormat;

/// Tracing directive applied when nothing supplies `observability.log_filter`.
pub const DEFAULT_LOG_FILTER: &str = "info,zeroship_migrate_server=debug";

/// The controls every migration-service launch resolves before anything else.
#[zeroship_config(binary = "zeroship-migrate-server", scope = "migrate_server")]
#[derive(Debug)]
pub struct MigrateServerSettings {
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
    #[config(name = "migrate_server.port", default = 9091)]
    pub port: Operational<u16>,

    /// Address to bind.
    #[config(name = "migrate_server.bind", default = "127.0.0.1".to_owned())]
    pub bind: Operational<String>,

    /// Directory for staged request migration files.
    #[config(name = "migrate_server.tmp_dir", default = std::env::temp_dir().join("zeroship-migrate-server"))]
    pub tmp_dir: Operational<PathBuf>,

    /// Active managed ceiling version stamped into sealed migration profiles.
    #[config(name = "migrate_server.policy_ceiling_version", default = 1)]
    pub policy_ceiling_version: Operational<u64>,

    /// Trust the rightmost usable `X-Forwarded-For` address from one upstream proxy.
    #[arg(
        num_args = 0..=1,
        default_missing_value = "true",
        value_parser = zeroship_core::config::parse_bool_flag
    )]
    #[config(shared = TRUST_PROXY, default = false)]
    pub trust_proxy: Operational<bool>,

    /// Immediate per-source-IP mutation burst.
    ///
    /// Two permits one normal apply plus one immediate retry. Migration DDL is
    /// far rarer and more expensive than control's broad admin surface.
    #[config(name = "migrate_server.mutation_rate_limit_burst", default = 2)]
    pub mutation_rate_limit_burst: Operational<u32>,

    /// Steady per-source-IP mutations per minute after the burst is spent.
    ///
    /// Three means one token every 20 seconds, twenty times below control's
    /// 60-per-minute admin quota while still allowing a bounded recovery loop.
    #[config(name = "migrate_server.mutation_rate_limit_per_minute", default = 3)]
    pub mutation_rate_limit_per_minute: Operational<u32>,

    /// Expected OAuth audience for accepted bearer tokens.
    #[config(shared = OAUTH_AUDIENCE, default = "control.zeroship.ai".to_owned())]
    pub oauth_audience: Operational<String>,

    /// Platform OP issuer for platform-issued migration-service access tokens.
    #[config(shared = AUTH_PLATFORM_ISSUER, default = String::new())]
    pub auth_platform_issuer: Operational<String>,

    /// JWKS URL for the platform OP. Defaults to `{issuer}/.well-known/jwks.json`.
    #[config(shared = AUTH_PLATFORM_JWKS_URL, default = String::new())]
    pub auth_platform_jwks_url: Operational<String>,

    // Secrets last within the table, by convention. Each generates ONE
    // `--<name>-file` path flag and no value flag, so none of them can reach a
    // process argument list.
    /// `PostgreSQL` DSN for control-plane authz data.
    ///
    /// Secret-classed by grammar: a DSN admits userinfo, so the type cannot
    /// depend on whether a particular deployment's value happens to carry a
    /// password.
    #[config(name = "migrate_server.database_url")]
    pub database_url: Secret<String>,

    /// Privileged `PostgreSQL` DSN used to provision/apply per-app migrations.
    #[config(name = "migrate_server.provision_database_url")]
    pub provision_database_url: Secret<String>,

    /// Admin/control API shared secret.
    #[config(shared = CONTROL_KEY)]
    pub control_key: Secret<String>,

    /// HMAC key used to seal server-composed migration policy profiles.
    #[config(name = "migrate_server.policy_seal_key")]
    pub policy_seal_key: Secret<String>,
}

impl OverlaySelector for MigrateServerSettingsSources {
    fn overlay_path(&self) -> Option<&Path> {
        self.config.as_deref()
    }

    fn allow_discovery(&self) -> bool {
        !self.no_config
    }
}

impl ObservabilityControls for MigrateServerSettings {
    fn log_filter(&self) -> &str {
        self.log_filter.get()
    }

    fn log_format(&self) -> LogFormat {
        *self.log_format.get()
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::MigrateServerSettingsSources;

    #[test]
    fn migrated_now_carries_the_same_bootstrap_controls_as_its_siblings() {
        // Before this conversion migrated had no --config, no --no-config and
        // no --check-config at all, so no process test could exercise it.
        let sources = MigrateServerSettingsSources::try_parse_from([
            "zeroship-migrate-server",
            "--check-config",
            "--no-config",
            "--check-config-format",
            "json",
        ])
        .expect("controls parse");
        assert!(sources.check_config);
        assert!(sources.no_config);

        // Does not cover what main.rs then does with them; the crate's
        // check_config integration target runs the real binary.
    }

    #[test]
    fn migrated_cli_rejects_deleted_security_relaxation_flag() {
        let error = MigrateServerSettingsSources::try_parse_from([
            "zeroship-migrate-server",
            "--dev-insecure",
        ])
        .expect_err("deleted --dev-insecure flag must be rejected");
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn migrated_cli_accepts_explicit_edge_safety_controls() {
        MigrateServerSettingsSources::try_parse_from([
            "zeroship-migrate-server",
            "--trust-proxy",
            "--mutation-rate-limit-burst",
            "2",
            "--mutation-rate-limit-per-minute",
            "3",
        ])
        .expect("the edge-facing migration service must expose typed proxy and quota controls");
    }

    // Secrets get a PATH flag and no value flag, so material never reaches an
    // argument list. The negative half is the point: the value spellings that
    // existed before this conversion must be GONE, not merely discouraged.
    #[test]
    fn a_secret_has_a_path_flag_and_no_value_flag() {
        let sources = MigrateServerSettingsSources::try_parse_from([
            "zeroship-migrate-server",
            "--policy-seal-key-file",
            "/run/secrets/seal",
            "--control-key-file",
            "/run/secrets/control",
        ])
        .expect("path flags parse");
        assert_eq!(
            sources.policy_seal_key.as_deref(),
            Some(std::path::Path::new("/run/secrets/seal"))
        );
        assert_eq!(
            sources.control_key.as_deref(),
            Some(std::path::Path::new("/run/secrets/control"))
        );

        for value_flag in [
            "--policy-seal-key",
            "--control-key",
            "--db",
            "--provision-db",
        ] {
            let error = MigrateServerSettingsSources::try_parse_from([
                "zeroship-migrate-server",
                value_flag,
                "x",
            ])
            .expect_err("a secret value flag must not exist");
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::UnknownArgument,
                "{value_flag} still parses"
            );
        }

        // Does NOT cover whether the paths are ever READ. Under --check-config
        // they must not be, and that is asserted in core against the resolver
        // and by the crate's check_config integration target.
    }
}
