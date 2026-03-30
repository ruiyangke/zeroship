# Multi-Tenant Quota & Metering System

## Overview

A comprehensive metering, quota enforcement, and billing-ready system for the appbase platform. Tracks resource usage per app, enforces configurable limits, and provides usage data for billing.

## Research: How Others Do It

### Cloudflare Workers
- **Billing unit:** CPU time (ms) — never charged for I/O wait
- **Free tier:** 100K requests/day, 10ms CPU per request
- **Paid:** $5/mo + $0.30/M requests + $0.02/M CPU-ms beyond 30M included
- **Limits:** 128MB memory, 30s CPU (configurable to 5min)
- **Key insight:** CPU-time billing is fairest — you don't pay for network wait

### AWS Lambda
- **Billing unit:** GB-seconds (memory × duration) + requests
- **Free tier:** 1M requests + 400K GB-seconds/mo
- **Paid:** $0.20/M requests + $0.0000166667/GB-s
- **Key insight:** Memory tier affects cost — bigger function = more expensive

### Vercel
- **Billing unit:** Function invocations + CPU-hours + bandwidth
- **Free tier:** 100K invocations, 100 CPU-hours
- **Paid:** $20/mo + $0.60/M invocations + $0.128/CPU-hour
- **Key insight:** No hard spending limits — can get surprise bills

### Supabase
- **Billing unit:** Database size + MAU + storage + egress
- **Free tier:** 500MB DB, 50K MAU, 1GB storage
- **Paid:** $25/mo/project + usage-based scaling
- **Key insight:** Resource-based (fixed provisioning) vs operation-based (per call)

---

## Design

### Metered Dimensions

Every request records these metrics per app:

| Dimension | Unit | How measured | Comparable to |
|---|---|---|---|
| **CPU time** | milliseconds | `CLOCK_THREAD_CPUTIME_ID` per request | CF Workers |
| **Wall time** | milliseconds | `Instant::now()` per request | Lambda duration |
| **Requests** | count | +1 per RPC call | All platforms |
| **Memory** | MB (peak) | V8 heap statistics per isolate | Lambda memory |
| **DB storage** | bytes | SQLite file size | Supabase |
| **DB reads** | count | op_db_find calls | Firebase reads |
| **DB writes** | count | op_db_insert/update/delete calls | Firebase writes |
| **KV operations** | count | op_kv_* calls | CF KV |
| **Egress** | bytes | response body size | Vercel bandwidth |

### Quota Plans

```rust
struct QuotaPlan {
    name: String,               // "free", "pro", "enterprise"

    // Per-request limits (hard kill)
    max_cpu_per_request_ms: u64,     // e.g., 10ms free, 30s paid
    max_wall_time_per_request_ms: u64, // e.g., 30s

    // Monthly quotas (soft → warning → hard)
    monthly_requests: u64,           // e.g., 100K free, 10M paid
    monthly_cpu_ms: u64,             // e.g., 10K free, 30M paid
    monthly_egress_bytes: u64,       // e.g., 1GB free, 100GB paid
    monthly_db_reads: u64,           // e.g., 500K free, unlimited paid
    monthly_db_writes: u64,          // e.g., 50K free, unlimited paid
    monthly_kv_ops: u64,             // e.g., 100K free, 10M paid

    // Resource limits (static)
    max_db_storage_bytes: u64,       // e.g., 500MB free, 10GB paid
    max_memory_mb: u64,              // e.g., 128MB
    max_isolates: u64,               // e.g., 1 free, unlimited paid
}
```

### Example Plans

| | Free | Pro ($20/mo) | Enterprise |
|---|---|---|---|
| **CPU/request** | 10ms | 30s | 5min |
| **Monthly requests** | 100K | 10M | Unlimited |
| **Monthly CPU** | 10K ms | 30M ms | Unlimited |
| **Monthly egress** | 1GB | 100GB | Unlimited |
| **DB storage** | 500MB | 10GB | 100GB |
| **DB reads/mo** | 500K | Unlimited | Unlimited |
| **DB writes/mo** | 50K | 5M | Unlimited |
| **Memory/isolate** | 128MB | 128MB | 256MB |

### Architecture

```
Request flow:

HTTP request
  ↓
QuotaMiddleware (Tower layer)
  ├── Check: is app over monthly quota? → 429 Too Many Requests
  ├── Check: is app rate-limited? → 429
  ↓
IsolatePool.dispatch(app_id, body)
  ↓
V8 executes → returns RpcResult { json, cpu_time }
  ↓
MeterRecorder.record(app_id, metrics)
  ├── Increment request count
  ├── Add CPU time
  ├── Add egress bytes
  ├── Increment DB read/write counts (from OpState counters)
  ↓
QuotaMiddleware
  ├── Add X-CPU-Time header
  ├── Add X-Quota-Remaining headers
  ↓
HTTP response
```

### Components

#### 1. `Meter` — Per-app usage counters

```rust
/// Accumulated usage for a billing period (typically monthly).
struct AppUsage {
    period_start: DateTime,
    requests: AtomicU64,
    cpu_time_us: AtomicU64,      // microseconds
    wall_time_us: AtomicU64,
    egress_bytes: AtomicU64,
    db_reads: AtomicU64,
    db_writes: AtomicU64,
    kv_ops: AtomicU64,
    peak_memory_bytes: AtomicU64,
    db_storage_bytes: AtomicU64,
}
```

