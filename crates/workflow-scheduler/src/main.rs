//! Standalone scheduler process placeholder.
//!
//! The library currently owns only the timers -> inflight transition. Dispatch,
//! ack processing, and registration still run in the control cron, so this
//! binary fails before touching the scheduler store until that loop is
//! extracted.

use clap::Parser;
use zeroship_workflow_scheduler::config::SchedulerCli;
use zeroship_workflow_scheduler::STANDALONE_SCHEDULER_UNAVAILABLE;

#[compio::main]
async fn main() {
    let cli = SchedulerCli::parse();
    tracing::error!(
        db_configured = !cli.db.is_empty(),
        scheduler_schema = %cli.scheduler_schema,
        gateway_url = %cli.gateway_url,
        control_apply_url = %cli.control_apply_url,
        tick_secs = cli.tick_secs,
        reaper_interval_secs = cli.reaper_interval_secs,
        near_horizon_ms = cli.near_horizon_ms,
        max_loaded_timers = cli.max_loaded_timers,
        max_due_per_tick = cli.max_due_per_tick,
        inflight_ttl_ms = cli.inflight_ttl_ms,
        STANDALONE_SCHEDULER_UNAVAILABLE
    );
    eprintln!("{STANDALONE_SCHEDULER_UNAVAILABLE}");
    std::process::exit(1);
}
