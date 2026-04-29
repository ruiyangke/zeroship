# Billing & Metering System

## Goal

Measure, aggregate, and bill usage across all platform resources — core compute (requests, CPU, egress) and plugins (KV, DB, queue, object storage). Support flexible pricing models from free tier to enterprise.

## Architecture

```
V8 App Code
  │
  ├─ fetch()       ──→ Meter.increment("fetch.requests", 1)
  ├─ kv.get()      ──→ Meter.increment("kv.reads", 1)
  ├─ db.query()    ──→ Meter.increment("db.queries", 1)
  ├─ queue.send()  ──→ Meter.increment("queue.messages_sent", 1)
  │
  ▼
Thread-local counters (HashMap<String, u64>)
  │
  │ flush every 10s
  ▼
Control plane → Postgres `usage` table
  │
  │ hourly aggregation
  ▼
Rating engine (pricing × usage = cost)
  │
  │ end of billing period
  ▼
Stripe / Lago / OpenMeter → Invoice → Payment
```

## Metering Trait

Plugins don't know about billing. They increment counters via a trait injected into the V8 runtime:

```rust
pub trait Meter: Send + Sync {
    fn increment(&self, metric: &str, delta: u64);
}
```

Each V8 isolate gets a `Box<dyn Meter>` stored in `RuntimeState`. Every native callback (fetch, KV, DB, queue) calls `meter.increment()` with a namespaced metric name.

## Metrics Taxonomy

### Core metrics (measured by gateway + worker)

| Metric | Unit | Where measured |
|---|---|---|
| `requests` | count | Gateway (log-before-execute) |
| `cpu_us` | microseconds | Worker (V8 `dispatch_rpc` return value) |
| `wall_us` | microseconds | Gateway (`Instant::now()` before/after) |
| `egress_bytes` | bytes | Gateway (response body length) |
| `ingress_bytes` | bytes | Gateway (request body length) |

### Plugin metrics (measured by native callbacks)

#### KV Store
| Metric | Unit | Where |
|---|---|---|
| `kv.reads` | count | `kv_get` callback |
| `kv.writes` | count | `kv_put` callback |
| `kv.deletes` | count | `kv_delete` callback |
| `kv.read_bytes` | bytes | value.len() on get |
| `kv.write_bytes` | bytes | value.len() on put |
| `kv.storage_bytes` | bytes | periodic scan (not per-request) |

#### Database
| Metric | Unit | Where |
|---|---|---|
| `db.queries` | count | `db_query` callback |
| `db.rows_read` | count | result set size |
| `db.rows_written` | count | affected rows on INSERT/UPDATE |
| `db.storage_bytes` | bytes | periodic scan |

#### Message Queue
| Metric | Unit | Where |
|---|---|---|
| `queue.messages_sent` | count | `queue_send` callback |
| `queue.messages_received` | count | `queue_receive` callback |
| `queue.message_bytes` | bytes | payload.len() |

#### Object Storage
| Metric | Unit | Where |
|---|---|---|
| `blob.reads` | count | `blob_get` callback |
| `blob.writes` | count | `blob_put` callback |
| `blob.read_bytes` | bytes | blob size on get |
| `blob.write_bytes` | bytes | blob size on put |
| `blob.storage_bytes` | bytes | periodic scan |

#### External Fetch
| Metric | Unit | Where |
|---|---|---|
| `fetch.requests` | count | `fetch` callback |
| `fetch.egress_bytes` | bytes | request body size |
| `fetch.ingress_bytes` | bytes | response body size |

### Metric naming convention

`{plugin}.{resource}.{operation}` or `{plugin}.{dimension}`

- Namespace by plugin: `kv.*`, `db.*`, `queue.*`, `blob.*`, `fetch.*`
- Core metrics have no namespace: `requests`, `cpu_us`, `wall_us`, `egress_bytes`, `ingress_bytes`

## Data Model

### Usage event (per request, in memory)

