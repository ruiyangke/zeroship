//! The scheduler's command definition plus its generated settings.
//!
//! In the LIBRARY rather than `main.rs` so the compiled configuration checker
//! can link it and invoke clap's `CommandFactory`, matching the five server
//! binaries.
//!
//! This binary's `main` still fails closed - the dispatch loop lives in the
//! control cron - and it was converted anyway. The reason is that its Cargo
//! target is classified `platform`
//! (`crates/workflow-scheduler/Cargo.toml`), which the design makes a
//! REQUIREMENT to register rather than a judgement call: "Production platform
//! one-shots ... require registration and cannot use an out-of-scope
//! classification". Its `WORKFLOW_SCHEDULER_*` family was also the last
//! hand-spelled environment family in a platform binary, and an operator-visible
//! surface is visible whether or not the process it configures currently runs.
//!
//! The bootstrap and observability controls come with the conversion for a
//! narrower reason: `Operational<T>` declares a TOML tier, and without an
//! overlay selector this binary would declare a source it could never be given.

use std::path::{Path, PathBuf};

use zeroship_core::config::{
    zeroship_config, BootstrapControl, CheckFormat, CommandControl, ObservabilityControls,
    Operational, OverlaySelector, Secret,
};
use zeroship_core::observability::LogFormat;

use crate::DEFAULT_TICK_SECS;

/// Tracing directive applied when nothing supplies `observability.log_filter`.
pub const DEFAULT_LOG_FILTER: &str = "info,zeroship_workflow_scheduler=debug";

/// Schema owning the scheduler tables.
///
/// Matches `WorkflowSchedulerStore::new` and the migration that owns them
/// (`db/migrations-ts/20260811000100_workflow_scheduler_store.ts`). They live in
/// the platform schema because the migration charter admits only
/// `["public", "zeroship"]`; the old `workflow_scheduler` schema no longer
/// exists anywhere.
pub const DEFAULT_SCHEDULER_SCHEMA: &str = "zeroship";

/// Every operational value the standalone scheduler resolves.
#[zeroship_config(binary = "zeroship-workflow-scheduler", scope = "workflow_scheduler")]
#[derive(Debug)]
pub struct SchedulerSettings {
    /// Optional shared config overlay path.
    #[config(shared = CONFIG)]
    pub config: BootstrapControl<Option<PathBuf>>,

    /// Disable auto-discovery of the well-known config overlay
    /// (`/etc/zeroship/zeroship.toml`); use compiled defaults instead.
    #[config(shared = NO_CONFIG)]
    pub no_config: BootstrapControl<bool>,

    /// Validate config (CLI + overlay) and print the resolved non-secret config,
    /// then exit without connecting to the scheduler store.
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

    /// Schema holding the scheduler timer and inflight tables.
    #[config(name = "workflow_scheduler.schema", default = DEFAULT_SCHEDULER_SCHEMA.to_owned())]
    pub schema: Operational<String>,

    /// Gateway internal base URL the dispatch seam posts to.
    #[config(name = "workflow_scheduler.gateway_url", default = String::new())]
    pub gateway_url: Operational<String>,

    /// Control-plane apply endpoint the scheduler acknowledges through.
    #[config(name = "workflow_scheduler.control_apply_url", default = String::new())]
    pub control_apply_url: Operational<String>,

    /// Timer-wheel tick interval in seconds.
    #[config(name = "workflow_scheduler.tick_secs", default = DEFAULT_TICK_SECS)]
    pub tick_secs: Operational<u64>,

    // Secrets last within the table, by convention.
    /// `PostgreSQL` DSN for the scheduler timer and inflight tables.
    ///
    /// Secret-classed by grammar: a DSN admits userinfo. Its only flag is
    /// `--database-url-file`, so the DSN cannot reach a process argument list.
    #[config(name = "workflow_scheduler.database_url")]
    pub database_url: Secret<String>,

    /// Interval in seconds between inflight-lease reaper sweeps.
    #[config(name = "workflow_scheduler.reaper_interval_secs", default = 30)]
    pub reaper_interval_secs: Operational<u64>,

    /// Horizon in milliseconds within which a timer is loaded into the wheel.
    #[config(name = "workflow_scheduler.near_horizon_ms", default = 60_000)]
    pub near_horizon_ms: Operational<i64>,

