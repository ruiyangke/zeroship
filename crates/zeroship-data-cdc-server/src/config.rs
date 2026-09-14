//! Configuration for the separately deployed PostgreSQL CDC relay.

use std::path::{Path, PathBuf};

use zeroship_core::config::{
    zeroship_config, BootstrapControl, CheckFormat, CommandControl, ObservabilityControls,
    Operational, OverlaySelector, Secret,
};
use zeroship_core::observability::LogFormat;

/// Tracing directive applied when nothing supplies `observability.log_filter`.
pub const DEFAULT_LOG_FILTER: &str = "info,zeroship_data_cdc_server=debug";

/// Every control the CDC relay resolves before anything else.
#[zeroship_config(binary = "zeroship-data-cdc-server", scope = "data_cdc_server")]
#[derive(Debug)]
pub struct CdcServerSettings {
    /// Optional shared config overlay path.
    #[config(shared = CONFIG)]
    pub config: BootstrapControl<Option<PathBuf>>,

    /// Disable auto-discovery of the well-known config overlay
    /// (`/etc/zeroship/zeroship.toml`); use compiled defaults instead.
    #[config(shared = NO_CONFIG)]
    pub no_config: BootstrapControl<bool>,

    /// Validate config (CLI + overlay) and print the resolved non-secret config,
    /// then exit without dialling the database.
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

    /// TLS listener address.
    #[config(name = "data_cdc_server.listen", default = "127.0.0.1:9094".to_owned())]
    pub listen: Operational<String>,
    /// PEM certificate chain for the relay endpoint.
    #[config(name = "data_cdc_server.tls_cert_file", default = PathBuf::new())]
    pub tls_cert_file: Operational<PathBuf>,
    /// PEM private key for the relay endpoint.
    #[config(name = "data_cdc_server.tls_key_file", default = PathBuf::new())]
    pub tls_key_file: Operational<PathBuf>,
    /// Maximum concurrent app capture tasks.
    #[config(name = "data_cdc_server.max_apps", default = 64)]
    pub max_apps: Operational<usize>,
    /// Maximum accepted transport connections, including pending authentication.
    #[config(name = "data_cdc_server.max_connections", default = 1024)]
    pub max_connections: Operational<usize>,
    /// Maximum subscriptions sharing an app capture task.
    #[config(name = "data_cdc_server.clients_per_app", default = 128)]
    pub clients_per_app: Operational<usize>,
    /// Pending events per transport connection before disconnect and resync.
    #[config(name = "data_cdc_server.queue_capacity", default = 128)]
    pub queue_capacity: Operational<usize>,
    /// Maximum retained transaction bytes before commit becomes a resync.
    #[config(name = "data_cdc_server.transaction_bytes", default = 8 * 1024 * 1024)]
    pub transaction_bytes: Operational<usize>,
    /// Maximum retained transaction changes before commit becomes a resync.
    #[config(name = "data_cdc_server.transaction_changes", default = 10000)]
    pub transaction_changes: Operational<usize>,
    /// Maximum relation cache entries per capture task.
    #[config(name = "data_cdc_server.max_relations", default = 4096)]
    pub max_relations: Operational<usize>,
    /// Replication login DSN. The role must also read enrolled worker public keys.
    #[config(name = "data_cdc_server.database_url")]
    pub database_url: Secret<String>,
}

impl OverlaySelector for CdcServerSettingsSources {
    fn overlay_path(&self) -> Option<&Path> {
        self.config.as_deref()
    }

    fn allow_discovery(&self) -> bool {
        !self.no_config
    }
}

impl ObservabilityControls for CdcServerSettings {
    fn log_filter(&self) -> &str {
        self.log_filter.get()
    }

    fn log_format(&self) -> LogFormat {
        *self.log_format.get()
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};
    use zeroship_core::config::GeneratedConfig;

    use super::{CdcServerSettings, CdcServerSettingsSources};

    #[test]
    fn the_relay_carries_the_same_bootstrap_controls_as_its_siblings() {
        // The failure this guards: a platform binary that cannot be dry-run
        // would otherwise be invisible until deployment.
        let sources = CdcServerSettingsSources::try_parse_from([
            "zeroship-data-cdc-server",
            "--check-config",
            "--no-config",
            "--check-config-format",
            "json",
        ])
        .expect("controls parse");
        assert!(sources.check_config);
        assert!(sources.no_config);
    }

    // The secret gets a PATH flag and no value flag, so material never reaches
    // an argument list. The negative half is the point: a DSN value flag must
    // not exist at all, not merely be discouraged.
    #[test]
    fn the_streaming_dsn_has_a_path_flag_and_no_value_flag() {
        let sources = CdcServerSettingsSources::try_parse_from([
            "zeroship-data-cdc-server",
            "--database-url-file",
            "/run/secrets/cdc-dsn",
        ])
        .expect("path flag parses");
        assert_eq!(
            sources.database_url.as_deref(),
            Some(std::path::Path::new("/run/secrets/cdc-dsn"))
        );

        for value_flag in ["--database-url", "--db", "--dsn"] {
            let error = CdcServerSettingsSources::try_parse_from([
                "zeroship-data-cdc-server",
                value_flag,
                "postgres://x",
            ])
            .expect_err("a secret value flag must not exist");
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::UnknownArgument,
                "{value_flag} still parses"
            );
        }

        // Does NOT cover whether the path is ever READ. Under --check-config it
        // must not be; core asserts that against the resolver.
    }

    #[test]
    fn every_declared_name_is_a_canonical_projection() {
        // A secret carries no `env` on its clap carrier - putting one there
        // would give a secret a clap-visible value source - so deriving the name
        // set from the Command alone cannot see it. Both sources are read here
        // for that reason.
        let from_clap = CdcServerSettingsSources::command()
            .get_arguments()
            .filter_map(|arg| arg.get_env().map(|env| env.to_string_lossy().into_owned()))
            .collect::<Vec<_>>();
        let from_specs = CdcServerSettings::SPECS
            .iter()
            .filter_map(|spec| spec.env_name())
            .collect::<Vec<String>>();
        assert!(
            !from_clap.is_empty() && !from_specs.is_empty(),
            "an empty name set would make the loop below vacuous"
        );
        assert!(from_specs.contains(&"ZEROSHIP_DATA_CDC_SERVER_DATABASE_URL".to_owned()));
        for env in from_clap.iter().chain(from_specs.iter()) {
            assert!(
                env.starts_with("ZEROSHIP_"),
                "{env} is not a canonical projection"
            );
        }
    }

    #[test]
    fn the_overlay_supplies_the_relay_scope_and_the_flag_overrides_it() {
        let overlay: toml::Value =
            toml::from_str("[observability]\nlog_filter = \"warn\"\n").expect("fixture overlay");

        let resolved = CdcServerSettings::resolve_config(
            CdcServerSettingsSources::try_parse_from(["zeroship-data-cdc-server"])
                .expect("bare parse"),
            Some(&overlay),
        )
        .expect("settings resolve");
        assert_eq!(resolved.log_filter.get(), "warn");

        let flagged = CdcServerSettings::resolve_config(
            CdcServerSettingsSources::try_parse_from([
                "zeroship-data-cdc-server",
                "--observability-log-filter",
                "trace",
            ])
            .expect("flag parse"),
            Some(&overlay),
        )
        .expect("settings resolve");
        assert_eq!(
            flagged.log_filter.get(),
            "trace",
            "the flag must win over the overlay"
        );

        // Does not cover the environment tier: clap merges it into the same
        // carrier, and a process-wide env mutation would race sibling tests.
    }
}
