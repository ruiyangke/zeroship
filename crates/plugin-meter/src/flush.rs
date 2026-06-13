//! Compio flush task — drains the per-worker [`Meter`] every ~10s and
//! POSTs a `UsageReport` to control `/internal/usage`.
//!
//! Salvaged from the since-deleted `crates/platform` (`metering/flusher.rs`):
//! the tokio `spawn` + `tokio::time::sleep` loop becomes a `compio::runtime::
//! spawn` + `compio::time::interval` loop (zero tokio). The reset-after-ack
//! discipline is preserved structurally by the meter API: `Meter::drain`
//! snapshots AND zeroes; on a POST failure we `Meter::merge` the snapshot
//! back so it is retried next tick (at-least-once producer — control dedups
//! on `(worker_id, sequence)`).

use std::sync::Arc;
use std::time::Duration;

use crate::meter::{build_report, Meter, SequenceSource};

/// Default flush cadence. Matches the design's "~10s".
pub const DEFAULT_FLUSH_INTERVAL: Duration = Duration::from_secs(10);

/// Per-request timeout for the usage POST (mirrors the worker's
/// control-request budget; a slow control plane must not wedge the loop).
const POST_TIMEOUT: Duration = Duration::from_secs(5);

/// Configuration for the flush task.
#[derive(Debug, Clone)]
pub struct FlushConfig {
    /// Control-plane base URL, e.g. `http://control:9090`.
    pub control_url: String,
    /// Shared control-key bearer secret. Empty ⇒ no auth header (dev).
    pub control_key: String,
    /// Stable identity string for this worker process. The dedup key
    /// component on the control side. MUST be stable across the process's
    /// lifetime and ideally across restarts of the same logical worker.
    pub worker_id: String,
    /// Flush cadence.
    pub interval: Duration,
}

/// Spawn the flush task on the compio runtime. Detaches; runs until the
/// process exits. One per worker process.
pub fn spawn_flush_task(meter: Arc<Meter>, config: FlushConfig) {
    compio::runtime::spawn(async move {
        flush_loop(meter, config).await;
    })
    .detach();
}

/// The flush loop: every `interval`, drain + build + POST. On a failed POST,
/// carry the snapshot forward (merge back) so no usage is lost.
async fn flush_loop(meter: Arc<Meter>, config: FlushConfig) {
    let seq = SequenceSource::new();
    let url = format!("{}/internal/usage", config.control_url);
    loop {
        compio::time::sleep(config.interval).await;
        flush_once(&meter, &seq, &config, &url).await;
    }
}

/// One flush cycle. Public(crate) for the unit test, which drives a single
/// cycle against a stub URL and asserts the carry-forward on failure.
pub(crate) async fn flush_once(
    meter: &Meter,
    seq: &SequenceSource,
    config: &FlushConfig,
    url: &str,
) {
    let snapshot = meter.drain();
    if snapshot.is_empty() {
        return; // no activity this interval
    }
    let sequence = seq.next();
    let Some(report) = build_report(&config.worker_id, sequence, snapshot.clone()) else {
        return;
    };

    match post_report(url, &config.control_key, &report).await {
        Ok(()) => {
            tracing::debug!(
                worker_id = %config.worker_id,
                sequence,
                apps = report.counters.len(),
                "meter: usage report acked"
            );
        }
        Err(e) => {
            // Carry forward: merge the drained snapshot back so it is
            // re-sent next tick. The sequence is NOT reused — the next
            // report gets a fresh sequence carrying the merged-back totals,
            // and control's dedup is on sequence, so this never
            // double-counts even if the failed POST actually landed.
            tracing::warn!(
                worker_id = %config.worker_id,
                sequence,
                error = %e,
                "meter: usage POST failed; carrying snapshot forward"
            );
            meter.merge(&snapshot);
        }
    }
}

/// POST a `UsageReport` as JSON to control `/internal/usage` via cyper
/// (zero tokio). Bounded by `POST_TIMEOUT`.
async fn post_report(
    url: &str,
    control_key: &str,
    report: &zeroship_core::types::UsageReport,
) -> Result<(), String> {
    compio::time::timeout(POST_TIMEOUT, post_report_inner(url, control_key, report))
        .await
        .map_err(|_| format!("usage POST timed out after {}s", POST_TIMEOUT.as_secs()))?
}

async fn post_report_inner(
    url: &str,
    control_key: &str,
    report: &zeroship_core::types::UsageReport,
) -> Result<(), String> {
    let body = serde_json::to_string(report).map_err(|e| format!("serialize report: {e}"))?;
    // Per-thread cyper client: cyper's connector uses SendWrapper and panics
    // if dereferenced off the creating thread, so it must NOT be a
    // process-wide static (same rationale as the worker's sync.rs client).
    let client = this_thread_meter_client();
    let mut builder = client
        .post(url)
        .map_err(|e| format!("invalid control URL: {e}"))?
        .header("content-type", "application/json")
        .map_err(|e| format!("invalid content-type header: {e}"))?;
    if !control_key.is_empty() {
        builder = builder
            .header("authorization", &format!("Bearer {control_key}"))
            .map_err(|e| format!("invalid auth header: {e}"))?;
    }
    let response = builder
        .body(body)
        .send()
        .await
        .map_err(|e| format!("usage POST failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "usage POST HTTP error: {} {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or("")
        ));
    }
    Ok(())
}

thread_local! {
    static METER_CLIENT: cyper::Client = cyper::Client::new();
}

fn this_thread_meter_client() -> cyper::Client {
    METER_CLIENT.with(|c| c.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn config(url_base: &str) -> FlushConfig {
        FlushConfig {
            control_url: url_base.to_string(),
            control_key: String::new(),
            worker_id: "w-test".to_string(),
            interval: Duration::from_millis(10),
        }
    }

    #[test]
    fn flush_once_empty_meter_is_noop_and_does_not_bump_sequence() {
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let meter = Meter::new();
            let seq = SequenceSource::new();
            let cfg = config("http://127.0.0.1:1"); // dead port; never reached
            let url = format!("{}/internal/usage", cfg.control_url);
            flush_once(&meter, &seq, &cfg, &url).await;
            // No drain payload ⇒ no POST attempted ⇒ sequence untouched.
            assert_eq!(seq.next(), 1, "empty flush must not consume a sequence");
        });
    }

    #[test]
    fn flush_once_carries_forward_on_failed_post() {
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let meter = Meter::new();
            let app = Uuid::new_v4().to_string();
            meter.increment(&app, "requests", 5);
            meter.increment(&app, "emails", 2);

            let seq = SequenceSource::new();
            // 127.0.0.1:1 refuses connections immediately ⇒ POST fails fast.
            let cfg = config("http://127.0.0.1:1");
            let url = format!("{}/internal/usage", cfg.control_url);
            flush_once(&meter, &seq, &cfg, &url).await;

            // The POST failed, so the snapshot must have been merged back:
            // a subsequent drain still sees the full totals (nothing lost).
            let snap = meter.drain();
            let id = Uuid::parse_str(&app).unwrap();
            let u = snap.get(&id).expect("carried-forward app present");
            assert_eq!(u.requests, 5, "requests carried forward after failed POST");
            assert_eq!(u.custom.get("emails").copied(), Some(2));
        });
    }
}
