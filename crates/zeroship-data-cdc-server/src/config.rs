//! The relay's command definition plus its generated controls.
//!
//! In the LIBRARY rather than `main.rs` so the compiled configuration checker
//! can link it and invoke clap's `CommandFactory`, matching the other six
//! platform binaries. That placement is not stylistic: `platform_specs()` in
//! `crates/zeroship-config-contract/src/registry.rs` reads
//! `CdcServerSettings::SPECS` off the lib, so a `main.rs`-only parser is
//! invisible to every check that tool performs.
//!
//! # Why a converted configuration for a process that refuses to start
//!
//! The same reason `zeroship-workflow-scheduler` converted before its dispatch
//! loop was extracted: the Cargo target is classified `platform`, which the
//! configuration design makes a REQUIREMENT to register rather than a judgement
//! call, and an operator-visible surface is visible whether or not the process
//! it configures currently runs.
//!
//! # Why the table is this short
//!
//! Every value here is one the relay cannot be without, and nothing here
//! anticipates a protocol. There is no bind address and no port: this crate
//! serves no endpoint, and declaring a listener's configuration would document
//! a surface that does not exist. The one non-boilerplate leaf is the DSN,
//! because a change-data relay with no database to stream from is not a relay -
//! every function the extraction will rewrite
//! (`crates/zeroship-data-v8/src/replication.rs`'s `ensure_worker_slot`, and
//! `wal_consumer.rs`'s decode loop) already takes a `PostgreSQL` connection as its
//! first input.
//!
//! The bootstrap and observability controls come with the conversion for the
//! narrower reason the scheduler records: `Operational<T>` declares a TOML tier,
//! and without an overlay selector this binary would declare a source it could
//! never be given.

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

    // Secrets last within the table, by convention. The secret generates ONE
    // `--<name>-file` path flag and no value flag, so the material cannot reach
    // a process argument list.
    /// `PostgreSQL` DSN the relay streams logical replication from.
    ///
    /// Secret-classed by grammar: a DSN admits userinfo, so the type cannot
    /// depend on whether a particular deployment's value happens to carry a
    /// password.
    ///
    /// THIS LOGIN IS THE POINT OF THE WHOLE SERVICE, and its shape is settled
    /// even though nothing dials it yet: it takes `REPLICATION` and never
    /// `BYPASSRLS`, and it issues no creator-table SQL. The worker's role keeps
    /// both attributes today (`db/migrations-ts/20260818000200_worker_database_authority.ts`),
    /// and dropping them is a coordinated privilege change that has not landed -
    /// see `crates/zeroship-worker/src/db_posture.rs`, which currently REFUSES
    /// to boot without them.
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
        // The failure this guards: a platform binary that cannot be dry-run is
        // invisible to tests/config_check_e2e.sh, which is the only harness that
        // runs the real executable against a real overlay.
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
        // must not be, and that is asserted in core against the resolver and end
        // to end by tests/config_check_e2e.sh.
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
