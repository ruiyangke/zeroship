//! Standalone scheduler process placeholder.
//!
//! The library currently owns only the timers -> inflight transition. Dispatch,
//! ack processing, and registration still run in the control cron, so this
//! binary fails before touching the scheduler store until that loop is
//! extracted.

use clap::Parser;
use zeroship_workflow_scheduler::{
    DEFAULT_TICK_SECS, STANDALONE_SCHEDULER_UNAVAILABLE,
};

#[derive(Debug, Parser)]
struct Cli {
    #[arg(long, env = "WORKFLOW_SCHEDULER_DB", default_value = "")]
    db: String,
    #[arg(
        long = "scheduler-schema",
        env = "WORKFLOW_SCHEDULER_SCHEMA",
        // Matches WorkflowSchedulerStore::new and the migration that owns the
        // tables (db/migrations-ts/20260811000100_workflow_scheduler_store.ts).
        // They live in the platform schema because the migration charter admits
        // only ["public", "zeroship"]; the old `workflow_scheduler` schema no
        // longer exists anywhere.
        default_value = "zeroship"
    )]
    scheduler_schema: String,
    #[arg(
        long = "gateway-url",
        env = "WORKFLOW_SCHEDULER_GATEWAY_URL",
        default_value = ""
    )]
    gateway_url: String,
    #[arg(
        long = "control-apply-url",
        env = "WORKFLOW_SCHEDULER_CONTROL_APPLY_URL",
        default_value = ""
    )]
    control_apply_url: String,
    #[arg(long = "tick-secs", default_value_t = DEFAULT_TICK_SECS)]
    tick_secs: u64,
    #[arg(long = "reaper-interval-secs", default_value_t = 30)]
    reaper_interval_secs: u64,
    #[arg(long, default_value_t = 60_000)]
    near_horizon_ms: i64,
    #[arg(long, default_value_t = 1_024)]
    max_loaded_timers: i64,
    #[arg(long, default_value_t = 64)]
    max_due_per_tick: usize,
    #[arg(long, default_value_t = 120_000)]
    inflight_ttl_ms: i64,
}

#[compio::main]
async fn main() {
    let cli = Cli::parse();
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
