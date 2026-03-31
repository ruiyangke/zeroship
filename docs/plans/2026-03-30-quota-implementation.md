# Quota System Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement the quota system spec (v2.4) — TOML-configured plans, MeterStore trait with pluggable storage, packed AtomicU64 token bucket, CAS-based concurrency guard, IETF rate limit headers, and admin API with RBAC.

**Architecture:** The metering pipeline has 3 tiers: hot (atomic counters for real-time enforcement), warm (MeterStore trait for persistent storage), cold (event log for audit). Plans are loaded from `appbase.toml` at startup. The server enforces quotas inline on every request with <3μs overhead.

**Tech Stack:** Rust, AtomicU64, CAS loops, TOML (config), axum (HTTP), serde

---

### Task 1: MeterStore Trait in Core

**Files:**
- Create: `crates/core/src/meter_store.rs`
- Modify: `crates/core/src/lib.rs`

**What:** Define the `MeterStore` trait that all storage adapters implement. This goes in core because it's a shared interface.

```rust
// crates/core/src/meter_store.rs
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceDelta {
    pub resource: String,
    pub delta: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeriodSnapshot {
    pub app_id: String,
    pub period: String,          // "2026-03" or "2026-03-30"
    pub counters: HashMap<String, u64>,
}

pub trait MeterStore: Send + Sync {
    fn flush(&self, app_id: &str, deltas: &[ResourceDelta]) -> Result<(), String>;
    fn load(&self, app_id: &str) -> Result<HashMap<String, u64>, String>;
    fn rollover(&self, app_id: &str) -> Result<PeriodSnapshot, String>;
    fn history(&self, app_id: &str, periods: u32) -> Result<Vec<PeriodSnapshot>, String>;
    fn close(&self) -> Result<(), String>;
}
```

**Commit:** `feat: add MeterStore trait to core crate`

---

### Task 2: TOML Config Parsing

**Files:**
- Modify: `crates/core/src/config.rs`
- Create: `crates/metering/src/config.rs`
- Create: `examples/appbase.toml`

**What:** Add plan definitions, policy definitions, and app-to-plan mapping to the config. Parse from TOML file.

The metering config module parses `[plans.*]`, `[policies.*]`, `[defaults]`, `[apps.*]`, and `[metering]` sections into typed Rust structs.

```rust
// crates/metering/src/config.rs
use crate::plan::QuotaPlan;
use std::collections::HashMap;

pub struct MeteringConfig {
    pub plans: HashMap<String, QuotaPlan>,
    pub policies: HashMap<String, PolicyDef>,
    pub default_plan: String,
    pub app_plans: HashMap<String, AppConfig>,
    pub store: String,           // "memory", "sqlite", "redis"
    pub store_config: toml::Value,
}

pub struct AppConfig {
    pub plan: String,
    pub overrides: HashMap<String, LimitOverride>,
}

pub fn load_config(path: &str) -> Result<MeteringConfig, String> { ... }
```

Create `examples/appbase.toml` with free/pro/enterprise plans as a reference config.

**Commit:** `feat: TOML config parsing for plans, policies, and app assignments`

---

### Task 3: Packed AtomicU64 Token Bucket (spec-compliant)

**Files:**
- Rewrite: `crates/metering/src/rate_limit.rs`

**What:** Replace the current Mutex-based token bucket with the packed single-AtomicU64 design from the spec. 32 bits for tokens (fixed-point ×1000), 32 bits for last_refill (epoch seconds). Single CAS loop with `compare_exchange`.

```rust
struct TokenBucket {
    state: AtomicU64,  // [tokens:32 | last_refill:32]
    capacity: u64,
    refill_rate: u64,
}

impl TokenBucket {
    fn try_acquire(&self) -> bool {
        loop {
            let state = self.state.load(Acquire);
            let (tokens, last_refill) = unpack(state);
            let now = current_time_secs();
            let elapsed = now.saturating_sub(last_refill);
            let refilled = (tokens + elapsed as u64 * self.refill_rate).min(self.capacity);
            if refilled < 1000 { return false; } // < 1 token
            let new_state = pack(refilled - 1000, now);
            if self.state.compare_exchange(state, new_state, AcqRel, Acquire).is_ok() {
                return true;
            }
        }
    }
}
```

Tests: within rate, burst exhaustion, separate apps, CAS contention.

**Commit:** `feat: packed AtomicU64 token bucket with CAS`

---

### Task 4: CAS-based Concurrency Guard

**Files:**
- Create: `crates/metering/src/concurrency.rs`
- Modify: `crates/metering/src/lib.rs`

**What:** RAII guard for concurrent request counting. Uses `compare_exchange` CAS loop (not fetch_add) to prevent TOCTOU race.

```rust
pub struct ConcurrencyGuard { gauge: Arc<AtomicU32> }

impl ConcurrencyGuard {
    pub fn try_acquire(gauge: &Arc<AtomicU32>, limit: u32) -> Option<Self> {
        loop {
            let current = gauge.load(Acquire);
            if current >= limit { return None; }
            if gauge.compare_exchange(current, current + 1, AcqRel, Acquire).is_ok() {
                return Some(Self { gauge: gauge.clone() });
            }
        }
    }
}

impl Drop for ConcurrencyGuard {
    fn drop(&mut self) { self.gauge.fetch_sub(1, Release); }
}
```

Tests: acquire up to limit, deny at limit, drop decrements, concurrent stress test.

**Commit:** `feat: CAS-based concurrency guard (RAII)`

---

### Task 5: Update QuotaPlan to Match Spec

**Files:**
- Rewrite: `crates/metering/src/plan.rs`

