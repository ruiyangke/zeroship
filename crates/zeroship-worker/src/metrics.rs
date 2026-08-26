//! Minimal process-wide metrics for the worker.
//!
//! Exposes atomic counters via `/metrics` in Prometheus plain-text format.
//! The goal is not to replicate a metrics library — it's to make basic
//! operational questions answerable without `strace`:
//!
//!   * How many requests has this worker handled?
//!   * How many fetches are in flight right now?
//!   * How much RAM is tied up in stream buffers?
//!   * Is the auth check rejecting anything?
//!
//! Counters are `AtomicU64`; gauges are read-on-demand. Cross-thread safe
//! because ntex workers share the same process.
//!
//! This is deliberately tiny — full-fat Prometheus integration (labels,
//! histograms, OTLP push) is out of scope for this file. When the platform
//! has paying customers, swap this for `metrics-rs` + a proper exposition
//! library.

use std::sync::atomic::{AtomicU64, Ordering};

macro_rules! counter {
    ($name:ident) => {
        pub static $name: AtomicU64 = AtomicU64::new(0);
    };
}

counter!(DISPATCH_TOTAL);
counter!(DISPATCH_REJECTED_AUTH);
counter!(DISPATCH_REJECTED_BAD_APP_ID);
counter!(DISPATCH_REJECTED_BAD_ENVELOPE);
counter!(DISPATCH_REJECTED_BODY_TOO_LARGE);
counter!(DISPATCH_ERRORS_TOTAL);
counter!(ON_DEMAND_LOADS_TOTAL);
counter!(ON_DEMAND_LOAD_FAILURES);
counter!(ENV_UNAVAILABLE_TOTAL);
counter!(ENV_FETCH_FAILURES);
counter!(BUNDLE_FETCH_FAILURES);
counter!(BUNDLE_HASH_MISMATCH);
counter!(LRU_EVICTIONS_TOTAL);
counter!(LOCK_POISONED_TOTAL);
counter!(RECONCILE_ITERATIONS_TOTAL);

/// Increment a counter. Thin wrapper so call sites read more intentionally
/// than a bare `ATOMIC.fetch_add(1, ...)`.
#[inline]
pub fn inc(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Render the current metrics snapshot as Prometheus plain-text. The
/// exposition format is deliberately simple — `HELP` + `TYPE` lines per
/// metric, one sample per line, no labels. Counters only (no histograms)
/// which matches what `metrics-rs` would emit for an `increment_counter!`.
pub fn render() -> String {
    let mut out = String::with_capacity(2048);

    emit_counter(&mut out, "zeroship_worker_dispatch_total",
        "HTTP requests dispatched to V8 via the unified fetch handler",
        &DISPATCH_TOTAL);
    emit_counter(&mut out, "zeroship_worker_dispatch_rejected_auth",
        "Dispatch requests rejected because the Authorization bearer didn't match the worker key",
        &DISPATCH_REJECTED_AUTH);
    emit_counter(&mut out, "zeroship_worker_dispatch_rejected_bad_app_id",
        "Dispatch requests rejected for malformed app_id path parameter",
        &DISPATCH_REJECTED_BAD_APP_ID);
    emit_counter(&mut out, "zeroship_worker_dispatch_rejected_bad_envelope",
        "HTTP-dispatch requests rejected for malformed envelope JSON",
        &DISPATCH_REJECTED_BAD_ENVELOPE);
    emit_counter(&mut out, "zeroship_worker_dispatch_rejected_body_too_large",
        "Dispatch envelopes rejected for exceeding MAX_DISPATCH_BODY_BYTES",
        &DISPATCH_REJECTED_BODY_TOO_LARGE);
    emit_counter(&mut out, "zeroship_worker_dispatch_errors_total",
        "Dispatch results returned to the gateway as errors (excludes rejections)",
        &DISPATCH_ERRORS_TOTAL);
    emit_counter(&mut out, "zeroship_worker_on_demand_loads_total",
        "Apps loaded on-demand (cache miss) from the control plane",
        &ON_DEMAND_LOADS_TOTAL);
    emit_counter(&mut out, "zeroship_worker_on_demand_load_failures",
        "On-demand app loads that failed",
        &ON_DEMAND_LOAD_FAILURES);
    emit_counter(&mut out, "zeroship_worker_env_unavailable_total",
        "Dispatches that returned 503 because the env cache had no entry for the app",
        &ENV_UNAVAILABLE_TOTAL);
    emit_counter(&mut out, "zeroship_worker_env_fetch_failures",
        "Failed env fetches from the control plane",
        &ENV_FETCH_FAILURES);
    emit_counter(&mut out, "zeroship_worker_bundle_fetch_failures",
        "Failed bundle fetches from the control plane",
        &BUNDLE_FETCH_FAILURES);
    emit_counter(&mut out, "zeroship_worker_bundle_hash_mismatch",
        "Bundles whose SHA256 didn't match the control plane's reported deploy_hash",
        &BUNDLE_HASH_MISMATCH);
    emit_counter(&mut out, "zeroship_worker_lru_evictions_total",
        "V8 isolates evicted from the per-thread cache to make room for new loads",
        &LRU_EVICTIONS_TOTAL);
    emit_counter(&mut out, "zeroship_worker_lock_poisoned_total",
        "Times a SharedVersions/SharedEnvs RwLock returned PoisonError — indicates a panic in the holder thread",
        &LOCK_POISONED_TOTAL);
    emit_counter(&mut out, "zeroship_worker_reconcile_iterations_total",
        "Reconcile-loop iterations (per-thread)",
        &RECONCILE_ITERATIONS_TOTAL);

    // Gauges sourced live from the runtime.
    emit_gauge(
        &mut out,
        "zeroship_runtime_stream_buffered_bytes",
        "Bytes currently buffered across all StreamWriter instances in this process",
        zeroship_runtime::channel::stream_global_buffered_bytes() as u64,
    );

    out
}

fn emit_counter(out: &mut String, name: &str, help: &str, c: &AtomicU64) {
    out.push_str("# HELP ");
    out.push_str(name);
    out.push(' ');
    out.push_str(help);
    out.push('\n');
    out.push_str("# TYPE ");
    out.push_str(name);
    out.push_str(" counter\n");
    out.push_str(name);
    out.push(' ');
    out.push_str(&c.load(Ordering::Relaxed).to_string());
    out.push('\n');
}

fn emit_gauge(out: &mut String, name: &str, help: &str, value: u64) {
    out.push_str("# HELP ");
    out.push_str(name);
    out.push(' ');
    out.push_str(help);
    out.push('\n');
    out.push_str("# TYPE ");
    out.push_str(name);
    out.push_str(" gauge\n");
    out.push_str(name);
    out.push(' ');
    out.push_str(&value.to_string());
    out.push('\n');
}