    /// Maximum timers held in the in-memory wheel.
    #[config(name = "workflow_scheduler.max_loaded_timers", default = 1_024)]
    pub max_loaded_timers: Operational<i64>,

    /// Maximum due timers claimed per tick.
    #[config(name = "workflow_scheduler.max_due_per_tick", default = 64)]
    pub max_due_per_tick: Operational<usize>,

    /// Inflight-lease time-to-live in milliseconds.
    #[config(name = "workflow_scheduler.inflight_ttl_ms", default = 120_000)]
    pub inflight_ttl_ms: Operational<i64>,
}

impl OverlaySelector for SchedulerSettingsSources {
    fn overlay_path(&self) -> Option<&Path> {
        self.config.as_deref()
    }

    fn allow_discovery(&self) -> bool {
        !self.no_config
    }
}

impl ObservabilityControls for SchedulerSettings {
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

    use super::{SchedulerSettings, SchedulerSettingsSources};

    #[test]
    fn the_hand_spelled_scheduler_env_family_is_gone() {
        // The failure this guards: the conversion renames the FLAG and leaves
        // the old environment variable working, so a deployment that sets
        // WORKFLOW_SCHEDULER_GATEWAY_URL keeps booting and nobody learns the
        // name changed until the alias is deleted later.
        let envs = SchedulerSettingsSources::command()
            .get_arguments()
            .filter_map(|arg| arg.get_env().map(|env| env.to_string_lossy().into_owned()))
            .collect::<Vec<_>>();
        assert!(!envs.is_empty(), "the settings must carry environment names");
        for env in &envs {
            assert!(
                env.starts_with("ZEROSHIP_"),
                "{env} is not a canonical projection"
            );
        }
        assert!(envs.contains(&"ZEROSHIP_WORKFLOW_SCHEDULER_GATEWAY_URL".to_owned()));

        // The DSN was the last hand-spelled name here, and the last consumer of
        // the `WORKFLOW_SCHEDULER_*` family. It is converted now, so the
        // assertion above covers it too - and the family must be GONE, not
        // merely unused: a deployment still setting WORKFLOW_SCHEDULER_DB has to
        // find that out, and nothing else in this binary would tell it.
        //
        // The DSN is read through the SPECS, not through clap: a secret carries
        // no `env` on its clap carrier, because putting one there would give a
        // secret a clap-visible value source. Deriving the name set from the
        // Command alone therefore CANNOT see a secret, which is exactly the
        // blind spot this line closes.
        let declared = SchedulerSettings::SPECS
            .iter()
            .filter_map(|spec| spec.env_name())
            .collect::<Vec<_>>();
        assert!(declared.contains(&"ZEROSHIP_WORKFLOW_SCHEDULER_DATABASE_URL".to_owned()));
        for env in envs.iter().chain(declared.iter()) {
            assert!(
                !env.starts_with("WORKFLOW_SCHEDULER_"),
                "the unprefixed family survives: {env}"
            );
        }
        let error =
            SchedulerSettingsSources::try_parse_from(["zeroship-workflow-scheduler", "--db", "x"])
                .expect_err("the DSN value flag must not exist");
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn the_overlay_supplies_a_scheduler_value_the_flag_then_overrides() {
        let overlay: toml::Value =
            toml::from_str("[workflow_scheduler]\ntick_secs = 9\nmax_due_per_tick = 7\n")
                .expect("fixture overlay");

        let resolved = SchedulerSettings::resolve_config(
            SchedulerSettingsSources::try_parse_from(["zeroship-workflow-scheduler"])
                .expect("bare parse"),
            Some(&overlay),
        )
        .expect("settings resolve");
        assert_eq!(*resolved.tick_secs.get(), 9);
        assert_eq!(*resolved.max_due_per_tick.get(), 7);

        let flagged = SchedulerSettings::resolve_config(
            SchedulerSettingsSources::try_parse_from([
                "zeroship-workflow-scheduler",
                "--tick-secs",
                "3",
            ])
            .expect("flag parse"),
            Some(&overlay),
        )
        .expect("settings resolve");
        assert_eq!(*flagged.tick_secs.get(), 3, "the flag must win over the overlay");
        assert_eq!(
            *flagged.max_due_per_tick.get(),
            7,
            "the untouched overlay value must survive"
        );

        // Does not cover the environment tier: clap merges it into the same
        // carrier, and a process-wide env mutation would race sibling tests.
    }
}
