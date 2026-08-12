//! The scheduler's command definition.
//!
//! In the LIBRARY rather than `main.rs` so the compiled configuration checker
//! can link it and invoke clap's `CommandFactory`, matching the five server
//! binaries. The fields themselves are still hand-spelled operational values;
//! converting them is a later step, and this move is what makes that step
//! reviewable rather than invisible.

use clap::Parser;

use crate::DEFAULT_TICK_SECS;

/// zeroship-workflow-scheduler startup configuration.
#[derive(Debug, Parser)]
#[command(name = "zeroship-workflow-scheduler")]
pub struct SchedulerCli {
    #[arg(long, env = "WORKFLOW_SCHEDULER_DB", default_value = "")]
    pub db: String,

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
    pub scheduler_schema: String,

    #[arg(
        long = "gateway-url",
        env = "WORKFLOW_SCHEDULER_GATEWAY_URL",
        default_value = ""
    )]
    pub gateway_url: String,

    #[arg(
        long = "control-apply-url",
        env = "WORKFLOW_SCHEDULER_CONTROL_APPLY_URL",
        default_value = ""
    )]
    pub control_apply_url: String,

    #[arg(long = "tick-secs", default_value_t = DEFAULT_TICK_SECS)]
    pub tick_secs: u64,

    #[arg(long = "reaper-interval-secs", default_value_t = 30)]
    pub reaper_interval_secs: u64,

    #[arg(long, default_value_t = 60_000)]
    pub near_horizon_ms: i64,

    #[arg(long, default_value_t = 1_024)]
    pub max_loaded_timers: i64,

    #[arg(long, default_value_t = 64)]
    pub max_due_per_tick: usize,

    #[arg(long, default_value_t = 120_000)]
    pub inflight_ttl_ms: i64,
}
