//! Warm tier flush loop — periodically writes hot-tier atomic counter deltas
//! to the MeterStore (persistent storage).
//!
//! Uses CounterRegistry::flush_deltas() to non-destructively compute deltas
//! since the last flush, then writes them to the configured MeterStore adapter.
//! Counters keep accumulating so the enforcer and reconciler see accurate
//! running totals via snapshot().

use appbase_core::meter_store::{MeterStore, ResourceDelta};
use std::sync::Arc;
use std::time::Duration;

use crate::meter::MeterRegistry;

/// Spawn a background task that flushes hot-tier counters to the MeterStore.
///
/// Runs every `interval` (default 5 seconds). Computes deltas since the last
/// flush via `flush_deltas()` (non-destructive), then calls
/// `store.flush(app_id, deltas)` with the incremental values.
pub fn spawn_flusher(
    registry: Arc<MeterRegistry>,
    store: Arc<dyn MeterStore>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            flush_all(&registry, store.as_ref());
        }
    })
}

/// Flush all apps' counters from hot tier to warm tier.
///
/// Uses `flush_deltas()` (non-destructive) instead of `swap_all()` so that
/// counters keep accumulating. The enforcer and reconciler read running
/// totals via `snapshot()`, which would see near-zero values if we zeroed
/// counters every 5 seconds.
fn flush_all(registry: &MeterRegistry, store: &dyn MeterStore) {
    let meters = registry.all_meters();

    for (app_id, meter) in &meters {
        let deltas = meter.counters.flush_deltas();

        if deltas.is_empty() {
            continue; // no activity since last flush, skip
        }

        let resource_deltas: Vec<ResourceDelta> = deltas
            .into_iter()
            .map(|(resource, delta)| ResourceDelta { resource, delta })
            .collect();

        if let Err(e) = store.flush(app_id, &resource_deltas) {
            eprintln!("[flusher] Failed to flush {app_id}: {e}");
            // On failure, the delta is not lost from the hot tier (counters
            // still hold the running total). The next flush will re-compute
            // the delta from last_flushed, which was already advanced.
            // The event log (cold tier) remains the source of truth.
        }
    }
}
