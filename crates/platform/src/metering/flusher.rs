//! Warm tier flush loop — periodically writes hot-tier atomic counter deltas
//! to the MeterStore (persistent storage).
//!
//! Uses CounterRegistry::pending_deltas() to non-destructively compute deltas
//! since the last flush, then writes them to the configured MeterStore adapter.
//! Watermark is only advanced via commit_flush() after a successful store write.
//! Counters keep accumulating so the enforcer and reconciler see accurate
//! running totals via snapshot().

use crate::core::meter_store::{MeterStore, ResourceDelta};
use std::sync::Arc;
use std::time::Duration;

use crate::metering::meter::MeterRegistry;

/// Spawn a background task that flushes hot-tier counters to the MeterStore.
///
/// Runs every `interval` (default 5 seconds). Computes deltas since the last
/// flush via `pending_deltas()` (non-destructive), then calls
/// `store.flush(app_id, deltas)` with the incremental values.
pub fn spawn_flusher(
    registry: Arc<MeterRegistry>,
    store: Arc<dyn MeterStore>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            flush_all(&registry, store.as_ref()).await;
        }
    })
}

/// Flush all apps' counters from hot tier to warm tier.
///
/// Uses `pending_deltas()` (non-destructive) instead of `swap_all()` so that
/// counters keep accumulating. The enforcer and reconciler read running
/// totals via `snapshot()`, which would see near-zero values if we zeroed
/// counters every 5 seconds.
pub async fn flush_all(registry: &MeterRegistry, store: &dyn MeterStore) {
    let meters = registry.all_meters();

    for (app_id, meter) in &meters {
        // Skip meters that are in the middle of a rollover to avoid double-counting
        if meter.rolling_over.load(std::sync::atomic::Ordering::Acquire) {
            continue;
        }

        let (deltas, snapshot) = meter.counters.pending_deltas();

        if deltas.is_empty() {
            continue; // no activity since last flush, skip
        }

        let resource_deltas: Vec<ResourceDelta> = deltas
            .into_iter()
            .map(|(resource, delta)| ResourceDelta { resource, delta })
            .collect();

        if store.flush(app_id, &resource_deltas).await.is_ok() {
            meter.counters.commit_flush(&snapshot);
        } else {
            eprintln!("[flusher] Failed to flush {app_id}, will retry next cycle");
            // Watermark NOT advanced — deltas will be retried next cycle.
        }
    }
}
