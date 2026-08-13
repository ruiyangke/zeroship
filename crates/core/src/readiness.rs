//! Shared readiness-probe plumbing for the platform services.
//!
//! Every platform service (control, gateway, worker, migrated, auth) exposes
//! the same pair:
//!
//! - `GET /healthz` - LIVENESS. "This process is running and its event loop is
//!   not wedged." A constant 200. It MUST NOT touch a dependency: a liveness
//!   probe that fails when Postgres blips gets the container killed for
//!   someone else's outage.
//! - `GET /readyz` - READINESS. "I can actually serve traffic." This one
//!   checks the dependencies the service cannot work without.
//!
//! Both are unauthenticated, so `/readyz` has to be built so that a flood of
//! probes cannot become a flood of dependency round trips, and so that the
//! answer leaks nothing. Two types here carry that:
//!
//! - [`ReadinessGate`] - for a dependency that must be *actively* probed
//!   (a Postgres round trip, a blob-store stat). It bounds the probe with an
//!   explicit timeout, caches the outcome for a short TTL, and collapses
//!   concurrent probes so at most ONE is outstanding at a time. Worst case is
//!   therefore one dependency round trip per TTL per process, no matter how
//!   many probes arrive.
//! - [`SyncFreshness`] - for a dependency a background loop is ALREADY
//!   polling (the gateway's route pull, the worker's version poll). The loop
//!   stamps each success; `/readyz` only reads the stamp. That costs zero
//!   outbound requests per probe, so there is no fan-out to bound at all.
//!
//! Neither type carries the failure reason. `/readyz` answers 200 or 503 with
//! a minimal body and nothing else - no DSN, no host, no driver error text,
//! no version. The operator reads the reason from the service's logs, which
//! are not reachable from an unauthenticated caller.

use std::future::Future;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// Default bound on a single dependency probe.
///
/// A readiness probe that hangs is worse than one that fails: it turns a probe
/// into a stuck request and the orchestrator learns nothing until its own
/// timeout fires.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Default lifetime of a cached probe outcome.
///
/// Short enough that a real dependency outage shows up almost immediately,
/// long enough that probe traffic cannot drive dependency traffic.
pub const CACHE_TTL: Duration = Duration::from_secs(2);

#[derive(Debug, Default)]
struct Slot {
    /// Last observed outcome. `false` before the first probe completes, so an
    /// un-probed service reads NOT ready rather than ready.
    ready: bool,
    /// When `ready` was last written. `None` = never probed.
    checked_at: Option<Instant>,
    /// A probe is running right now; concurrent callers reuse `ready`.
    in_flight: bool,
}

/// A cached, bounded, single-flight readiness check over one dependency.
///
/// Construct one per service (not per request) and keep it alive for the
/// process lifetime.
#[derive(Debug)]
pub struct ReadinessGate {
    ttl: Duration,
    timeout: Duration,
    slot: Mutex<Slot>,
}

impl Default for ReadinessGate {
    fn default() -> Self {
        Self::new(CACHE_TTL, PROBE_TIMEOUT)
    }
}

impl ReadinessGate {
    /// A gate with the platform default TTL and probe timeout.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn new(ttl: Duration, timeout: Duration) -> Self {
        Self {
            ttl,
            timeout,
            slot: Mutex::new(Slot::default()),
        }
    }

    /// Evaluate readiness, running `probe` at most once per TTL.
    ///
    /// `probe` returns `true` when the dependency answered. It is bound by the
    /// gate's timeout; a probe that outlives it counts as NOT ready and the
    /// caller is released on schedule.
    ///
    /// Callers that arrive while a probe is in flight get the previous
    /// outcome immediately rather than starting a second probe. Before the
    /// first probe ever completes that previous outcome is `false`, which is
    /// the fail-closed answer.
    pub async fn ready<F, Fut>(&self, probe: F) -> bool
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = bool>,
    {
        {
            let mut slot = self.slot.lock();
            let fresh = slot
                .checked_at
                .is_some_and(|at| at.elapsed() < self.ttl);
            if fresh || slot.in_flight {
                return slot.ready;
            }
            slot.in_flight = true;
        }
        // The guard clears `in_flight` on EVERY exit, including the one that
        // has no code after it: ntex drops a handler future when the client
        // disconnects, so a probe can be cancelled mid-await. Clearing the
        // flag only on the success path would leave it set forever and wedge
        // the gate on its last answer for the life of the process.
        let _guard = InFlightGuard { gate: self };
        let ok = matches!(compio::time::timeout(self.timeout, probe()).await, Ok(true));
        let mut slot = self.slot.lock();
        slot.ready = ok;
        slot.checked_at = Some(Instant::now());
        ok
    }
}

struct InFlightGuard<'a> {
    gate: &'a ReadinessGate,
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.gate.slot.lock().in_flight = false;
    }
}

