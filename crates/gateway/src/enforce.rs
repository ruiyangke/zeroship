use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use ntex::web::HttpResponse;
use uuid::Uuid;

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
