//! Warm tier flush loop — periodically writes hot-tier atomic counter deltas
//! to the MeterStore (persistent storage).
//!
//! Uses CounterRegistry::swap_all() to atomically read and reset all counters,
//! then flushes the deltas to the configured MeterStore adapter.

use appbase_core::meter_store::{MeterStore, ResourceDelta};
use std::sync::Arc;
use std::time::Duration;

use crate::meter::MeterRegistry;

/// Spawn a background task that flushes hot-tier counters to the MeterStore.
///
/// Runs every `interval` (default 5 seconds). Reads each app's atomic counters
/// via `swap_all()` (atomically read + reset to zero), then calls
/// `store.flush(app_id, deltas)` with the accumulated values.
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
fn flush_all(registry: &MeterRegistry, store: &dyn MeterStore) {
    let meters = registry.all_meters();

    for (app_id, meter) in &meters {
        let deltas = meter.counters.swap_all();

        if deltas.is_empty() {
            continue; // no activity, skip flush
        }

        let resource_deltas: Vec<ResourceDelta> = deltas
            .into_iter()
            .map(|(resource, delta)| ResourceDelta { resource, delta })
            .collect();

        if let Err(e) = store.flush(app_id, &resource_deltas) {
            eprintln!("[flusher] Failed to flush {app_id}: {e}");
            // Deltas are lost — the hot tier has already been reset.
            // This is acceptable: the event log (cold tier) is the
            // source of truth for billing reconciliation.
        }
    }
}
