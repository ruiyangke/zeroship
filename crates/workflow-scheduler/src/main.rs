//! Standalone scheduler process placeholder.
//!
//! The library currently owns only the timers -> inflight transition. Dispatch,
//! ack processing, and registration still run in the control cron, so this
//! binary fails before touching the scheduler store until that loop is
//! extracted. `--check-config` is the one thing it can do honestly, and it does
//! it: validating and printing configuration touches nothing.

use clap::Parser;
use zeroship_core::config::{bootstrap_or_exit, CheckConfigReport, CheckValue};
use zeroship_workflow_scheduler::config::{SchedulerCli, SchedulerSettings, DEFAULT_LOG_FILTER};
use zeroship_workflow_scheduler::STANDALONE_SCHEDULER_UNAVAILABLE;

#[compio::main]
async fn main() {
    let cli = SchedulerCli::parse();
    let (settings, boot) = bootstrap_or_exit::<SchedulerSettings>(
        cli.settings,
        DEFAULT_LOG_FILTER,
        "workflow-scheduler",
    );

    if *settings.check_config.get() {
        let mut report = CheckConfigReport::new();
        report.field(
            "config_source",
            CheckValue::Plain(boot.overlay.source.to_string()),
        );
        report.field("log_filter", CheckValue::Plain(boot.log_filter.clone()));
        report.field("log_format", CheckValue::Plain(boot.log_format.to_string()));
        report.field("db_configured", CheckValue::Secret(!cli.db.is_empty()));
        report.field("schema", CheckValue::Plain(settings.schema.get().clone()));
        report.field(
            "gateway_url",
            CheckValue::Plain(settings.gateway_url.get().clone()),
        );
        report.field(
            "control_apply_url",
            CheckValue::Plain(settings.control_apply_url.get().clone()),
        );
        report.field(
            "tick_secs",
            CheckValue::Count(usize::try_from(*settings.tick_secs.get()).unwrap_or(usize::MAX)),
        );
        report.field(
            "reaper_interval_secs",
            CheckValue::Count(
                usize::try_from(*settings.reaper_interval_secs.get()).unwrap_or(usize::MAX),
            ),
        );
        report.field(
            "near_horizon_ms",
            CheckValue::Count(
                usize::try_from(*settings.near_horizon_ms.get()).unwrap_or(usize::MAX),
            ),
        );
        report.field(
            "max_loaded_timers",
            CheckValue::Count(
                usize::try_from(*settings.max_loaded_timers.get()).unwrap_or(usize::MAX),
            ),
        );
        report.field(
            "max_due_per_tick",
            CheckValue::Count(*settings.max_due_per_tick.get()),
        );
        report.field(
            "inflight_ttl_ms",
            CheckValue::Count(
                usize::try_from(*settings.inflight_ttl_ms.get()).unwrap_or(usize::MAX),
            ),
        );
        report.emit(*settings.check_config_format.get());
        return;
    }

    tracing::error!(
        db_configured = !cli.db.is_empty(),
        schema = %settings.schema.get(),
        gateway_url = %settings.gateway_url.get(),
        control_apply_url = %settings.control_apply_url.get(),
        tick_secs = *settings.tick_secs.get(),
        reaper_interval_secs = *settings.reaper_interval_secs.get(),
        near_horizon_ms = *settings.near_horizon_ms.get(),
        max_loaded_timers = *settings.max_loaded_timers.get(),
        max_due_per_tick = *settings.max_due_per_tick.get(),
        inflight_ttl_ms = *settings.inflight_ttl_ms.get(),
        STANDALONE_SCHEDULER_UNAVAILABLE
    );
    eprintln!("{STANDALONE_SCHEDULER_UNAVAILABLE}");
    std::process::exit(1);
}
