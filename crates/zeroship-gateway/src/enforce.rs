use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use std::collections::HashSet;

use ntex::web::HttpResponse;
use uuid::Uuid;
use zeroship_bundle::{RateLimit, RateLimitPer};
use zeroship_core::types::{AccountState, SpendState};

/// Throttle multiplier applied to a Degraded app: its effective concurrency
/// ceiling is divided by this, and each of its requests consumes this many
/// rate-limit tokens instead of one — a `1/DEGRADE_FACTOR` throughput cut
/// against the SAME immutable buckets (no rebuild, instant recovery).
pub const DEGRADE_FACTOR: u32 = 8;

/// Spend-limit gate. Run BEFORE rate-limit/dispatch (decision D1 — state is
/// pulled on the `RouteEntry`). `Block` → 402 `SPEND_LIMIT`; every other state
/// passes (Warn stamps a header at the call site; Degrade is throttled by the
/// degraded registries). A blocked request never reaches the worker proxy.
pub fn check_spend(state: SpendState) -> Result<(), HttpResponse> {
    match state {
        SpendState::Block => Err(HttpResponse::PaymentRequired()
            .json(&serde_json::json!({"code": "SPEND_LIMIT"}))),
        SpendState::Allow | SpendState::Warn | SpendState::Degrade => Ok(()),
    }
}

/// Payment/account gate (billing G2). The OUTER AND with [`check_spend`]: a
/// request is served iff the creator's account is `Active`/`PastDue` AND spend
/// is not `Block`. Run this BEFORE `check_spend` at the same hoist point spend
/// uses, so a `Suspended` creator's apps 402 across EVERY action class (worker,
/// redirect, rewrite, AND static egress) before any worker proxy.
///
/// `Suspended` → 402 `ACCOUNT_SUSPENDED`. `PastDue` is the GRACE window (still
/// served — it is the warning state, not a block) and `Active` passes. The
/// two gates emit DISTINCT codes (`ACCOUNT_SUSPENDED` vs `SPEND_LIMIT`) so a
/// caller can tell a dead-card suspension from a usage-cap block.
pub fn check_account(state: AccountState) -> Result<(), HttpResponse> {
    match state {
        AccountState::Suspended => Err(HttpResponse::PaymentRequired()
            .json(&serde_json::json!({"code": "ACCOUNT_SUSPENDED"}))),
        AccountState::Active | AccountState::PastDue => Ok(()),
    }
}

// --- Token Bucket Rate Limiter ---

fn pack(tokens: u64, timestamp: u32) -> u64 {
    (tokens << 32) | timestamp as u64
}

fn unpack(state: u64) -> (u64, u32) {
    (state >> 32, state as u32)
}

fn now_secs() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as u32
}

struct TokenBucket {
    state: AtomicU64,
    capacity: u64,
    refill_rate: u64,
}

impl TokenBucket {
    fn new(rate: u32, burst: u32) -> Self {
        let capacity = u64::from(burst) * 1000;
        Self {
            state: AtomicU64::new(pack(capacity, now_secs())),
            capacity,
            refill_rate: u64::from(rate) * 1000,
        }
    }

    fn try_acquire(&self) -> bool {
        self.try_acquire_n(1)
    }

