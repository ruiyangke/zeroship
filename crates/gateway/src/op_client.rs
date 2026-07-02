//! Shared, breaker-guarded outbound HTTP client for the gateway → OP
//! path (auth-sdk §8.7, round-6 MAJOR #3).
//!
//! Every gateway→OP call used to construct a fresh `cyper::Client::new()`
//! with no bounded timeout and no shared failure state. A OP brownout
//! could then exhaust the gateway's outbound connections — the mint path's
//! exact risk (`/oauth2/token` slow or 5xx-ing while every request opens a
//! new connection pool). This module fixes that with three pieces wired
//! together:
//!
//! 1. **One reused `cyper::Client` per worker thread.** `cyper::Client`'s
//!    connector wraps its connect future in [`send_wrapper::SendWrapper`]
//!    (cyper-0.8 `connector.rs`), which *panics* if the client is used on a
//!    thread other than the one that created it. It is therefore effectively
//!    `!Send` in practice — exactly like the compio-postgres [`Pool`] in
//!    [`crate::db`]. We mirror that crate's per-worker-thread `thread_local`:
//!    the first OP touch on a worker thread builds the client; every
//!    subsequent call on that thread reuses it. NO `cyper::Client::new()`
//!    per call anywhere on the auth path.
//!
//! 2. **Bounded per-call timeout.** Each outbound call runs under a
//!    [`compio::time::timeout`] (the same primitive the worker-proxy path
//!    uses), so a hung OP returns a fast [`OpError::Timeout`] instead
//!    of an unbounded await pinning a connection.
//!
//! 3. **Circuit breaker with SHARED state.** The breaker's counters live in
//!    an `Arc<…atomics…>` stored on the `OidcRp` (one instance per gateway
//!    process, behind `Arc<GateState>`), so a brownout trips ALL worker
//!    threads consistently — not one breaker per thread. Closed →
//!    (N consecutive transport failures/timeouts) → Open (fast-fail every
//!    call for a cooldown window) → HalfOpen (one probe; success closes,
//!    failure re-opens). A OP **4xx** (e.g. `invalid_grant`) is a VALID
//!    upstream response, NOT a breaker failure — only transport errors and
//!    timeouts count.
//!
//! The happy path is unchanged: on success the breaker just resets its
//! failure counter and the caller gets exactly the response it got before.

use std::cell::RefCell;
use std::future::Future;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::time::{Duration, Instant};

/// Default bounded timeout for a single outbound OP call. Picked to
/// match the "few seconds" the spec (§8.7) calls for and the worker-proxy
/// idiom of a hard wall on every outbound await.
pub const DEFAULT_OP_TIMEOUT: Duration = Duration::from_secs(5);

/// Consecutive transport failures/timeouts that trip a closed breaker open.
pub const DEFAULT_FAILURE_THRESHOLD: u32 = 5;

/// How long the breaker stays open before allowing a single half-open probe.
pub const DEFAULT_COOLDOWN: Duration = Duration::from_secs(10);

thread_local! {
    /// One reused `cyper::Client` per worker thread. `cyper::Client` is
    /// `!Send`-in-practice (its connector panics off the creating thread),
    /// so — exactly like the compio-postgres pool in [`crate::db`] — we
    /// build it lazily on first use per worker thread and reuse it for the
    /// life of that thread. `cyper::Client` is internally `Arc<ClientInner>`,
    /// so the clone the connection machinery makes is a cheap refcount bump,
    /// not a new connection pool.
    static CLIENT: RefCell<Option<cyper::Client>> = const { RefCell::new(None) };
}

/// Get (build-once) this worker thread's reused `cyper::Client`.
fn thread_client() -> cyper::Client {
    CLIENT.with(|c| {
        let mut slot = c.borrow_mut();
        if let Some(existing) = slot.as_ref() {
            return existing.clone();
        }
        let client = cyper::Client::new();
        *slot = Some(client.clone());
        client
    })
}

/// Breaker state, encoded as a `u8` for the `AtomicU8`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    /// Normal operation — calls flow, failures are counted.
    Closed,
    /// Tripped — every call fast-fails until the cooldown elapses.
    Open,
    /// Cooldown elapsed — exactly one probe call is admitted; its outcome
    /// closes (success) or re-opens (failure) the breaker.
    HalfOpen,
}

