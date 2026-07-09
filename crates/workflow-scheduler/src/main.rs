use clap::Parser;
use zeroship_workflow_scheduler::{run, SchedulerConfig, WorkflowSchedulerStore};

#[derive(Debug, Parser)]
struct Cli {
    #[arg(long, env = "WORKFLOW_SCHEDULER_DB")]
    db: String,
    #[arg(long, default_value_t = 60_000)]
    near_horizon_ms: i64,
    #[arg(long, default_value_t = 1_024)]
    max_loaded_timers: i64,
    #[arg(long, default_value_t = 64)]
    max_due_per_tick: usize,
    #[arg(long, default_value_t = 120_000)]
    inflight_ttl_ms: i64,
    #[arg(long, default_value_t = 1_000)]
    empty_sleep_ms: u64,
}

#[compio::main]
async fn main() {
    let cli = Cli::parse();
    let config = SchedulerConfig {
        near_horizon_ms: cli.near_horizon_ms,
        max_loaded_timers: cli.max_loaded_timers,
        max_due_per_tick: cli.max_due_per_tick,
        inflight_ttl_ms: cli.inflight_ttl_ms,
        empty_sleep_ms: cli.empty_sleep_ms,
    };
    if let Err(err) = run(WorkflowSchedulerStore::new(cli.db), config).await {
        tracing::error!(error = %err, "workflow scheduler exited");
        std::process::exit(1);
    }
}