    /// Consume `n` whole tokens (the token math is 1000-scaled internally, so
    /// one logical token is `1000` units). A Degraded app calls this with
    /// `DEGRADE_FACTOR`, so each request costs `DEGRADE_FACTOR` tokens — a
    /// `1/DEGRADE_FACTOR` throughput cut against the SAME bucket with no
    /// rebuild. `n == 0` is treated as `1` (never free).
    ///
    /// The cost is CLAMPED to the bucket capacity (`min(n*1000, capacity)`).
    /// Without the clamp, a Degraded request costing `DEGRADE_FACTOR` tokens
    /// against a bucket whose capacity is `< DEGRADE_FACTOR` (a tiny-burst
    /// rule) could NEVER be satisfied — Degrade would silently become a hard
    /// Block regardless of refill. Clamping guarantees Degrade is always a
    /// throttle, never a hard Block, for any `(rate, burst)` config (#6).
    fn try_acquire_n(&self, n: u32) -> bool {
        let cost = (u64::from(n.max(1)) * 1000).min(self.capacity);
        loop {
            let state = self.state.load(Ordering::Acquire);
            let (tokens, last) = unpack(state);
            let now = now_secs();
            let elapsed = u64::from(now.saturating_sub(last));
            let refilled = (tokens + elapsed * self.refill_rate).min(self.capacity);
            if refilled < cost {
                return false;
            }
            let new_state = pack(refilled - cost, now);
            if self
                .state
                .compare_exchange(state, new_state, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return true;
            }
        }
    }
}

// Manual Debug — inner fields are not Debug-friendly.
impl std::fmt::Debug for RateLimitRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimitRegistry").finish_non_exhaustive()
    }
}

pub struct RateLimitRegistry {
    buckets: RwLock<HashMap<Uuid, Arc<TokenBucket>>>,
    /// Apps in spend-Degrade. A request from a degraded app consumes
    /// `DEGRADE_FACTOR` tokens instead of 1 (a `1/DEGRADE_FACTOR` throughput
    /// cut) against the SAME immutable bucket — no rebuild, instant recovery
    /// on `clear_degraded`.
    degraded: RwLock<HashSet<Uuid>>,
    default_rate: u32,
    default_burst: u32,
}

impl RateLimitRegistry {
    pub fn new(rate: u32, burst: u32) -> Self {
        Self {
            buckets: RwLock::new(HashMap::new()),
            degraded: RwLock::new(HashSet::new()),
            default_rate: rate,
            default_burst: burst,
        }
    }

    fn get_or_create(&self, app_id: &Uuid) -> Arc<TokenBucket> {
        {
            let r = self.buckets.read().unwrap();
            if let Some(b) = r.get(app_id) {
                return b.clone();
            }
        }
        let mut w = self.buckets.write().unwrap();
        w.entry(*app_id)
            .or_insert_with(|| Arc::new(TokenBucket::new(self.default_rate, self.default_burst)))
            .clone()
    }

    /// Mark `app_id` degraded (`on = true`) or clear it. Idempotent.
    pub fn set_degraded(&self, app_id: &Uuid, on: bool) {
        let mut w = self.degraded.write().unwrap();
        if on {
            w.insert(*app_id);
        } else {
            w.remove(app_id);
        }
    }

    /// Convenience: clear `app_id`'s degraded flag. Recovery is instant — the
    /// bucket was never rebuilt, so it serves at its normal rate immediately.
    pub fn clear_degraded(&self, app_id: &Uuid) {
        self.set_degraded(app_id, false);
    }

    #[must_use]
    pub fn is_degraded(&self, app_id: &Uuid) -> bool {
        self.degraded.read().unwrap().contains(app_id)
    }
}

pub fn check_rate_limit(
    registry: &RateLimitRegistry,
    app_id: &Uuid,
) -> Result<(), HttpResponse> {
    let bucket = registry.get_or_create(app_id);
    // A spend-Degraded app pays DEGRADE_FACTOR tokens per request.
    let cost = if registry.is_degraded(app_id) { DEGRADE_FACTOR } else { 1 };
    if bucket.try_acquire_n(cost) {
        Ok(())
    } else {
        Err(HttpResponse::TooManyRequests()
            .json(&serde_json::json!({"error": "rate limit exceeded"})))
    }
}