```rust
pub struct AppUsage {
    // Core (fixed schema, always present)
    pub requests: u64,
    pub cpu_us: u64,
    pub wall_us: u64,
    pub egress_bytes: u64,
    pub ingress_bytes: u64,
    
    // Plugin (dynamic schema, varies by app)
    pub plugin_counters: HashMap<String, u64>,
}
```

### Storage (Postgres)

```sql
-- Per-resource counters, upserted on each flush
CREATE TABLE usage (
    app_id    UUID NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    resource  TEXT NOT NULL,          -- "requests", "kv.reads", "db.queries", etc.
    value     BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (app_id, resource)
);

-- Monthly snapshots for billing history
CREATE TABLE usage_history (
    app_id     UUID NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    period     TEXT NOT NULL,          -- "2026-04"
    counters   JSONB NOT NULL,         -- { "requests": 50000, "kv.reads": 1200, ... }
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

Both tables already exist. The `resource` column accepts any metric name — core or plugin. No schema change needed when adding new plugins.

## Collection Pipeline

### Where each metric is recorded

```
User request arrives at gateway:
  │
  ├─ Gateway (before dispatch):
  │   requests += 1                        ← log-before-execute
  │   ingress_bytes += body.len()
  │   wall_start = Instant::now()
  │   request_id = Uuid::new_v4()          ← idempotency key
  │
  ├─ Worker (during dispatch):
  │   V8 executes app code:
  │     fetch() → meter.increment("fetch.requests", 1)
  │     kv.get() → meter.increment("kv.reads", 1)
  │     kv.get() → meter.increment("kv.read_bytes", N)
  │   
  │   Returns: cpu_time in X-Cpu-Time-Ms header
  │
  ├─ Gateway (after response):
  │   egress_bytes += response.len()
  │   wall_us = wall_start.elapsed()
  │   cpu_us = parse X-Cpu-Time-Ms header
  │
  └─ Gateway accumulates into thread-local counters
```

### Flush pipeline

```
Worker (per thread):
  Thread-local HashMap<Uuid, AppUsage>
    │
    │ every 10 seconds
    ▼
  POST /internal/usage to control plane
  {
    "worker_id": "w1-thread-3",
    "counters": {
      "app-uuid-1": {
        "requests": 142,
        "cpu_us": 50000,
        "kv.reads": 30,
        "kv.read_bytes": 15000
      }
    }
  }
    │
    ▼
Control plane:
  INSERT INTO usage ... ON CONFLICT DO UPDATE SET value = value + $delta
```

### Alternative: gateway collects (recommended)

Since the gateway already has request/ingress/egress/wall metrics, and the worker returns cpu_time + plugin counters in response headers:

```
Worker response headers:
  X-Cpu-Time-Ms: 2.5
  X-Plugin-Usage: kv.reads=3,kv.read_bytes=1500,fetch.requests=1

Gateway parses → accumulates all metrics → flushes to control
```

This centralizes metering in the gateway. Workers don't need a flush loop — they just return per-request plugin counters in headers. Simpler, fewer moving parts.

## Pricing

### Pricing table

```rust
pub struct PricingTable {
    /// Metric name → price per unit per billing period
    pub prices: HashMap<String, PriceEntry>,
}

