use clap::Parser;
use zeroship_workflow_scheduler::{
    run, SchedulerConfig, WorkflowSchedulerStore, DEFAULT_TICK_SECS,
};

#[derive(Debug, Parser)]
struct Cli {
    #[arg(long, env = "WORKFLOW_SCHEDULER_DB")]
    db: String,
    #[arg(
        long = "scheduler-schema",
        env = "WORKFLOW_SCHEDULER_SCHEMA",
        default_value = "workflow_scheduler"
    )]
    scheduler_schema: String,
    #[arg(long = "gateway-url", env = "WORKFLOW_SCHEDULER_GATEWAY_URL")]
    gateway_url: String,
    #[arg(
        long = "control-apply-url",
        env = "WORKFLOW_SCHEDULER_CONTROL_APPLY_URL"
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
    tracing::info!(
        scheduler_schema = %cli.scheduler_schema,
        gateway_url = %cli.gateway_url,
        control_apply_url = %cli.control_apply_url,
        tick_secs = cli.tick_secs,
        reaper_interval_secs = cli.reaper_interval_secs,
        "workflow scheduler starting"
    );
    let config = SchedulerConfig {
        near_horizon_ms: cli.near_horizon_ms,
        max_loaded_timers: cli.max_loaded_timers,
        max_due_per_tick: cli.max_due_per_tick,
        inflight_ttl_ms: cli.inflight_ttl_ms,
        empty_sleep_ms: cli.tick_secs.saturating_mul(1_000),
    };
    let store = WorkflowSchedulerStore::new_with_schema(cli.db, cli.scheduler_schema);
    if let Err(err) = run(store, config).await {
        tracing::error!(error = %err, "workflow scheduler exited");
        std::process::exit(1);
    }
}