// ---------------------------------------------------------------------------
// Per-Rule Rate Limit Registry
// ---------------------------------------------------------------------------
//
// Layered on top of the global per-app `RateLimitRegistry`. The two are
// conceptually different and have different lifetimes:
//
// * Global per-app limit — platform-level DoS guard, set at gateway boot,
//   keyed by `app_id` only. Lives in `RateLimitRegistry`.
// * Per-rule limit — creator-defined business logic, declared on a
//   `Action::Worker.rate_limit` entry in the manifest, changes with every
//   deploy, keyed by `(app_id, rule_idx, bucket)` where `bucket` is
//   derived from `RateLimitPer` (client IP, authenticated user `sub`,
//   session id, or "app" for a single platform-wide bucket).
//
// Per-rule fires FIRST in the request path because it's cheaper for
// high-rule-rate cases — early reject before the global check. A rule
// that's already over its limit shouldn't pay for a second bucket lookup.

/// Composite key identifying a per-rule rate-limit bucket. Equal keys
/// share a `TokenBucket`; different keys (different app, different rule
/// in the same app, different IP/session) get their own buckets.
#[derive(Hash, PartialEq, Eq, Debug, Clone)]
pub struct PerRuleKey {
    pub app_id: Uuid,
    pub rule_idx: u32,
    /// Bucket discriminator derived from `RateLimitPer`:
    /// * `Ip` → request's client IP string
    /// * `User` → authenticated `sub:<jwt-sub>` (falls back to the session
    ///   cookie, then IP, for anonymous callers)
    /// * `Session` → `__Host-zeroship_app_session` cookie value (or fallback IP)
    /// * `App` → constant `"app"` (single bucket shared by all clients)
    pub bucket: String,
}

#[allow(missing_debug_implementations)]
pub struct PerRuleRateLimitRegistry {
    buckets: RwLock<HashMap<PerRuleKey, Arc<TokenBucket>>>,
}

impl Default for PerRuleRateLimitRegistry {
    fn default() -> Self { Self::new() }
}

impl std::fmt::Debug for PerRuleRateLimitRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PerRuleRateLimitRegistry").finish_non_exhaustive()
    }
}

impl PerRuleRateLimitRegistry {
    pub fn new() -> Self {
        Self { buckets: RwLock::new(HashMap::new()) }
    }

    /// Resolve the (rate, burst) parameters from the manifest's
    /// `RateLimit` config. Returns `None` when neither `rps` nor `rpm`
    /// is set — caller treats that as a no-op pass-through.
    ///
    /// Preference order:
    /// * `rps` wins when present (the brief calls this "probably more
    ///   useful" — granular enforcement).
    /// * `rpm` falls back to `rpm / 60` rounded up to at least 1 rps so
    ///   a 30 rpm rule still gets a non-zero refill rate.
    /// * Burst defaults to the rate (so a 1 rps rule has a 1-token
    ///   burst — single request, no warm-up).
    fn resolve_rate(rl: &RateLimit) -> Option<(u32, u32)> {
        if let Some(rps) = rl.rps {
            if rps == 0 { return None; }
            return Some((rps, rps));
        }
        if let Some(rpm) = rl.rpm {
            if rpm == 0 { return None; }
            // 60 rpm = 1 rps. Sub-60 rpm rounds up to 1 rps so the
            // bucket can ever fill.
            let rps = rpm.div_ceil(60);
            return Some((rps, rps));
        }
        None
    }

    fn get_or_create(&self, key: PerRuleKey, rate: u32, burst: u32) -> Arc<TokenBucket> {
        {
            let r = self.buckets.read().unwrap();
            if let Some(b) = r.get(&key) {
                return b.clone();
            }
        }
        let mut w = self.buckets.write().unwrap();
        w.entry(key)
            .or_insert_with(|| Arc::new(TokenBucket::new(rate, burst)))
            .clone()
    }