**What:** Restructure `QuotaPlan` to match the spec's concept separation: entitlements (boolean), quotas (cap + period + policy), rate limits (rate + burst).

```rust
pub struct QuotaPlan {
    pub name: String,
    pub version: u32,
    pub entitlements: HashMap<String, serde_json::Value>,
    pub quotas: HashMap<String, QuotaDef>,
    pub rate_limits: HashMap<String, RateLimitDef>,
}

pub struct QuotaDef {
    pub max: Option<u64>,    // None = unlimited
    pub period: Period,       // PerRequest, Daily, Monthly, Absolute
    pub policy: String,       // references a policy name
}

pub struct RateLimitDef {
    pub max_per_second: u32,
    pub burst: u32,
    pub policy: String,
}

pub enum Period { PerRequest, Daily, Monthly, Absolute }
```

Update `enforcer.rs` to use the new structure.

**Commit:** `feat: restructure QuotaPlan with entitlements, quotas, rate_limits`

---

### Task 6: Update Enforcer for New Plan Structure

**Files:**
- Rewrite: `crates/metering/src/enforcer.rs`

**What:** The enforcer reads the new `QuotaPlan` structure. Check each quota dimension against its plan limit. Support policy references (warn_then_block, hard_kill, etc.). Return distinct error codes per violation type (-32029 to -32034).

**Commit:** `feat: update enforcer for structured plans with policy references`

---

### Task 7: Memory MeterStore Adapter

**Files:**
- Create: `crates/metering/src/store/mod.rs`
- Create: `crates/metering/src/store/memory.rs`
- Modify: `crates/metering/src/lib.rs`

**What:** In-memory implementation of `MeterStore` for development and testing. Stores counters in a `HashMap<String, HashMap<String, u64>>` behind a `Mutex`.

**Commit:** `feat: add InMemoryMeterStore adapter`

---

### Task 8: Warm Tier Flush Loop

**Files:**
- Create: `crates/metering/src/flusher.rs`

**What:** Background task that runs every 5 seconds. Reads atomic counter deltas from the hot tier and writes them to the MeterStore. Uses `swap(0, AcqRel)` to atomically read and reset each counter.

```rust
pub fn spawn_flusher(
    registry: Arc<MeterRegistry>,
    store: Arc<dyn MeterStore>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            flush_all(&registry, &store);
        }
    })
}
```

**Commit:** `feat: warm tier flush loop (hot atomics → MeterStore)`

---

### Task 9: IETF RateLimit Response Headers

**Files:**
- Modify: `crates/server/src/router.rs`

**What:** Replace custom `x-requests-*` headers with IETF `RateLimit` + `RateLimit-Policy` headers per draft-ietf-httpapi-ratelimit-headers-10. Keep `x-cpu-time-ms` and `x-wall-time-ms` as custom headers.

```
RateLimit: limit=100000, remaining=99958, reset=1714521600
RateLimit-Policy: 100000;w=2592000
```

**Commit:** `feat: IETF RateLimit response headers (draft-10)`

---

### Task 10: Config-Driven Plan Loading in CLI

**Files:**
- Modify: `crates/cli/src/main.rs`

**What:** Load `appbase.toml` (if present) at startup. Parse plans, policies, app assignments. Pass to `MeterRegistry` and `IsolatePool`. Fall back to `QuotaPlan::unlimited()` if no config file.

```rust
// In cmd_serve:
let config = if Path::new("appbase.toml").exists() {
    load_config("appbase.toml")?
} else {
    MeteringConfig::default()  // unlimited, dev-friendly
};
```

**Commit:** `feat: load plans from appbase.toml at startup`

---

### Task 11: Admin API — Usage + Plan Management

**Files:**
- Modify: `crates/server/src/router.rs`

**What:** Add `/v1/` prefixed admin endpoints:
- `GET /v1/_admin/apps` — list all apps with usage
- `GET /v1/_admin/apps/{id}/usage` — per-app usage
- `PUT /v1/_admin/apps/{id}/plan` — change plan
- `POST /v1/_admin/apps/{id}/reset` — reset counters
- `GET /v1/_admin/plans` — list available plans
- `POST /v1/_admin/config/reload` — hot-reload config

**Commit:** `feat: admin API for usage and plan management`

---

### Task 12: End-to-End Test

**Files:**
- Create: `examples/bench/quota_test.sh`

**What:** Shell script that:
1. Creates `appbase.toml` with a free plan (100 req limit)
2. Starts the server
3. Sends 100 requests (should succeed)
4. Sends 1 more (should get 429)
5. Checks `/_admin/apps/default/usage` shows 100 requests
6. Changes plan to pro via `PUT /_admin/apps/default/plan`
7. Sends 1 more (should succeed now)

**Commit:** `test: end-to-end quota enforcement test`

---

## Summary

| Task | What | LOC est |
|------|------|---------|
| 1 | MeterStore trait in core | ~50 |
| 2 | TOML config parsing | ~200 |
| 3 | Packed AtomicU64 token bucket | ~120 |
| 4 | CAS concurrency guard | ~60 |
| 5 | Restructure QuotaPlan | ~100 |
| 6 | Update enforcer | ~150 |
| 7 | Memory MeterStore adapter | ~80 |
| 8 | Warm tier flush loop | ~60 |
| 9 | IETF RateLimit headers | ~40 |
| 10 | Config-driven plan loading | ~80 |
| 11 | Admin API endpoints | ~150 |
| 12 | E2E test | ~50 |
| **Total** | | **~1,140** |

Tasks 1-4 are foundations (no dependencies).
Tasks 5-6 restructure existing code.
Tasks 7-8 add the warm tier.
Tasks 9-11 wire into the server.
Task 12 validates everything.