impl BreakerState {
    const fn as_u8(self) -> u8 {
        match self {
            Self::Closed => 0,
            Self::Open => 1,
            Self::HalfOpen => 2,
        }
    }
    const fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Open,
            2 => Self::HalfOpen,
            _ => Self::Closed,
        }
    }
}

/// Shared circuit-breaker state. Cheap to `Arc`-share: a brownout on one
/// worker thread trips the breaker for every thread that holds the same
/// `Arc<CircuitBreaker>` (one per gateway process, on the `OidcRp`).
#[derive(Debug)]
pub struct CircuitBreaker {
    /// Current state (`BreakerState` as `u8`).
    state: AtomicU8,
    /// Consecutive transport failures since the last success (closed state).
    consecutive_failures: AtomicU32,
    /// Monotonic millis (since `epoch`) at which the breaker last opened.
    opened_at_ms: AtomicU64,
    /// Whether a half-open probe is currently in flight — admits exactly one.
    probe_in_flight: std::sync::atomic::AtomicBool,
    /// Failures needed to trip the breaker open.
    failure_threshold: u32,
    /// How long to stay open before admitting a probe.
    cooldown: Duration,
    /// Process-start reference for the monotonic `opened_at_ms` clock.
    /// `Instant` is not atomic, so we store elapsed millis against this.
    epoch: Instant,
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new(DEFAULT_FAILURE_THRESHOLD, DEFAULT_COOLDOWN)
    }
}

impl CircuitBreaker {
    /// Construct a breaker with explicit thresholds. `failure_threshold` is
    /// clamped to at least 1 (a 0-threshold breaker would trip on the first
    /// success-or-failure, which is nonsense).
    #[must_use]
    pub fn new(failure_threshold: u32, cooldown: Duration) -> Self {
        Self {
            state: AtomicU8::new(BreakerState::Closed.as_u8()),
            consecutive_failures: AtomicU32::new(0),
            opened_at_ms: AtomicU64::new(0),
            probe_in_flight: std::sync::atomic::AtomicBool::new(false),
            failure_threshold: failure_threshold.max(1),
            cooldown,
            epoch: Instant::now(),
        }
    }

