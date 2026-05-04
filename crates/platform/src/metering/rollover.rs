//! Period rollover — double-buffered counter swap with drain-and-wait.
//!
//! At period boundaries (monthly, daily), atomically swaps each app's
//! counter set so new requests write to fresh counters. After draining
//! in-flight writers, reads the old counters and archives them.

use crate::core::meter_store::{MeterStore, ResourceDelta};
use std::sync::Arc;
use std::time::Duration;

use crate::metering::meter::MeterRegistry;

/// Configuration for the period roller.
pub struct RolloverConfig {
    /// How long to wait for in-flight requests to drain after buffer swap.
    /// Default: 20s (2x max wall time of 10s).
    pub drain_wait: Duration,
    /// Batch size for processing apps (avoids warm-tier write spikes).
    pub batch_size: usize,
    /// Sleep between batches.
    pub batch_delay: Duration,
}

impl Default for RolloverConfig {
    fn default() -> Self {
        Self {
            drain_wait: Duration::from_secs(20),
            batch_size: 100,
            batch_delay: Duration::from_millis(10),
        }
    }
}

/// Spawn a background task that checks for period boundaries and triggers rollover.
///
/// Returns a `JoinHandle` that runs until the runtime shuts down. The task
/// checks every 60 seconds whether the calendar month has changed and, if so,
/// performs the double-buffered rollover for every registered app.
pub fn spawn_period_roller(
    registry: Arc<MeterRegistry>,
    store: Arc<dyn MeterStore>,
    config: RolloverConfig,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Check every 60 seconds if a period boundary has been crossed
        let mut last_period = current_period_key();
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let now_period = current_period_key();
            if now_period != last_period {
                tracing::info!(
                    last_period = %last_period,
                    now_period = %now_period,
                    "rollover period boundary crossed"
                );
                rollover_all(&registry, store.as_ref(), &config).await;
                last_period = now_period;
            }
        }
    })
}

/// Roll over all apps' counters for the new period.
///
/// For each app (processed in batches):
/// 1. Lock the registry, swap the meter for a fresh one, release the lock.
/// 2. Sleep `drain_wait` so in-flight request handlers drop their `Arc<AppMeter>`.
/// 3. Read the old meter's snapshot (safe because all writers have drained).
/// 4. Call `MeterStore::rollover()` to archive the warm-tier counters.
async fn rollover_all(registry: &MeterRegistry, store: &dyn MeterStore, config: &RolloverConfig) {
    let app_ids: Vec<String> = registry.all_meters().keys().cloned().collect();

    // Process in batches to avoid warm-tier write spikes
    for batch in app_ids.chunks(config.batch_size) {
        let mut old_meters: Vec<(String, Arc<crate::metering::meter::AppMeter>)> = Vec::new();

        // Step 1-2: Mark old meters as rolling over, then swap atomically
        for app_id in batch {
            if let Some(old) = registry.swap_for_rollover(app_id) {
                // Set rolling_over BEFORE drain wait so flusher skips this meter
                old.rolling_over.store(true, std::sync::atomic::Ordering::Release);
                old_meters.push((app_id.clone(), old));
            }
        }

        // Step 3: Drain in-flight writers — after this sleep, all Arc refs
        // held by request handlers should have been dropped.
        tokio::time::sleep(config.drain_wait).await;

        // Step 4-5: Read remaining deltas from old meters, then archive.
        //
        // We use pending_deltas() (non-destructive read) because the old meter is
        // removed from the registry (no new writes) and marked rolling_over (flusher
        // skips it). This avoids the double-counting race where both flusher and
        // rollover could read the same values via swap_all().
        for (app_id, old_meter) in &old_meters {
            let (final_deltas, snapshot) = old_meter.counters.pending_deltas();

            // Flush remaining deltas from old meter to warm tier before archiving.
            // Any increments since the last flusher tick would otherwise be lost.
            let deltas: Vec<ResourceDelta> = final_deltas
                .iter()
                .map(|(k, &v)| ResourceDelta {
                    resource: k.clone(),
                    delta: v,
                })
                .collect();
            if !deltas.is_empty() {
                if let Err(e) = store.flush(app_id, &deltas).await {
                    tracing::error!(app_id = %app_id, error = %e, "rollover flush failed before rollover");
                    continue; // skip archive for this app — data would be incomplete
                }
                old_meter.counters.commit_flush(&snapshot);
            }

            // Now archive and reset warm tier
            if let Err(e) = store.rollover(app_id).await {
                tracing::error!(app_id = %app_id, error = %e, "rollover store call failed");
            }

            let requests = final_deltas.get("requests").copied().unwrap_or(0);
            let cpu_us = final_deltas.get("cpu_us").copied().unwrap_or(0);
            let cpu_ms = cpu_us as f64 / 1000.0;
            tracing::info!(
                app_id = %app_id,
                requests,
                cpu_ms,
                "rollover archived",
            );
        }

        // Batch delay to avoid write spikes
        if batch.len() == config.batch_size {
            tokio::time::sleep(config.batch_delay).await;
        }
    }
}

