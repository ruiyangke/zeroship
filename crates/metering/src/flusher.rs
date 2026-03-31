//! Warm tier flush loop — periodically writes hot-tier atomic counter deltas
//! to the MeterStore (persistent storage).
//!
//! Uses `swap(0, AcqRel)` to atomically read and reset each counter,
//! then flushes the deltas to the configured MeterStore adapter.

use appbase_core::meter_store::{MeterStore, ResourceDelta};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use crate::meter::MeterRegistry;

/// Spawn a background task that flushes hot-tier counters to the MeterStore.
///
/// Runs every `interval` (default 5 seconds). Reads each app's atomic counters
/// via `swap(0, AcqRel)` (atomically read + reset to zero), then calls
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
        // Atomically read and reset each counter
        let deltas = collect_deltas(meter);

        if deltas.is_empty() {
            continue; // no activity, skip flush
        }

        if let Err(e) = store.flush(app_id, &deltas) {
            eprintln!("[flusher] Failed to flush {app_id}: {e}");
            // Deltas are lost — the hot tier has already been reset.
            // This is acceptable: the event log (cold tier) is the
            // source of truth for billing reconciliation.
        }
    }
}

/// Read and reset atomic counters for one app, returning non-zero deltas.
fn collect_deltas(meter: &crate::meter::AppMeter) -> Vec<ResourceDelta> {
    let mut deltas = Vec::new();

    let swap = |counter: &std::sync::atomic::AtomicU64| -> u64 {
        counter.swap(0, Ordering::AcqRel)
    };

    macro_rules! collect {
        ($name:expr, $field:ident) => {
            let v = swap(&meter.$field);
            if v > 0 {
                deltas.push(ResourceDelta {
                    resource: $name.into(),
                    delta: v,
                });
            }
        };
    }

    collect!("requests", requests);
    collect!("cpu_ms", cpu_time_us); // stored as microseconds, key is cpu_ms (enforcer divides by 1000)
    collect!("wall_ms", wall_time_us); // stored as microseconds, key is wall_ms
    collect!("egress_bytes", egress_bytes);
    collect!("db_reads", db_reads);
    collect!("db_writes", db_writes);
    collect!("kv_ops", kv_ops);

    deltas
}