    fn now_ms(&self) -> u64 {
        // The `unwrap_or(u64::MAX)` saturation is unreachable within process
        // lifetime: `u64::MAX` milliseconds is ~584 million years of uptime.
        // Documented so the (theoretical) saturating-to-MAX path — which would
        // make `cooldown_elapsed()` return true spuriously — is a known,
        // can't-happen cliff rather than a silent surprise.
        debug_assert!(
            u64::try_from(self.epoch.elapsed().as_millis()).is_ok(),
            "process uptime exceeded u64::MAX ms (~584M years) — unreachable",
        );
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Snapshot the current state (after applying any due cooldown→half-open
    /// transition). Intended for tests/metrics — the gating decision in the
    /// call path is made by [`Self::admit`].
    #[must_use]
    pub fn state(&self) -> BreakerState {
        let raw = self.state.load(Ordering::Acquire);
        let st = BreakerState::from_u8(raw);
        if st == BreakerState::Open && self.cooldown_elapsed() {
            BreakerState::HalfOpen
        } else {
            st
        }
    }

    fn cooldown_elapsed(&self) -> bool {
        let opened = self.opened_at_ms.load(Ordering::Acquire);
        let cooldown_ms = u64::try_from(self.cooldown.as_millis()).unwrap_or(u64::MAX);
        self.now_ms().saturating_sub(opened) >= cooldown_ms
    }

    /// Try to claim the single half-open probe slot, transitioning to
    /// `HalfOpen` on success. Returns the admission decision. Used by both the
    /// `Open`-after-cooldown and the already-`HalfOpen` arms of [`Self::admit`]
    /// so the "exactly one probe in flight" CAS lives in one place.
    fn claim_probe(&self) -> Admit {
        if self
            .probe_in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.state
                .store(BreakerState::HalfOpen.as_u8(), Ordering::Release);
            Admit::Allowed { is_probe: true }
        } else {
            // Another caller already holds the probe — reject this one.
            Admit::Rejected
        }
    }

    /// Decide whether to admit a call right now.
    ///
    /// - **Closed** → admit (a normal call).
    /// - **Open**, cooldown NOT elapsed → reject (fast-fail).
    /// - **Open**, cooldown elapsed → admit exactly ONE probe (transition to
    ///   half-open, mark a probe in flight). A second concurrent caller in
    ///   this window is rejected until the probe resolves.
    /// - **`HalfOpen`** → reject any call beyond the single in-flight probe.
    fn admit(&self) -> Admit {
        match self.state() {
            // `state()` already maps an Open-past-cooldown to HalfOpen, so the
            // cooldown gate is implicit here.
            BreakerState::Closed => Admit::Allowed { is_probe: false },
            BreakerState::Open => Admit::Rejected,
            BreakerState::HalfOpen => self.claim_probe(),
        }
    }

    /// Record a successful call. Closes the breaker and clears counters.
    fn on_success(&self, was_probe: bool) {
        self.consecutive_failures.store(0, Ordering::Release);
        self.state
            .store(BreakerState::Closed.as_u8(), Ordering::Release);
        if was_probe {
            self.probe_in_flight.store(false, Ordering::Release);
        }
    }

    /// Record a failing call (transport error or timeout — NOT a 4xx). In
    /// closed state this increments the consecutive-failure count and trips
    /// open at the threshold; a failing half-open probe re-opens immediately.
    fn on_failure(&self, was_probe: bool) {
        if was_probe {
            // A failed probe re-opens the breaker and restarts the cooldown.
            self.opened_at_ms.store(self.now_ms(), Ordering::Release);
            self.state.store(BreakerState::Open.as_u8(), Ordering::Release);
            self.probe_in_flight.store(false, Ordering::Release);
            return;
        }
        let n = self.consecutive_failures.fetch_add(1, Ordering::AcqRel) + 1;
        if n >= self.failure_threshold {
            self.opened_at_ms.store(self.now_ms(), Ordering::Release);
            self.state.store(BreakerState::Open.as_u8(), Ordering::Release);
        }
    }

    /// Release a half-open probe slot that was claimed but never resolved
    /// (neither [`Self::on_success`] nor [`Self::on_failure`] ran). This is the
    /// liveness backstop for a probe future that is **dropped mid-flight** —
    /// e.g. the mint hot path drives the call inside a cancellable
    /// `futures::future::Shared` single-flight, and the sole leader can be
    /// cancelled on client disconnect / ntex timeout with no follower to finish
    /// it. Without this, `probe_in_flight` would stay `true` forever and every
    /// subsequent [`Self::claim_probe`] CAS would fail — wedging the breaker in
    /// HalfOpen-rejecting for the life of the process (a self-DoS of all
    /// gateway→OP traffic).
    ///
    /// We treat an abandoned probe like an inconclusive (not failed) attempt:
    /// re-arm the cooldown by stamping `opened_at_ms = now` and dropping back to
    /// `Open`, then clear the probe slot. A future call past the cooldown is
    /// then admitted as a fresh probe. We do NOT touch `consecutive_failures`
    /// (cancellation is not an upstream failure signal).
    fn probe_abandoned(&self) {
        self.opened_at_ms.store(self.now_ms(), Ordering::Release);
        self.state.store(BreakerState::Open.as_u8(), Ordering::Release);
        self.probe_in_flight.store(false, Ordering::Release);
    }
}

/// RAII guard for an in-flight half-open probe. Created on the admitted probe
/// path; its [`Drop`] calls [`CircuitBreaker::probe_abandoned`] **unless** it
/// was explicitly disarmed by a resolved outcome ([`Self::disarm`], called from
/// the `on_success`/`on_failure` arms of [`call`]). This makes the probe slot
/// leak-proof against future cancellation: drop the call future at any point
/// between admission and resolution and the breaker re-arms its cooldown rather
/// than wedging shut.
struct ProbeGuard<'a> {
    breaker: &'a CircuitBreaker,
    armed: bool,
}

impl<'a> ProbeGuard<'a> {
    fn new(breaker: &'a CircuitBreaker) -> Self {
        Self { breaker, armed: true }
    }

    /// Disarm the guard: the probe resolved (success or failure) and the
    /// breaker already accounted for the slot, so Drop must NOT touch it.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProbeGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            // Resolved cleanly → disarmed. Still armed here ⇒ the probe future
            // was dropped/cancelled before resolving: release the slot.
            self.breaker.probe_abandoned();
        }
    }
}

/// The breaker's admission decision for one call.
enum Admit {
    Allowed { is_probe: bool },
    Rejected,
}