/// Generate a period key like "2026-03" for the current month.
fn current_period_key() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!("{}-{:02}", now.year(), now.month() as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::QuotaPlan;
    use crate::metering::store::memory::InMemoryStore;

    #[tokio::test]
    async fn rollover_swaps_meter_and_archives() {
        let registry = Arc::new(MeterRegistry::new(QuotaPlan::free(), vec![]));
        let store = Arc::new(InMemoryStore::new());

        // Record some usage
        let meter = registry.get_or_create("app1");
        meter.record_request(
            100_000, // 100ms in microseconds
            200_000, // 200ms in microseconds
            1024,
            256,
        );

        // Perform rollover with 0s drain (test only)
        let config = RolloverConfig {
            drain_wait: Duration::from_millis(0),
            batch_size: 100,
            batch_delay: Duration::from_millis(0),
        };

        rollover_all(&registry, store.as_ref(), &config).await;

        // New meter should have zero counters
        let new_meter = registry.get_or_create("app1");
        let snap = new_meter.snapshot();
        assert_eq!(snap.get("requests").copied().unwrap_or(0), 0);

        // Store should have history
        let hist = store.history("app1", 10).await.unwrap();
        assert!(!hist.is_empty());
    }

    #[tokio::test]
    async fn rollover_handles_missing_app() {
        let registry = Arc::new(MeterRegistry::new(QuotaPlan::free(), vec![]));
        let store = Arc::new(InMemoryStore::new());

        // Rollover with no apps should be a no-op
        let config = RolloverConfig {
            drain_wait: Duration::from_millis(0),
            batch_size: 100,
            batch_delay: Duration::from_millis(0),
        };

        rollover_all(&registry, store.as_ref(), &config).await;
        // No panic = success
    }

    #[tokio::test]
    async fn rollover_preserves_old_meter_snapshot() {
        let registry = Arc::new(MeterRegistry::new(QuotaPlan::free(), vec![]));
        let store = Arc::new(InMemoryStore::new());

        // Record usage on two apps
        let m1 = registry.get_or_create("app1");
        m1.record_request(50_000, 100_000, 512, 128);
        // Also increment db counters via the plugin meter interface
        // (db_reads, db_writes, kv_ops are no longer core — if needed, register them as plugins)

        let m2 = registry.get_or_create("app2");
        m2.record_request(200_000, 400_000, 2048, 512);

        let config = RolloverConfig {
            drain_wait: Duration::from_millis(0),
            batch_size: 100,
            batch_delay: Duration::from_millis(0),
        };

        rollover_all(&registry, store.as_ref(), &config).await;

        // Both apps should have fresh meters
        let s1 = registry.get_or_create("app1").snapshot();
        let s2 = registry.get_or_create("app2").snapshot();
        assert_eq!(s1.get("requests").copied().unwrap_or(0), 0);
        assert_eq!(s2.get("requests").copied().unwrap_or(0), 0);

        // Both should have history
        assert!(!store.history("app1", 10).await.unwrap().is_empty());
        assert!(!store.history("app2", 10).await.unwrap().is_empty());
    }

    #[test]
    fn period_key_format() {
        let key = current_period_key();
        assert!(key.contains('-'));
        assert!(key.len() >= 6); // "YYYY-MM"
    }
}