Uses `AtomicU64` for lock-free concurrent updates from multiple handler threads.

#### 2. `QuotaEnforcer` — Checks limits before/after requests

```rust
enum QuotaDecision {
    Allow,
    Warn { dimension: String, usage_pct: f64 },
    Deny { dimension: String, message: String },
}
```

Three thresholds per dimension:
- **80%:** Warning header (`X-Quota-Warning`)
- **100%:** Deny request (`429 Too Many Requests`)
- **Per-request:** Hard kill via CPU time limit (existing)

#### 3. `MeterStore` — Persistent usage storage

```rust
trait MeterStore: Send + Sync {
    /// Record usage for an app.
    async fn record(&self, app_id: &str, usage: &UsageDelta);
    /// Get current period usage for an app.
    async fn get_usage(&self, app_id: &str) -> AppUsage;
    /// Get the quota plan for an app.
    async fn get_plan(&self, app_id: &str) -> QuotaPlan;
    /// Reset usage for a new billing period.
    async fn reset_period(&self, app_id: &str);
}
```

Implementations:
- `InMemoryMeterStore` — for development/testing
- `SqliteMeterStore` — persistent, per-platform database
- Future: `PostgresMeterStore`, `RedisMeterStore`

#### 4. `QuotaMiddleware` — Tower layer

Wraps every request with quota checking and metering:

```rust
// Before request:
//   - Load app's plan + current usage
//   - Check if any monthly quota exceeded → 429
//   - Check rate limit (requests/second) → 429

// After request:
//   - Record: requests++, cpu_time, egress_bytes
//   - Add response headers:
//     X-CPU-Time: 0.82ms
//     X-Requests-Remaining: 9423
//     X-Quota-Reset: 2026-04-01T00:00:00Z
```

#### 5. Per-op counters in plugins

Plugins increment counters in `OpState` during execution:

```rust
// In op_db_find:
if let Ok(counter) = state.try_borrow::<DbOpCounter>() {
    counter.reads.fetch_add(1, Ordering::Relaxed);
}
```

After the request completes, the meter reads these counters.

### Response Headers

Every RPC response includes quota headers:

```
HTTP/1.1 200 OK
X-CPU-Time: 0.82ms
X-Wall-Time: 14.2ms
X-Requests-Used: 576
X-Requests-Limit: 100000
X-Requests-Remaining: 99424
X-CPU-Used-Ms: 1240
X-CPU-Limit-Ms: 10000
X-Quota-Reset: 2026-04-01T00:00:00Z
```

### Rate Limiting

In addition to monthly quotas, per-second rate limits prevent burst abuse:

| Plan | Requests/second | Burst |
|---|---|---|
| Free | 10 req/s | 50 |
| Pro | 1000 req/s | 5000 |
| Enterprise | Unlimited | — |

Implementation: token bucket algorithm per app, stored in the pool.

### Quota Exceeded Response

```json
{
  "jsonrpc": "2.0",
  "error": {
    "code": -32429,
    "message": "Monthly request quota exceeded (100000/100000). Upgrade to Pro for 10M requests/mo.",
    "data": {
      "dimension": "requests",
      "used": 100000,
      "limit": 100000,
      "reset": "2026-04-01T00:00:00Z",
      "upgrade_url": "https://appbase.dev/pricing"
    }
  },
  "id": 1
}
```

### Admin API

```
GET  /_admin/apps                    → list all apps with usage
GET  /_admin/apps/{id}/usage         → detailed usage for an app
PUT  /_admin/apps/{id}/plan          → change an app's plan
POST /_admin/apps/{id}/reset         → reset usage counters
GET  /_admin/usage/summary           → platform-wide usage summary
```

### Crate Structure

```
crates/
  core/
    quota.rs      ← QuotaPlan, AppUsage, QuotaDecision types
    meter.rs      ← MeterStore trait

  metering/       ← NEW CRATE
    store.rs      ← InMemoryMeterStore, SqliteMeterStore
    enforcer.rs   ← QuotaEnforcer logic
    middleware.rs  ← Tower QuotaMiddleware layer
    rate_limit.rs ← Token bucket rate limiter
    headers.rs    ← X-CPU-Time, X-Quota-* header injection
    counters.rs   ← AtomicU64 per-app counters
```

### Implementation Priority

| Phase | What | Why |
|---|---|---|
| **1** | Core types + in-memory counters | Foundation |
| **2** | QuotaMiddleware + response headers | Visible to users |
| **3** | Per-request CPU/wall limit enforcement | Safety |
| **4** | Monthly quota tracking + 429 responses | Billing readiness |
| **5** | Rate limiting (token bucket) | Abuse prevention |
| **6** | Admin API | Platform management |
| **7** | Persistent MeterStore (SQLite) | Survives restarts |
| **8** | Per-op counters in plugins | Granular billing |

---

## References

- [Cloudflare Workers Pricing](https://developers.cloudflare.com/workers/platform/pricing/)
- [AWS Lambda Pricing](https://aws.amazon.com/lambda/pricing/)
- [Vercel Pricing](https://vercel.com/pricing)
- [Supabase Pricing](https://supabase.com/pricing)
- [Cloudflare Workers Limits](https://developers.cloudflare.com/workers/platform/limits/)
- [CF Workers CPU-time billing blog](https://blog.cloudflare.com/workers-pricing-scale-to-zero/)