/// Error surfaced by [`call`]. The variants map directly onto the §8.7
/// behavior: `Upstream`/`Timeout` count as breaker failures, `Open` is the
/// fast-fail-while-open path that becomes a `503 upstream_unavailable`.
#[derive(Debug, thiserror::Error)]
pub enum OpError {
    /// The breaker is open — the call was rejected WITHOUT touching OP.
    /// Maps to `503 upstream_unavailable` at the handler.
    #[error("upstream_unavailable: op circuit breaker open")]
    Open,
    /// The bounded per-call timeout elapsed. Counts as a breaker failure.
    #[error("upstream timeout after {0:?}")]
    Timeout(Duration),
    /// A transport-level error from the reused client (connect refused,
    /// reset, TLS, body read). Counts as a breaker failure. A OP HTTP
    /// 4xx/5xx is NOT this — that surfaces inside the `Ok(Response)` and the
    /// caller decides; only sub-HTTP failures are `Upstream`.
    #[error("upstream transport error: {0}")]
    Upstream(String),
}

/// Run one outbound OP call through the shared reused client, the bounded
/// timeout, and the circuit breaker.
///
/// `make` is given this worker thread's reused [`cyper::Client`] and returns
/// the request future. We keep the closure shape (rather than taking a built
/// future) so the client is only cloned on the admitted path and the caller
/// reads the same reused client every other call site does.
///
/// On `Ok(resp)` — ANY completed HTTP response, including a OP 4xx/5xx —
/// the failure counter resets (a 4xx is a valid upstream answer, not a breaker
/// failure); the caller inspects `resp.status()`.
///
/// # Errors
/// - [`OpError::Open`] immediately if the breaker is open (no OP contact
///   at all — the brownout fast-fail).
/// - [`OpError::Timeout`] if the call exceeds `timeout` (counted as a
///   breaker failure).
/// - [`OpError::Upstream`] on a transport error (counted as a failure).
///
/// # Cancellation / liveness
/// If this future is dropped mid-flight while it holds the half-open probe
/// (e.g. the mint single-flight leader is cancelled on client disconnect / ntex
/// timeout with no follower), the [`ProbeGuard`]'s Drop re-arms the breaker's
/// cooldown and releases the probe slot — so a dropped probe can NEVER wedge the
/// breaker in HalfOpen-rejecting. A normal closed-state call holds no probe slot
/// and is unaffected.
///
/// # Send-ness
/// The returned future is **`!Send` by construction**: it borrows this thread's
/// reused `cyper::Client` (whose connector panics off its creating thread), so
/// it must be driven on the thread that created it — exactly the worker
/// single-thread-per-isolate model. Do not move it across a `Send` boundary
/// (e.g. a `Send`-requiring spawn); that yields a confusing error far from here.
// The returned future is `!Send` by construction (it borrows this thread's
// reused `cyper::Client`), exactly like `auth_token::mint`/`do_refresh`. The
// worker drives it on its creating thread, so `future_not_send` is expected,
// not a defect — silence it here so it doesn't drown the warn-level signal.
#[allow(clippy::future_not_send)]
pub async fn call<F, Fut>(
    breaker: &CircuitBreaker,
    timeout: Duration,
    make: F,
) -> Result<cyper::Response, OpError>
where
    F: FnOnce(cyper::Client) -> Fut,
    Fut: Future<Output = cyper::Result<cyper::Response>>,
{
    let is_probe = match breaker.admit() {
        Admit::Allowed { is_probe } => is_probe,
        Admit::Rejected => return Err(OpError::Open),
    };

    // RAII backstop for the single half-open probe slot: if THIS future is
    // dropped before the await below resolves, the guard's Drop releases the
    // slot and re-arms the cooldown (see `ProbeGuard`). For a closed-state call
    // (`is_probe == false`) the guard is inert — it owns no slot to release.
    let mut probe_guard = is_probe.then(|| ProbeGuard::new(breaker));

    let client = thread_client();
    let fut = make(client);

    match compio::time::timeout(timeout, fut).await {
        Ok(Ok(resp)) => {
            // The probe resolved: account for the slot via on_success and
            // disarm the guard so its Drop does NOT also release it.
            if let Some(g) = probe_guard.as_mut() {
                g.disarm();
            }
            // ANY completed HTTP response (2xx..5xx) is a valid upstream
            // answer for breaker purposes — a 4xx invalid_grant must NOT trip
            // the breaker (§8.7). Reset the failure counter / close.
            breaker.on_success(is_probe);
            Ok(resp)
        }
        Ok(Err(e)) => {
            if let Some(g) = probe_guard.as_mut() {
                g.disarm();
            }
            // Sub-HTTP transport failure: connect refused, reset, TLS, etc.
            breaker.on_failure(is_probe);
            Err(OpError::Upstream(e.to_string()))
        }
        Err(_elapsed) => {
            if let Some(g) = probe_guard.as_mut() {
                g.disarm();
            }
            // Bounded timeout fired — fast error, never an unbounded await.
            breaker.on_failure(is_probe);
            Err(OpError::Timeout(timeout))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn fast_breaker() -> CircuitBreaker {
        // 3-failure threshold, 50ms cooldown — keeps the state-machine unit
        // tests quick while exercising every transition.
        CircuitBreaker::new(3, Duration::from_millis(50))
    }

    #[test]
    fn breaker_starts_closed_and_admits() {
        let b = fast_breaker();
        assert_eq!(b.state(), BreakerState::Closed);
        assert!(matches!(b.admit(), Admit::Allowed { is_probe: false }));
    }

    #[test]
    fn failure_threshold_zero_is_clamped_to_one() {
        let b = CircuitBreaker::new(0, Duration::from_millis(10));
        // One failure must be enough to open (threshold clamped to 1), not
        // an immediate trip with zero failures.
        assert_eq!(b.state(), BreakerState::Closed);
        b.on_failure(false);
        assert_eq!(b.state.load(Ordering::Acquire), BreakerState::Open.as_u8());
    }

    #[test]
    fn n_consecutive_failures_open_the_breaker() {
        let b = fast_breaker();
        b.on_failure(false);
        b.on_failure(false);
        assert_eq!(b.state.load(Ordering::Acquire), BreakerState::Closed.as_u8());
        b.on_failure(false); // 3rd → trips
        assert_eq!(b.state.load(Ordering::Acquire), BreakerState::Open.as_u8());
        // While open (cooldown not elapsed) calls are rejected.
        assert!(matches!(b.admit(), Admit::Rejected));
    }

    #[test]
    fn success_resets_failure_count() {
        let b = fast_breaker();
        b.on_failure(false);
        b.on_failure(false);
        b.on_success(false); // resets the streak
        b.on_failure(false);
        b.on_failure(false);
        // Only 2 consecutive failures since the reset → still closed.
        assert_eq!(b.state.load(Ordering::Acquire), BreakerState::Closed.as_u8());
    }

    #[test]
    fn open_breaker_transitions_to_half_open_after_cooldown() {
        let b = fast_breaker();
        for _ in 0..3 {
            b.on_failure(false);
        }
        assert_eq!(b.state.load(Ordering::Acquire), BreakerState::Open.as_u8());
        assert!(matches!(b.admit(), Admit::Rejected));
        // Wait out the cooldown.
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(b.state(), BreakerState::HalfOpen);
        // Exactly one probe is admitted...
        assert!(matches!(b.admit(), Admit::Allowed { is_probe: true }));
        // ...and a concurrent second caller is rejected until it resolves.
        assert!(matches!(b.admit(), Admit::Rejected));
    }

    #[test]
    fn half_open_probe_success_closes_the_breaker() {
        let b = fast_breaker();
        for _ in 0..3 {
            b.on_failure(false);
        }
        std::thread::sleep(Duration::from_millis(60));
        let Admit::Allowed { is_probe } = b.admit() else {
            panic!("probe should be admitted after cooldown");
        };
        assert!(is_probe);
        b.on_success(is_probe);
        assert_eq!(b.state(), BreakerState::Closed);
        // Breaker fully usable again.
        assert!(matches!(b.admit(), Admit::Allowed { is_probe: false }));
    }

    #[test]
    fn half_open_probe_failure_reopens_the_breaker() {
        let b = fast_breaker();
        for _ in 0..3 {
            b.on_failure(false);
        }
        std::thread::sleep(Duration::from_millis(60));
        let Admit::Allowed { is_probe } = b.admit() else {
            panic!("probe admitted");
        };
        b.on_failure(is_probe); // probe fails → re-open, cooldown restarts
        assert_eq!(b.state.load(Ordering::Acquire), BreakerState::Open.as_u8());
        assert!(matches!(b.admit(), Admit::Rejected));
    }

    #[test]
    fn abandoned_probe_re_arms_cooldown_and_is_re_admittable() {
        // Regression for the dropped-half-open-probe wedge: claim the probe
        // (as admit() does on the half-open path), then ABANDON it without
        // calling on_success/on_failure — exactly what happens when the call
        // future is dropped mid-flight. The slot must be released and the
        // cooldown re-armed so a future probe is admittable rather than the
        // breaker wedging in HalfOpen-rejecting forever.
        let b = fast_breaker();
        for _ in 0..3 {
            b.on_failure(false);
        }
        std::thread::sleep(Duration::from_millis(60));
        let Admit::Allowed { is_probe } = b.admit() else {
            panic!("probe admitted after cooldown");
        };
        assert!(is_probe);
        // A second caller is rejected while the probe is (notionally) in flight.
        assert!(matches!(b.admit(), Admit::Rejected));

        // The probe future is dropped mid-flight: neither on_success nor
        // on_failure ran. The guard's Drop calls probe_abandoned().
        b.probe_abandoned();

        // The breaker is Open again with a freshly re-armed cooldown (NOT
        // wedged in HalfOpen with a stuck probe slot).
        assert_eq!(b.state.load(Ordering::Acquire), BreakerState::Open.as_u8());
        // Immediately, the cooldown has NOT elapsed → still fast-fails.
        assert!(matches!(b.admit(), Admit::Rejected));
        // After the (re-armed) cooldown, a fresh probe is admittable again.
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(b.state(), BreakerState::HalfOpen);
        assert!(
            matches!(b.admit(), Admit::Allowed { is_probe: true }),
            "a fresh probe must be admittable after an abandoned one",
        );
    }

    #[test]
    fn probe_guard_drop_releases_the_slot_unless_disarmed() {
        // Exercise the RAII ProbeGuard directly (the `call()` end-to-end drop is
        // covered by the integration test, which runs inside a compio/ntex
        // runtime — `call()`'s internal `compio::time::timeout` can't be polled
        // outside one). Here we prove the two guard arms in isolation:
        //
        // (1) An ARMED guard that is dropped (the cancellation path) releases the
        //     probe slot via probe_abandoned() — the breaker re-arms, not wedges.
        // (2) A DISARMED guard (the resolved path) does NOT touch the slot on
        //     drop — the resolution (`on_success`/`on_failure`) owns it.
        let b = fast_breaker();
        for _ in 0..3 {
            b.on_failure(false);
        }
        std::thread::sleep(Duration::from_millis(60));
        // Claim the probe exactly as the half-open admit() path does.
        assert!(matches!(b.admit(), Admit::Allowed { is_probe: true }));
        assert!(b.probe_in_flight.load(Ordering::Acquire), "probe slot is taken");

        // (1) Armed guard dropped ⇒ slot released + cooldown re-armed.
        {
            let _g = ProbeGuard::new(&b);
            // (guard is armed by default)
        } // <- drop here fires probe_abandoned()
        assert!(!b.probe_in_flight.load(Ordering::Acquire), "armed-guard drop released the slot");
        assert_eq!(b.state.load(Ordering::Acquire), BreakerState::Open.as_u8());

        // Re-claim and prove the DISARMED arm leaves the slot for the resolver.
        std::thread::sleep(Duration::from_millis(60));
        assert!(matches!(b.admit(), Admit::Allowed { is_probe: true }));
        assert!(b.probe_in_flight.load(Ordering::Acquire));
        {
            let mut g = ProbeGuard::new(&b);
            g.disarm(); // resolved cleanly — Drop must be a no-op
        }
        // (2) Slot is STILL held — the disarmed guard didn't touch it; the
        // resolver (on_success/on_failure) is responsible for clearing it.
        assert!(
            b.probe_in_flight.load(Ordering::Acquire),
            "disarmed-guard drop must NOT release the slot",
        );
        // And the resolver does clear it.
        b.on_success(true);
        assert!(!b.probe_in_flight.load(Ordering::Acquire));
        assert_eq!(b.state(), BreakerState::Closed);
    }

    #[test]
    fn shared_breaker_arc_trips_for_all_holders() {
        // The breaker is Arc-shared so a brownout observed on one worker
        // thread trips the SAME state every other thread sees.
        let b = Arc::new(fast_breaker());
        let b2 = Arc::clone(&b);
        for _ in 0..3 {
            b.on_failure(false);
        }
        // The clone observes the open state — one shared state, not per-thread.
        assert_eq!(b2.state.load(Ordering::Acquire), BreakerState::Open.as_u8());
        assert!(matches!(b2.admit(), Admit::Rejected));
    }
}
