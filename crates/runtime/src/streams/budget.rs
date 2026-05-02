//! Per-isolate concurrent stream cap (D-18).
//!
//! Constructions over `MAX_LIVE_STREAMS` throw `RangeError("too many
//! concurrent streams")`. The cap is per-thread per-isolate.
//!
//! Justification (design D-18): target 200K req/s × 32-thread worker =
//! ~6,250 req/s/thread; mean lifecycle ~50ms = 312 streams in flight per
//! thread; 65,536 is 200× headroom.
//!
//! The cap exists to surface bugs that would otherwise leak streams; it
//! is not a perf limit. If real workloads approach it, switch to
//! `track_alloc/free` instrumentation and raise to `u32::MAX`.

use std::cell::Cell;

/// Soft cap on concurrent live ReadableStream/WritableStream/TransformStream
/// instances per isolate. Construction past this throws `RangeError`.
pub const MAX_LIVE_STREAMS: u32 = 65_536;

thread_local! {
    /// Per-thread (== per-isolate, single-threaded model) live count.
    static LIVE_COUNT: Cell<u32> = const { Cell::new(0) };
}

/// Try to register a new live stream. Returns `Ok(())` if under the cap,
/// `Err` (suitable for the construction site to throw) otherwise.
pub fn try_alloc_stream() -> Result<StreamBudgetGuard, &'static str> {
    let current = LIVE_COUNT.with(|c| c.get());
    if current >= MAX_LIVE_STREAMS {
        return Err("too many concurrent streams");
    }
    LIVE_COUNT.with(|c| c.set(current + 1));
    Ok(StreamBudgetGuard { _priv: () })
}

/// RAII guard. Decrement the live count when the underlying stream's
/// boxed state is dropped (which happens via the V8 weak finalizer).
#[allow(missing_debug_implementations)]
pub struct StreamBudgetGuard {
    _priv: (),
}

impl Drop for StreamBudgetGuard {
    fn drop(&mut self) {
        LIVE_COUNT.with(|c| {
            let n = c.get();
            // saturating_sub guards against double-frees in the rare
            // case the finalizer races on isolate shutdown — we'd
            // rather log-zero than wrap to u32::MAX.
            c.set(n.saturating_sub(1));
        });
    }
}

/// Read the current live count. Test-only — production code should never
/// inspect this except for diagnostics.
pub fn live_count() -> u32 {
    LIVE_COUNT.with(|c| c.get())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_increments_and_drop_decrements() {
        let baseline = live_count();
        {
            let _g = try_alloc_stream().expect("under cap");
            assert_eq!(live_count(), baseline + 1);
        }
        assert_eq!(live_count(), baseline);
    }

    #[test]
    fn nested_guards_track_correctly() {
        let baseline = live_count();
        let _g1 = try_alloc_stream().unwrap();
        {
            let _g2 = try_alloc_stream().unwrap();
            assert_eq!(live_count(), baseline + 2);
        }
        assert_eq!(live_count(), baseline + 1);
    }
}