pub struct PriceEntry {
    pub unit_name: String,         // "per million", "per GB", "per query"
    pub price_per_unit: f64,       // in dollars
    pub unit_size: u64,            // 1_000_000 for "per million", 1_073_741_824 for "per GB"
    pub free_tier: u64,            // units included free
}
```

### Default pricing (Cloudflare-comparable)

| Metric | Unit size | Free tier | Price |
|---|---|---|---|
| `requests` | 1,000,000 | 100,000/day | $0.50/M |
| `cpu_us` | 1,000,000,000 (= 1000s) | 10ms/request | $12.50/M_ms |
| `egress_bytes` | 1,073,741,824 (1 GB) | 1 GB/day | $0.09/GB |
| `kv.reads` | 1,000,000 | 100,000/day | $0.50/M |
| `kv.writes` | 1,000,000 | 10,000/day | $5.00/M |
| `kv.storage_bytes` | 1,073,741,824 (1 GB) | 1 GB | $0.50/GB/mo |
| `db.queries` | 1,000,000 | 5,000,000/mo | $0.001/query |
| `db.rows_read` | 1,000,000 | 25,000,000/mo | $0.001/M |
| `db.storage_bytes` | 1,073,741,824 (1 GB) | 500 MB | $0.75/GB/mo |
| `queue.messages_sent` | 1,000,000 | 100,000/mo | $0.40/M |
| `blob.storage_bytes` | 1,073,741,824 (1 GB) | 10 GB | $0.015/GB/mo |

### Cost calculation

```rust
fn calculate_cost(usage: &HashMap<String, u64>, pricing: &PricingTable) -> f64 {
    let mut total = 0.0;
    for (metric, &value) in usage {
        if let Some(price) = pricing.prices.get(metric) {
            let billable = value.saturating_sub(price.free_tier);
            let units = billable as f64 / price.unit_size as f64;
            total += units * price.price_per_unit;
        }
    }
    total
}
```

## Spending Limits

### Per-app spending control

```rust
pub struct SpendingLimit {
    pub limit_dollars: f64,        // $5.00 for free tier
    pub action: SpendAction,       // Allow, Warn, Block
    pub warn_threshold: f64,       // 0.8 = warn at 80% of limit
}

pub enum SpendAction {
    Allow,    // normal operation
    Warn,     // add X-Spending-Warning header
    Block,    // reject requests with 429
}
```

### Enforcement flow

```
Control plane (every 60s):
  For each app:
    usage = SELECT resource, value FROM usage WHERE app_id = $1
    cost = calculate_cost(usage, pricing)
    if cost >= limit:
      set spend_action = Block
    elif cost >= limit * 0.8:
      set spend_action = Warn

Gateway (on each request):
  Routes include spend_action per app
  if spend_action == Block:
    return 429 { "error": "spending limit reached" }
  if spend_action == Warn:
    add header: X-Spending-Warning: "approaching limit"
    proxy normally
```

## Idempotency

- Each request gets a `request_id` (UUID) assigned at the gateway.
- This is the dedup key. If a flush is retried (e.g., control plane was down), the control plane can deduplicate by checking if that request_id was already counted.
- For v1: accept at-most-once (if flush fails, those 10s of counters are lost). At our scale this is immaterial.
- For v2: add a local WAL (append-only file) that survives worker crashes. Replay unshipped entries on restart.

## Monthly Rollover

```
At the start of each billing period (monthly):
  1. Read all counters for all apps
  2. Store snapshot in usage_history (JSONB)
  3. Reset counters to 0
  4. Send aggregated usage to Stripe/Lago for invoicing
```

Already implemented in the platform crate (`metering/rollover.rs`).

## Implementation Order

1. **Meter trait** — add to runtime, inject into RuntimeState
2. **Thread-local counters in worker** — accumulate core metrics in handler.rs
3. **Wire fetch() to meter** — first plugin metric
4. **Flush to control plane** — POST /internal/usage every 10s
5. **Worker returns plugin counters in response header** — gateway aggregates
6. **Pricing table + cost calculation** — in control plane
7. **Spending limit enforcement** — in gateway via /internal/routes
8. **KV, DB, Queue plugin metering** — each calls meter.increment()
9. **Monthly rollover** — port from existing platform crate
10. **Stripe/Lago integration** — push hourly summaries

## Scope Exclusions (v1)

- Per-request event log (Kafka/Kinesis) — flush aggregated counters instead
- WAL for crash recovery — accept at-most-once for v1
- Real-time cost dashboard — use /api/apps/:id/usage endpoint
- Custom pricing per app — use plan-level pricing
- Prepaid/drawdown billing — postpaid only for v1