    /// Check + decrement the per-rule bucket for `(app_id, rule_idx,
    /// bucket_id)`. Builds the bucket lazily based on `rl`. Returns
    /// `Err` with a 429 response (with `Retry-After: 1`) on overflow.
    /// `rate_limit: None` (no `rps` and no `rpm`) is a pass-through.
    pub fn check(
        &self,
        app_id: &Uuid,
        rule_idx: u32,
        _per: RateLimitPer,
        bucket_id: &str,
        rl: &RateLimit,
    ) -> Result<(), HttpResponse> {
        let Some((rate, burst)) = Self::resolve_rate(rl) else {
            // Both rps and rpm absent → no enforcement. Cheap path.
            return Ok(());
        };
        let key = PerRuleKey {
            app_id: *app_id,
            rule_idx,
            bucket: bucket_id.to_string(),
        };
        let bucket = self.get_or_create(key, rate, burst);
        if bucket.try_acquire() {
            Ok(())
        } else {
            Err(HttpResponse::TooManyRequests()
                .header("retry-after", "1")
                .json(&serde_json::json!({"error": "rate limit exceeded"})))
        }
    }
}

// --- Concurrency Guard ---

impl std::fmt::Debug for ConcurrencyRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConcurrencyRegistry").finish_non_exhaustive()
    }
}

pub struct ConcurrencyRegistry {
    gauges: RwLock<HashMap<Uuid, Arc<AtomicU32>>>,
    /// Apps in spend-Degrade. A degraded app's EFFECTIVE ceiling is
    /// `(limit / DEGRADE_FACTOR).max(1)` instead of `limit` — the same gauge
    /// is compared against a smaller ceiling (no rebuild, instant recovery on
    /// `clear_degraded`).
    degraded: RwLock<HashSet<Uuid>>,
    limit: u32,
}

impl ConcurrencyRegistry {
    pub fn new(limit: u32) -> Self {
        Self {
            gauges: RwLock::new(HashMap::new()),
            degraded: RwLock::new(HashSet::new()),
            limit,
        }
    }

    fn get_or_create(&self, app_id: &Uuid) -> Arc<AtomicU32> {
        {
            let r = self.gauges.read().unwrap();
            if let Some(g) = r.get(app_id) {
                return g.clone();
            }
        }
        let mut w = self.gauges.write().unwrap();
        w.entry(*app_id)
            .or_insert_with(|| Arc::new(AtomicU32::new(0)))
            .clone()
    }

    /// Mark `app_id` degraded (`on = true`) or clear it. Idempotent.
    pub fn set_degraded(&self, app_id: &Uuid, on: bool) {
        let mut w = self.degraded.write().unwrap();
        if on {
            w.insert(*app_id);
        } else {
            w.remove(app_id);
        }
    }

    /// Convenience: clear `app_id`'s degraded flag (instant recovery).
    pub fn clear_degraded(&self, app_id: &Uuid) {
        self.set_degraded(app_id, false);
    }

    #[must_use]
    pub fn is_degraded(&self, app_id: &Uuid) -> bool {
        self.degraded.read().unwrap().contains(app_id)
    }

    /// Effective ceiling for `app_id`: the tightened `(limit /
    /// DEGRADE_FACTOR).max(1)` when degraded, else the global `limit`.
    #[must_use]
    fn effective_limit(&self, app_id: &Uuid) -> u32 {
        if self.is_degraded(app_id) {
            (self.limit / DEGRADE_FACTOR).max(1)
        } else {
            self.limit
        }
    }
}

pub struct ConcurrencyGuard {
    gauge: Arc<AtomicU32>,
}

impl Drop for ConcurrencyGuard {
    fn drop(&mut self) {
        self.gauge.fetch_sub(1, Ordering::Release);
    }
}