/// The last time a background reconcile loop reached its upstream.
///
/// Used where the service already polls the dependency on a timer, so
/// `/readyz` reads a stamp instead of issuing its own request.
///
/// Monotonic ([`Instant`], not wall clock) so a system clock step cannot make
/// a stale service read fresh or a fresh one read stale.
#[derive(Debug, Default)]
pub struct SyncFreshness {
    last_success: Mutex<Option<Instant>>,
}

impl SyncFreshness {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that the loop just completed a full successful cycle.
    pub fn mark_success(&self) {
        *self.last_success.lock() = Some(Instant::now());
    }

    /// `true` when a cycle succeeded within `max_age`.
    ///
    /// Before the first success this is `false`: a service that has never
    /// reached its upstream is not ready, even though its process is alive.
    #[must_use]
    pub fn is_fresh(&self, max_age: Duration) -> bool {
        self.last_success
            .lock()
            .is_some_and(|at| at.elapsed() <= max_age)
    }
}

/// How stale a background-polled dependency may get before `/readyz` fails.
///
/// Three poll periods: one missed cycle is a blip (a slow control plane, a
/// retried connection), three consecutive missed cycles is an outage. The
/// floor keeps a very short poll interval from producing a budget so tight
/// that ordinary scheduling jitter reads as an outage.
#[must_use]
pub fn staleness_budget(poll_interval: Duration) -> Duration {
    std::cmp::max(poll_interval.saturating_mul(3), Duration::from_secs(10))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn block_on<F: Future>(fut: F) -> F::Output {
        compio::runtime::Runtime::new().unwrap().block_on(fut)
    }

    #[test]
    fn gate_caches_the_outcome_for_the_ttl() {
        block_on(async {
            let gate = ReadinessGate::new(Duration::from_secs(60), PROBE_TIMEOUT);
            let calls = Cell::new(0u32);
            for _ in 0..25 {
                assert!(gate.ready(|| async { calls.set(calls.get() + 1); true }).await);
            }
            // 25 probes, ONE dependency round trip: this is the bound that
            // keeps an unauthenticated endpoint from being a DoS lever.
            assert_eq!(calls.get(), 1);
        });
    }

    #[test]
    fn gate_reprobes_once_the_ttl_expires() {
        block_on(async {
            let gate = ReadinessGate::new(Duration::ZERO, PROBE_TIMEOUT);
            let calls = Cell::new(0u32);
            for _ in 0..3 {
                gate.ready(|| async { calls.set(calls.get() + 1); true }).await;
            }
            assert_eq!(calls.get(), 3);
        });
    }

    #[test]
    fn gate_is_not_ready_before_the_first_probe_answers() {
        block_on(async {
            let gate = ReadinessGate::new(CACHE_TTL, Duration::from_millis(20));
            // A probe that never completes must read NOT ready, and must
            // return within the timeout rather than hanging the caller.
            let started = Instant::now();
            let ready = gate
                .ready(|| async {
                    compio::time::sleep(Duration::from_secs(30)).await;
                    true
                })
                .await;
            assert!(!ready);
            assert!(started.elapsed() < Duration::from_secs(5));
        });
    }

    #[test]
    fn gate_reports_a_failing_dependency() {
        block_on(async {
            let gate = ReadinessGate::new(Duration::ZERO, PROBE_TIMEOUT);
            assert!(gate.ready(|| async { true }).await);
            assert!(!gate.ready(|| async { false }).await);
            assert!(gate.ready(|| async { true }).await);
        });
    }

    #[test]
    fn a_cancelled_probe_does_not_wedge_the_gate() {
        block_on(async {
            let gate = ReadinessGate::new(Duration::ZERO, Duration::from_secs(30));
            // Drop the probe future mid-flight, exactly as ntex does when the
            // client disconnects. Without the Drop guard `in_flight` stays
            // set and every later probe short-circuits to the stale answer.
            {
                let fut = gate.ready(|| async {
                    compio::time::sleep(Duration::from_secs(30)).await;
                    true
                });
                let mut fut = std::pin::pin!(fut);
                let waker = std::task::Waker::noop();
                let mut cx = std::task::Context::from_waker(waker);
                assert!(fut.as_mut().poll(&mut cx).is_pending());
            }
            assert!(gate.ready(|| async { true }).await);
        });
    }

    #[test]
    fn freshness_is_false_until_the_first_success() {
        let fresh = SyncFreshness::new();
        assert!(!fresh.is_fresh(Duration::from_secs(3600)));
        fresh.mark_success();
        assert!(fresh.is_fresh(Duration::from_secs(3600)));
        // A zero budget means "must have succeeded this instant"; the stamp is
        // already in the past, so this is the stale arm.
        assert!(!fresh.is_fresh(Duration::ZERO));
    }

    #[test]
    fn staleness_budget_is_three_polls_with_a_floor() {
        assert_eq!(
            staleness_budget(Duration::from_secs(5)),
            Duration::from_secs(15)
        );
        assert_eq!(
            staleness_budget(Duration::from_secs(1)),
            Duration::from_secs(10)
        );
    }
}
