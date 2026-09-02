//! Process-wide scheduling for abandoned CDC slot cleanup.

use std::time::Duration;

use compio::runtime::JoinHandle;
use zeroship_data_core::error::DbError;
use zeroship_plugin_db::slot_reaper::{
    OperatorSlotReaper, ABANDONED_INACTIVITY_THRESHOLD, SWEEP_INTERVAL,
};

/// Bound every catalog sweep below its cadence. A maintenance connection that
/// stops making progress must release its liveness lease and fail the worker;
/// otherwise peers could eventually mistake its CDC slots for abandoned while
/// the worker keeps serving requests.
const SWEEP_DEADLINE: Duration = Duration::from_secs(30);

/// Acquire the worker's crash-released lease before returning, then run one
/// periodic cluster sweep on the process runtime.
pub async fn start(
    db_url: &str,
    worker_id: &str,
) -> Result<JoinHandle<Result<(), DbError>>, DbError> {
    let initial = OperatorSlotReaper::connect(db_url, worker_id).await?;
    tracing::info!(
        sweep_interval_secs = SWEEP_INTERVAL.as_secs(),
        sweep_deadline_secs = SWEEP_DEADLINE.as_secs(),
        inactivity_threshold_secs = ABANDONED_INACTIVITY_THRESHOLD.as_secs(),
        "operator abandoned-slot reaper started"
    );

    Ok(compio::runtime::spawn(run(initial)))
}

async fn run(mut reaper: OperatorSlotReaper) -> Result<(), DbError> {
    loop {
        let report = compio::time::timeout(SWEEP_DEADLINE, reaper.sweep())
            .await
            .map_err(|_| DbError::Transient {
                message: format!(
                    "operator abandoned-slot sweep exceeded its {} second deadline",
                    SWEEP_DEADLINE.as_secs()
                ),
            })??;
        if !report.dropped.is_empty() {
            tracing::warn!(
                inspected = report.inspected,
                dropped = report.dropped.len(),
                slots = ?report.dropped,
                "operator reaped abandoned logical replication slots"
            );
        }
        compio::time::sleep(SWEEP_INTERVAL).await;
    }
}