pub fn acquire_concurrency(
    registry: &ConcurrencyRegistry,
    app_id: &Uuid,
) -> Result<ConcurrencyGuard, HttpResponse> {
    let gauge = registry.get_or_create(app_id);
    let ceiling = registry.effective_limit(app_id);
    loop {
        let current = gauge.load(Ordering::Acquire);
        if current >= ceiling {
            return Err(HttpResponse::TooManyRequests()
                .json(&serde_json::json!({"error": "concurrency limit exceeded"})));
        }
        if gauge
            .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Ok(ConcurrencyGuard { gauge });
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — per-rule rate-limit registry
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn rl(rps: Option<u32>, rpm: Option<u32>, per: RateLimitPer) -> RateLimit {
        RateLimit { rps, rpm, per }
    }

    #[test]
    fn per_rule_rps_enforced() {
        // rps=2 → bucket capacity 2, refill 2/s. 5 rapid requests:
        // first 2 succeed (drain the burst), the rest 429 until refill.
        let reg = PerRuleRateLimitRegistry::new();
        let app = Uuid::nil();
        let lim = rl(Some(2), None, RateLimitPer::App);
        // First two within the burst succeed.
        assert!(reg.check(&app, 0, lim.per, "app", &lim).is_ok());
        assert!(reg.check(&app, 0, lim.per, "app", &lim).is_ok());
        // Next three in the same instant are rate-limited.
        for _ in 0..3 {
            let resp = reg
                .check(&app, 0, lim.per, "app", &lim)
                .expect_err("should be rate limited");
            assert_eq!(resp.status(), ntex::http::StatusCode::TOO_MANY_REQUESTS);
            assert_eq!(
                resp.headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok()),
                Some("1"),
                "Retry-After: 1 must be set on per-rule 429"
            );
        }
    }

    #[test]
    fn per_rule_rpm_to_rps_conversion() {
        // rpm=60, rps=None → 1 rps internally. Burst=1, so the first
        // request succeeds and the second within the same second 429s.
        let reg = PerRuleRateLimitRegistry::new();
        let app = Uuid::nil();
        let lim = rl(None, Some(60), RateLimitPer::App);
        assert!(reg.check(&app, 0, lim.per, "app", &lim).is_ok());
        let err = reg
            .check(&app, 0, lim.per, "app", &lim)
            .expect_err("second within 1s should 429");
        assert_eq!(err.status(), ntex::http::StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn per_rule_per_ip_separates_buckets() {
        // Two requests from different IPs with rps=1 each. Both within
        // their own burst → both succeed even though aggregate is 2.
        let reg = PerRuleRateLimitRegistry::new();
        let app = Uuid::nil();
        let lim = rl(Some(1), None, RateLimitPer::Ip);
        assert!(reg.check(&app, 0, lim.per, "1.1.1.1", &lim).is_ok());
        assert!(reg.check(&app, 0, lim.per, "2.2.2.2", &lim).is_ok());
        // A third request reusing IP "1.1.1.1" within the same second
        // 429s — confirms the keying actually used the IP discriminator
        // rather than collapsing both IPs into one bucket.
        let err = reg
            .check(&app, 0, lim.per, "1.1.1.1", &lim)
            .expect_err("repeat IP should be rate limited");
        assert_eq!(err.status(), ntex::http::StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn per_rule_per_session_separates_buckets() {
        // Different `__Host-zeroship_app_session` values → independent buckets even
        // when the request comes from the same machine.
        let reg = PerRuleRateLimitRegistry::new();
        let app = Uuid::nil();
        let lim = rl(Some(1), None, RateLimitPer::Session);
        assert!(reg.check(&app, 0, lim.per, "session-aaa", &lim).is_ok());
        assert!(reg.check(&app, 0, lim.per, "session-bbb", &lim).is_ok());
        // Reusing "session-aaa" within the same second is rate-limited.
        let err = reg
            .check(&app, 0, lim.per, "session-aaa", &lim)
            .expect_err("repeat session id should be rate limited");
        assert_eq!(err.status(), ntex::http::StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn per_rule_app_shares_bucket() {
        // RateLimitPer::App with bucket="app" → every caller hits the
        // same bucket. The router uses "app" verbatim regardless of
        // IP/session, so we feed "app" here too.
        let reg = PerRuleRateLimitRegistry::new();
        let app = Uuid::nil();
        let lim = rl(Some(1), None, RateLimitPer::App);
        assert!(reg.check(&app, 0, lim.per, "app", &lim).is_ok());
        // Even an "unrelated" caller (different IP, different session)
        // shares the bucket because the router collapses them all into
        // the same `bucket_id`.
        let err = reg
            .check(&app, 0, lim.per, "app", &lim)
            .expect_err("App-scoped bucket is shared platform-wide");
        assert_eq!(err.status(), ntex::http::StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn per_rule_no_limit_passes_through() {
        // rate_limit with both fields None → no enforcement, ever.
        let reg = PerRuleRateLimitRegistry::new();
        let app = Uuid::nil();
        let lim = rl(None, None, RateLimitPer::Ip);
        for _ in 0..1000 {
            assert!(reg.check(&app, 0, lim.per, "1.1.1.1", &lim).is_ok());
        }
    }

    /// #6: a Degraded app on a TINY-burst global bucket (capacity <
    /// DEGRADE_FACTOR) must still be admitted at least once — Degrade is a
    /// throttle, never a hard Block. Pre-fix, a degraded request cost
    /// `DEGRADE_FACTOR` tokens against a 1-token bucket and could never be
    /// satisfied (silent hard Block). The cost-clamp (`min(cost, capacity)`)
    /// guarantees admission regardless of the burst config.
    #[test]
    fn degraded_tiny_burst_still_admits_some_requests() {
        // rate=1, burst=1 → capacity 1 logical token, far below DEGRADE_FACTOR.
        let reg = RateLimitRegistry::new(1, 1);
        let app = Uuid::nil();
        reg.set_degraded(&app, true);
        assert!(
            reg.is_degraded(&app),
            "precondition: app is flagged degraded",
        );
        // The first degraded request must still be admitted (cost clamped to
        // the 1-token capacity), proving Degrade did not become a hard Block.
        assert!(
            check_rate_limit(&reg, &app).is_ok(),
            "a degraded app on a tiny-burst bucket must still admit a request \
             (Degrade is a throttle, not a hard Block)",
        );
        // It IS still throttled: the bucket is now drained, so the immediate
        // next request 429s (this is the throttle, not a permanent block).
        assert!(
            check_rate_limit(&reg, &app).is_err(),
            "the drained bucket throttles the next immediate request",
        );
    }

    /// G2: `check_account` is a pure match — Suspended 402s with
    /// `ACCOUNT_SUSPENDED`; Active/PastDue pass. PastDue is the grace window
    /// (NOT a block), distinguishing it from spend's Block.
    #[test]
    fn check_account_suspended_402s_others_pass() {
        let err = check_account(AccountState::Suspended)
            .expect_err("suspended must 402");
        assert_eq!(err.status(), ntex::http::StatusCode::PAYMENT_REQUIRED);
        assert!(check_account(AccountState::Active).is_ok(), "active passes");
        assert!(
            check_account(AccountState::PastDue).is_ok(),
            "past_due is the grace window, not a block",
        );
    }

    #[test]
    fn different_rules_separate_buckets() {
        // Same shape (rps=1, App), same app, different rule_idx →
        // independent buckets. A rule-0 burst doesn't drain rule-1.
        let reg = PerRuleRateLimitRegistry::new();
        let app = Uuid::nil();
        let lim = rl(Some(1), None, RateLimitPer::App);
        assert!(reg.check(&app, 0, lim.per, "app", &lim).is_ok());
        // Rule 1's bucket is fresh.
        assert!(reg.check(&app, 1, lim.per, "app", &lim).is_ok());
        // Repeating rule 0 in the same second 429s.
        let err = reg
            .check(&app, 0, lim.per, "app", &lim)
            .expect_err("rule 0 already drained");
        assert_eq!(err.status(), ntex::http::StatusCode::TOO_MANY_REQUESTS);
        // Rule 1 still has its own state.
        let err = reg
            .check(&app, 1, lim.per, "app", &lim)
            .expect_err("rule 1 also at burst capacity 1");
        assert_eq!(err.status(), ntex::http::StatusCode::TOO_MANY_REQUESTS);
    }
}
