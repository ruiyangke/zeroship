use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use ntex::web::HttpResponse;
use uuid::Uuid;
use zeroship_core::types::{RateLimit, RateLimitPer};

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
        loop {
            let state = self.state.load(Ordering::Acquire);
            let (tokens, last) = unpack(state);
            let now = now_secs();
            let elapsed = u64::from(now.saturating_sub(last));
            let refilled = (tokens + elapsed * self.refill_rate).min(self.capacity);
            if refilled < 1000 {
                return false;
            }
            let new_state = pack(refilled - 1000, now);
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
    default_rate: u32,
    default_burst: u32,
}

impl RateLimitRegistry {
    pub fn new(rate: u32, burst: u32) -> Self {
        Self {
            buckets: RwLock::new(HashMap::new()),
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
}

pub fn check_rate_limit(
    registry: &RateLimitRegistry,
    app_id: &Uuid,
) -> Result<(), HttpResponse> {
    let bucket = registry.get_or_create(app_id);
    if bucket.try_acquire() {
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
//   derived from `RateLimitPer` (client IP, session id, or "app" for a
//   single platform-wide bucket).
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
    /// * `Session` → `__zs_session` cookie value (or fallback IP)
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
    limit: u32,
}

impl ConcurrencyRegistry {
    pub fn new(limit: u32) -> Self {
        Self {
            gauges: RwLock::new(HashMap::new()),
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
    loop {
        let current = gauge.load(Ordering::Acquire);
        if current >= registry.limit {
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
        // Different `__zs_session` values → independent buckets even
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
