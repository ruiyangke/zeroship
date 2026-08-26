//! In-flight deploy accounting.
//!
//! A deploy streams the uploaded artifact to a tmp file and then mmaps it for
//! ingest, so its resident cost is bounded per request but not in aggregate:
//! nothing caps how many deploys run at once, and the control plane shares its
//! address space with every other endpoint.
//!
//! Whether that needs a semaphore is a capacity decision, and it cannot be made
//! from this code - it needs the peak concurrency a real deployment reaches.
//! This module measures that and nothing else. **It admits every deploy**; no
//! caller is ever made to wait, and no request is rejected on account of the
//! count. Adding a limit here would be a policy change, not a measurement.
//!
//! The counters live on an owned struct rather than in statics so a test can
//! hold its own instance. Two tests sharing a process-global counter observe
//! each other's deploys, which makes an exact-count assertion racy against the
//! test harness's own thread scheduling.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Deploy concurrency for this process.
///
/// The resource being sized is the control plane's address space, which is per
/// process and not per `AppState`, so this is a static rather than a field.
/// Tests that run deploys in a shared process therefore share it; nothing
/// branches on the count, so the only consequence is which deploy logs the
/// peak.
pub static DEPLOY_INFLIGHT: DeployInflight = DeployInflight::new();

/// Concurrency counters for deploys currently holding an artifact in memory.
#[derive(Debug, Default)]
pub struct DeployInflight {
    current: AtomicUsize,
    peak: AtomicUsize,
}

impl DeployInflight {
    pub const fn new() -> Self {
        Self {
            current: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        }
    }

    /// Count one deploy as in flight until the returned guard is dropped.
    ///
    /// Never blocks and never fails: the guard is an observation, not a permit.
    pub fn enter(&self) -> InflightDeployGuard<'_> {
        let depth = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        let prior_peak = self.peak.fetch_max(depth, Ordering::SeqCst);
        InflightDeployGuard {
            owner: self,
            depth,
            new_peak: depth > prior_peak,
        }
    }

    /// Deploys in flight right now.
    pub fn current(&self) -> usize {
        self.current.load(Ordering::SeqCst)
    }

    /// High-water mark of `current` since process start. Never decreases.
    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
}

/// Holds a deploy's place in the in-flight count for the guard's lifetime.
///
/// Every exit path out of the deploy handler runs `Drop`, including the early
/// returns for an oversized body, a payload error, and a failed mmap - which is
/// the reason this is a guard and not a matched increment/decrement pair. An
/// unwind releases it too.
#[must_use = "the deploy is counted only while this guard is alive; dropping it immediately closes the accounting window"]
#[derive(Debug)]
pub struct InflightDeployGuard<'a> {
    owner: &'a DeployInflight,
    depth: usize,
    new_peak: bool,
}

impl InflightDeployGuard<'_> {
    /// In-flight count at the moment this deploy entered, including itself.
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// True when this deploy raised the high-water mark. Lets a caller log the
    /// peak as it moves rather than once per deploy.
    pub fn is_new_peak(&self) -> bool {
        self.new_peak
    }
}

impl Drop for InflightDeployGuard<'_> {
    fn drop(&mut self) {
        self.owner.current.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_guard_counts_while_alive_and_releases_on_drop() {
        let inflight = DeployInflight::new();
        assert_eq!(inflight.current(), 0);
        {
            let guard = inflight.enter();
            assert_eq!(guard.depth(), 1);
            assert_eq!(inflight.current(), 1);
        }
        assert_eq!(inflight.current(), 0, "drop must release the slot");
    }

    #[test]
    fn peak_holds_the_high_water_mark_after_everything_drains() {
        let inflight = DeployInflight::new();
        {
            let _a = inflight.enter();
            let _b = inflight.enter();
            let _c = inflight.enter();
            assert_eq!(inflight.current(), 3);
        }
        assert_eq!(inflight.current(), 0);
        assert_eq!(
            inflight.peak(),
            3,
            "the peak is what the operator needs; draining must not erase it"
        );
    }

    #[test]
    fn only_the_deploy_that_raises_the_mark_reports_a_new_peak() {
        let inflight = DeployInflight::new();
        let a = inflight.enter();
        assert!(a.is_new_peak(), "the first deploy sets the mark at 1");
        let b = inflight.enter();
        assert!(b.is_new_peak(), "the second deploy raises it to 2");
        drop(b);

        // Re-entering at a depth the mark already covers is not a new peak, so
        // a caller logging on `is_new_peak` emits per rise and not per deploy.
        let c = inflight.enter();
        assert_eq!(c.depth(), 2);
        assert!(!c.is_new_peak());
        assert_eq!(inflight.peak(), 2);
    }

    #[test]
    fn a_later_shallower_deploy_does_not_lower_the_mark() {
        // The distinguishing case for "high-water mark" against "last observed
        // depth": both agree while the count only rises, so a test that never
        // re-enters below an established peak passes either implementation.
        let inflight = DeployInflight::new();
        {
            let _a = inflight.enter();
            let _b = inflight.enter();
            let _c = inflight.enter();
        }
        assert_eq!(inflight.peak(), 3);

        let lone = inflight.enter();
        assert_eq!(lone.depth(), 1);
        assert!(!lone.is_new_peak());
        assert_eq!(
            inflight.peak(),
            3,
            "a quiet period must not erase the busiest moment the operator is sizing for"
        );
    }

    #[test]
    fn an_early_return_out_of_the_guarded_region_still_releases() {
        fn guarded(inflight: &DeployInflight, fail_early: bool) -> Result<usize, &'static str> {
            let guard = inflight.enter();
            if fail_early {
                // The shape of every rejection below the guard in the deploy
                // handler: return without reaching the end of the function.
                return Err("rejected");
            }
            Ok(guard.depth())
        }

        let inflight = DeployInflight::new();
        assert!(guarded(&inflight, true).is_err());
        assert_eq!(inflight.current(), 0, "an early return must release");
        assert_eq!(guarded(&inflight, false), Ok(1));
        assert_eq!(inflight.current(), 0);
    }

    #[test]
    fn an_unwind_out_of_the_guarded_region_still_releases() {
        let inflight = DeployInflight::new();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = inflight.enter();
            panic!("deploy blew up");
        }));
        assert!(caught.is_err());
        assert_eq!(inflight.current(), 0, "an unwind must release");
        assert_eq!(inflight.peak(), 1);
    }
}
