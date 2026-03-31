# Appbase Quota, Metering & Billing System — v2 Design

> **Status:** Draft v2.0 | **Last Updated:** 2026-03-30 | **Author:** Platform Team
>
> **Revision History:**
> - v2.0 (2026-03-30): Complete rewrite. Adds entitlements, spending limits, credits,
>   event log, webhook system, enforcement modes, tiered pricing, and operational guidance.
> - v1.0: Initial quota system design (see `quota-system.md`).

## Table of Contents

1. [Overview](#1-overview)
2. [Core Concepts](#2-core-concepts)
3. [Configuration](#3-configuration)
4. [Architecture](#4-architecture)
5. [Event Schema](#5-event-schema)
6. [Spending Limits & Budget Alerts](#6-spending-limits--budget-alerts)
7. [Credits & Prepaid Wallets](#7-credits--prepaid-wallets)
8. [Response Headers & Error Responses](#8-response-headers--error-responses)
9. [Admin API](#9-admin-api)
10. [Monitoring & Observability](#10-monitoring--observability)
11. [Billing Integration](#11-billing-integration)
12. [Webhook Notifications](#12-webhook-notifications)
13. [Security & Abuse Prevention](#13-security--abuse-prevention)
14. [Developer Experience](#14-developer-experience)
15. [Testing Strategy](#15-testing-strategy)
16. [Implementation Plan](#16-implementation-plan)
17. [Operational Guidance](#17-operational-guidance)
18. [Future Considerations](#18-future-considerations)
19. [Glossary](#19-glossary)
20. [References](#20-references)

---

## 1. Overview

This document specifies the quota, metering, and billing system for the Appbase
multi-tenant app hosting platform. It governs resource definition, usage measurement,
limit enforcement, plan management, spending controls, billing integration, and
operational observability.

**TL;DR:** The system uses three layers — (1) in-memory atomic counters for real-time
enforcement at sub-microsecond latency, (2) periodic SQLite flushes for durability, and
(3) an append-only event log for audit/billing. Plans bundle entitlements, quotas, rate
limits, and policies. Spending limits prevent bill shock. The platform meters everything
automatically; no SDK is needed for app developers. External billing systems (Stripe,
Lago) consume the usage data via Admin API and webhooks.

### 1.1 Design Goals

| # | Goal | Description |
|---|---|---|
| G1 | **Accuracy** | Every billable event is counted exactly once (idempotent metering) |
| G2 | **Real-time enforcement** | Per-request and rate limits decided in sub-microsecond time |
| G3 | **Auditability** | Immutable event log enables billing reconciliation and dispute resolution |
| G4 | **Extensibility** | New resources and pricing models without code changes |
| G5 | **Graceful degradation** | Configurable policies from soft warnings to hard kills |
| G6 | **Cost predictability** | Spending limits and budget alerts prevent bill shock |
| G7 | **Tenant isolation** | One app cannot affect another's metering, limits, or performance |
| G8 | **Operational transparency** | Every enforcement decision is observable, explainable, and reversible |

### 1.2 Non-Goals (v2)

- Multi-region distributed metering (single-node; see Section 18 for future direction)
- Real-time payment processing (invoices are generated; payment is external)
- Self-service plan creation by app developers (platform owner only in v2)
- Tax calculation (delegated to payment processor)
- Per-user metering within an app (metering is per-app, not per-end-user)

---

## 2. Core Concepts

### 2.1 Concept Map

```
                    ┌──────────┐
                    │   PLAN   │  A named, versioned bundle
                    └────┬─────┘
                         │ contains
          ┌──────────────┼──────────────┐
          ▼              ▼              ▼
   ┌─────────────┐ ┌──────────┐ ┌────────────┐
   │ ENTITLEMENT │ │  QUOTA   │ │ RATE LIMIT │
   │ (boolean)   │ │ (cap/    │ │ (throughput │
   │             │ │  window) │ │  cap)       │
   └─────────────┘ └────┬─────┘ └─────┬──────┘
                         │             │
                    references    references
                         │             │
                    ┌────▼─────┐       │
                    │ RESOURCE │◄──────┘
                    │ (what is │
                    │ consumed)│
                    └────┬─────┘
                         │
                    measured by
                         │
                    ┌────▼─────┐
                    │ METERING │  Always on, independent
                    │ (events  │  of plans
                    │ +counters│
                    └──────────┘

   When a quota or rate limit is crossed → POLICY defines what happens
```

### 2.2 Seven Distinct Concerns

| # | Concern | Description |
|---|---|---|
| 1 | **Resource** | A measurable dimension of consumption (CPU, requests, storage, ...) |
| 2 | **Metering** | Recording consumption events (always on, independent of plans) |
| 3 | **Entitlement** | Binary access control: can this app use feature X? |
| 4 | **Quota** | A numeric cap on a resource over a time window |
| 5 | **Rate Limit** | A throughput cap on a resource per unit time (with burst) |
| 6 | **Policy** | Enforcement behavior when a boundary is crossed |
| 7 | **Plan** | A named, versioned bundle of entitlements, quotas, rate limits, and policies |

Quota and Rate Limit are both "limits" but have fundamentally different enforcement
mechanics and are configured separately:
- **Quotas** check accumulated counters against a cap (budget-like).
- **Rate limits** check instantaneous throughput via token bucket (velocity-like).

### 2.3 Entitlements vs Quotas

**Entitlements** are boolean feature gates:
- "Can this app use custom domains?" "Can this app create cron jobs?"
- Checked once at request routing time, not metered continuously
- Enforcement: 403 Forbidden
- Can also encode numeric limits that are not usage-tracked (e.g., `max_apps = 3`)

**Quotas** are numeric boundaries on metered resources:
- "Max 100K requests/month" "Max 500MB database storage"
- Checked on every request against running counters
- Enforcement: configurable policy (warn, throttle, block, kill)

This distinction aligns with how Metronome and Stigg separate entitlements (access
decisions) from usage tracking (metering decisions).

### 2.4 Resources

A resource is a measurable dimension of consumption with the following attributes:

| Attribute | Description | Example |
|---|---|---|
| `name` | Unique identifier | `cpu_ms` |
| `unit` | Human-readable unit label | `"milliseconds"` |
| `aggregation` | How values combine across events | `sum`, `max`, `latest`, `count_unique` |
| `category` | Logical grouping for display | `compute`, `storage`, `network`, `database`, `custom` |
| `resettable` | Whether the counter resets at period boundaries | `true` for sum/max, `false` for latest |

**Aggregation semantics:**

| Type | AtomicOp | Behavior | Use For |
|---|---|---|---|
| `sum` | `fetch_add` | Accumulates all values | Requests, CPU, bytes |
| `max` | `fetch_max` | Keeps highest value seen | Peak memory |
| `latest` | `store` | Overwrites with newest value | Storage size (sampled) |
| `count_unique` | HyperLogLog | Approximate distinct count | Unique users, IPs |
| `gauge` | AtomicU32 inc/dec | Current instantaneous value | Concurrent connections |

Note: `latest` uses `AtomicU64::store(value, Ordering::Release)` with a separate
timestamp-guarded compare to ensure only newer values overwrite older ones. `count_unique`
uses a probabilistic HyperLogLog sketch, not an atomic counter.

**Built-in resources:**

| Resource | Unit | Category | Aggregation | Measurement Method |
|---|---|---|---|---|
| `cpu_ms` | ms | compute | sum | `CLOCK_THREAD_CPUTIME_ID` via `clock_gettime` |
| `wall_ms` | ms | compute | sum | `Instant::now()` delta |
| `memory_peak_mb` | MB | compute | max | V8 `GetHeapStatistics()` after execution |
| `requests` | count | compute | sum | +1 per RPC call |
| `concurrent_requests` | count | compute | gauge | RAII guard: inc on entry, dec on exit (see 17.5) |
| `egress_bytes` | bytes | network | sum | `Content-Length` or chunked body byte count |
| `ingress_bytes` | bytes | network | sum | Request body byte count |
| `subrequests` | count | network | sum | `op_fetch` call counter |
| `db_reads` | count | database | sum | `op_db_find`/`op_db_get` counter |
| `db_writes` | count | database | sum | `op_db_insert`/`op_db_update`/`op_db_delete` counter |
| `db_storage_bytes` | bytes | storage | latest | `stat()` on SQLite file (sampled every 60s) |
| `kv_reads` | count | database | sum | `op_kv_get`/`op_kv_list` counter |
| `kv_writes` | count | database | sum | `op_kv_set`/`op_kv_delete` counter |
| `kv_storage_bytes` | bytes | storage | latest | `stat()` on KV file (sampled every 60s) |

**Custom resources** are declared in config and tracked via the same pipeline:

```toml
[resources.ai_tokens]
unit = "count"
description = "AI API tokens consumed"
aggregation = "sum"
category = "custom"
```

### 2.5 Metering

Metering is **always active**, independent of plans. Every resource consumption is
recorded regardless of whether limits exist. This ensures:
- Usage data is available for future plan design and pricing decisions
- Plan changes never cause data loss
- Billing reconciliation has complete data even for resources added after the fact

**Three-tier pipeline:**

| Tier | Latency | Durability | Purpose |
|---|---|---|---|
| **Hot** — Atomic counters | ~10 ns | None (in-memory) | Real-time enforcement |
| **Warm** — Periodic flush | ~5 s | SQLite WAL | Survives restarts |
| **Cold** — Event log | ~100 ms | Append-only file | Audit trail, billing replay |

**Idempotency:** Every usage delta carries an `idempotency_key` (typically
`{request_id}_{resource}`). A bounded LRU deduplication set (capacity: 1M keys,
TTL: 24 hours) tracks seen keys. Duplicates are silently dropped at the hot tier.
The event log also stores the key for cold-tier deduplication during replay.

**Backpressure:** The event log enqueue is a bounded channel (capacity: 10,000). If the
channel is full (event log writer is slow), the metering pipeline:
1. Still updates atomic counters (enforcement is never delayed)
2. Drops the event with an `event_log_drop` counter increment
3. Logs a warning with the dropped event's key (recoverable from warm tier)

### 2.6 Quotas

A quota is a numeric cap on accumulated resource usage within a time window:

| Window | Reset Trigger | Example |
|---|---|---|
| `per_request` | Each request boundary | max 10ms CPU per request |
| `daily` | 00:00 UTC | max 100K requests/day |
| `monthly` | Billing period start | max 10M requests/month |
| `absolute` | Never (manual only) | max 500MB storage |

**Soft vs hard quotas:** When `overage` billing is enabled for a plan, monthly quotas
become "soft" — usage beyond the quota is allowed but billed at overage rates. The
enforcement policy changes from `block` to `allow` (with `notify`). When overage is
disabled, quotas are "hard" — usage is blocked at 100%.

### 2.7 Rate Limits

Rate limits are enforced via a **token bucket algorithm**:

```
Configuration:
  max_per_second = 10     # Sustained rate (refill rate)
  burst = 50              # Maximum burst (bucket capacity)

Behavior:
  - Bucket starts full (50 tokens)
  - Each request consumes 1 token
  - Tokens refill at 10/second
  - When empty → policy applies (usually "block" → 429)
  - Bucket can never exceed capacity
```

**Why token bucket:** Handles bursty traffic gracefully while enforcing a sustained rate.
Used by Cloudflare Workers, AWS API Gateway, and nginx. Simpler than sliding window log
with comparable fairness properties.

**Implementation:** Per-app `TokenBucket` struct with atomic state:
```rust
struct TokenBucket {
    tokens: AtomicU64,        // Fixed-point: tokens * 1000 for sub-token precision
    last_refill: AtomicU64,   // Timestamp in microseconds
    capacity: u64,            // = burst * 1000
    refill_rate: u64,         // = max_per_second * 1000
}
```

### 2.8 Policies

A policy defines enforcement behavior at configurable thresholds:

| Action | Behavior | HTTP | When To Use |
|---|---|---|---|
| `allow` | Normal processing | 200 | Default below any threshold |
| `warn` | Add warning header, allow | 200 | Early awareness (80%) |
| `notify` | Send webhook, allow | 200 | External system integration |
| `throttle` | Artificial delay (configurable) | 200 | Slow down before hard stop |
| `block` | Reject request | 429 | Hard quota enforcement |
| `block_writes` | Allow reads, reject writes | 429 on write | Storage preservation |
| `kill` | Terminate V8 mid-execution | 503 | Per-request CPU/wall limits |
| `degrade` | Switch to degraded limit set (see below) | 200 | Spending limit response |

**Threshold composition:**

```toml
[policies.progressive]
at_50_pct = "notify"
at_80_pct = "warn"
at_95_pct = "throttle"
at_100_pct = "block"
```

Enforcement evaluates thresholds in descending order and applies the first matching action.

**Degrade policy details:** When `degrade` is triggered (typically by spending limits),
the app's effective quotas and rate limits are replaced with a configurable degraded set:

```toml
[plans.pro.degraded]
# These limits apply when "degrade" policy is active
requests = { max_per_second = 10, burst = 50 }
cpu_ms_per_request = { max = 10 }
# Unspecified resources use the free plan's limits as fallback
fallback_plan = "free"
```

If no `[degraded]` section is configured, `degrade` falls back to the `free` plan's limits.

**Grace period for in-flight requests:** When a quota transitions from allow to block,
requests already in V8 execution complete normally. Only new incoming requests are
blocked. This prevents half-processed side effects and aligns with Cloudflare Workers'
behavior.

### 2.9 Plans

A plan is a named, versioned bundle:

| Component | Type | Description |
|---|---|---|
| `name` | String | Unique identifier (e.g., "free", "pro") |
| `version` | u32 | Incremented on any change to the plan definition |
| `description` | String | Human-readable description |
| `entitlements` | Map\<String, Value\> | Feature gates (bool or numeric) |
| `quotas` | Map\<String, Quota\> | Resource caps with windows and policies |
| `rate_limits` | Map\<String, RateLimit\> | Throughput caps with burst |
| `overage` | Option\<OverageConfig\> | Per-resource overage pricing |

**Plan versioning:** Plans have a `version` field. When a plan definition changes in
config, the version must be explicitly incremented. Existing apps remain on their assigned
version until migrated via admin API. This prevents surprise behavior changes. Migration
can be done per-app or in bulk:

```
PUT  /_admin/apps/{id}/plan         — Single app migration
POST /_admin/plans/{name}/migrate   — Bulk: migrate all apps on this plan to new version
```

**Per-app overrides:** Individual quotas can be overridden without creating a custom plan:

```toml
[apps.my_blog.overrides.quotas]
requests = { max = 200_000 }    # Override only this quota
```

Overrides are merged at enforcement time: override values replace plan values for the
specified resources; all other resources use the plan's definition.

**Plan downgrade when usage exceeds new limits:** When an app is moved to a plan with
lower limits than its current usage:
- **Monthly quotas:** The app is immediately over-quota. The configured policy applies
  (e.g., `warn_then_block` means the app is blocked until the next period reset).
- **Storage quotas:** The app is over its storage limit. The `block_writes` policy
  prevents further growth but does not delete existing data.
- **Rate limits:** New rate limits apply immediately. Token buckets are reinitialized.
- Admin API response includes a `warnings` field when a plan change would cause
  immediate enforcement.

### 2.10 Enforcement Modes

To support safe rollout and testing, the system supports three enforcement modes:

| Mode | Behavior | Use Case |
|---|---|---|
| `enforce` | Full enforcement (default) | Production |
| `dry_run` | Log decisions but always allow | Testing new limits |
| `shadow` | Enforce old plan, log decisions for new plan | Plan migration validation |

Configurable globally or per-app:

```toml
[defaults]
enforcement_mode = "enforce"

[apps.my_blog]
enforcement_mode = "dry_run"    # Testing new limits
```

**Mode details:**

- **`enforce`** (default): Full enforcement. Requests are blocked/throttled as configured.
- **`dry_run`**: Decisions are computed and logged but never applied. All requests are
  allowed. Response headers include `X-Enforcement-Mode: dry_run`. Useful for testing
  new quota configurations before enabling them.
- **`shadow`**: The current plan is enforced normally, but a second "shadow" plan is also
  evaluated. Both decisions are logged for comparison. This enables safe migration testing:

```toml
[apps.my_blog]
plan = "free"
enforcement_mode = "shadow"
shadow_plan = "free"
shadow_plan_version = 2    # Compare current free v1 against proposed free v2
```

Shadow mode emits Prometheus metrics with a `shadow="true"` label for A/B analysis.

---

## 3. Configuration

### 3.1 Complete Example

```toml
# appbase.toml

[server]
port = 3000
host = "0.0.0.0"
graceful_shutdown_timeout_secs = 30   # Drain in-flight requests before exit

[admin]
api_key_env = "APPBASE_ADMIN_API_KEY"  # Env var containing the API key
cors_origins = ["https://dashboard.example.com"]  # CORS for browser dashboard
rate_limit = 100                      # Admin API rate limit (req/s)
read_only = false                     # Set true for monitoring-only instances

[isolates]
max = 1000
idle_timeout_secs = 60

# ── Billing ──

[billing]
period = "calendar_month"       # "calendar_month" | "anniversary"
currency = "usd"
timezone = "UTC"                # For daily resets and period boundaries
overage_enabled = false         # Global default; per-plan override

# ── Spending Controls ──

[spending]
enabled = true
check_interval_secs = 60
default_alert_thresholds = [50, 75, 90, 100]
alert_channels = ["webhook"]

# ── Pricing (for spend calculation and invoice preview) ──
# Standard pricing: flat per-unit rate
# Tiered pricing: volume-based brackets (see commented example)

[pricing.requests]
per_million = 0.30

[pricing.cpu_ms]
per_million = 0.02

[pricing.egress]
per_gb = 0.09

[pricing.ingress]
per_gb = 0.00               # Free inbound

[pricing.db_reads]
per_million = 0.50

[pricing.db_writes]
per_million = 1.00

[pricing.db_storage]
per_gb_month = 0.25

[pricing.kv_reads]
per_million = 0.50

[pricing.kv_writes]
per_million = 1.00

[pricing.kv_storage]
per_gb_month = 0.10

[pricing.ai_tokens]
per_million = 3.00

# ── Tiered Pricing Example (for pro plan overage) ──
# [pricing.requests.tiers]
# 1 = { up_to = 10_000_000,  per_million = 0.00 }    # Included in plan
# 2 = { up_to = 50_000_000,  per_million = 0.25 }    # First overage tier
# 3 = { up_to = -1,          per_million = 0.15 }    # Volume discount tier (-1 = unlimited)

# ── Resources ──
# Built-in resources are auto-registered. Only custom resources need declaration.

[resources.ai_tokens]
unit = "count"
description = "AI API tokens consumed"
aggregation = "sum"
category = "custom"

[resources.email_sends]
unit = "count"
description = "Transactional emails sent"
aggregation = "sum"
category = "custom"

# ── Policies ──

[policies.warn_then_block]
at_80_pct = "warn"
at_100_pct = "block"

[policies.progressive]
at_50_pct = "notify"
at_80_pct = "warn"
at_95_pct = "throttle"
at_100_pct = "block"

[policies.hard_kill]
at_100_pct = "kill"

[policies.block_writes_only]
at_80_pct = "warn"
at_100_pct = "block_writes"

[policies.soft_block]
at_100_pct = "block"

# ── Plans ──

[plans.free]
description = "Free tier — development and small projects"
version = 1

[plans.free.entitlements]
custom_domains = false
cron_jobs = false
websockets = false
priority_support = false
max_apps = 3

[plans.free.quotas]
cpu_ms              = { max = 10_000,          period = "monthly",      policy = "warn_then_block" }  # 10s total
cpu_ms_per_request  = { resource = "cpu_ms",   max = 10,               period = "per_request",  policy = "hard_kill" }  # 10ms safety cap
wall_ms_per_request = { resource = "wall_ms",  max = 30_000,           period = "per_request",  policy = "hard_kill" }
requests            = { max = 100_000,         period = "monthly",     policy = "warn_then_block" }
requests_daily      = { resource = "requests", max = 10_000,           period = "daily",        policy = "warn_then_block" }
egress_bytes        = { max = 1_000_000_000,   period = "monthly",     policy = "warn_then_block" }
ingress_bytes       = { max = 500_000_000,     period = "monthly",     policy = "warn_then_block" }
db_reads            = { max = 500_000,         period = "monthly",     policy = "warn_then_block" }
db_writes           = { max = 50_000,          period = "monthly",     policy = "warn_then_block" }
db_storage_bytes    = { max = 500_000_000,     period = "absolute",    policy = "block_writes_only" }
kv_reads            = { max = 100_000,         period = "monthly",     policy = "warn_then_block" }
kv_writes           = { max = 50_000,          period = "monthly",     policy = "warn_then_block" }
kv_storage_bytes    = { max = 100_000_000,     period = "absolute",    policy = "block_writes_only" }
memory_peak_mb      = { max = 128,             period = "absolute",    policy = "hard_kill" }
subrequests         = { max = 50,              period = "per_request", policy = "soft_block" }
concurrent          = { resource = "concurrent_requests", max = 5,     period = "absolute",    policy = "soft_block" }

[plans.free.rate_limits]
requests = { max_per_second = 10, burst = 50, policy = "soft_block" }

[plans.pro]
description = "Pro tier — production apps"
version = 1

[plans.pro.entitlements]
custom_domains = true
cron_jobs = true
websockets = true
priority_support = false
max_apps = 25

[plans.pro.quotas]
cpu_ms              = { max = 30_000_000,       period = "monthly",     policy = "progressive" }
cpu_ms_per_request  = { resource = "cpu_ms",    max = 30_000,           period = "per_request", policy = "hard_kill" }
wall_ms_per_request = { resource = "wall_ms",   max = 60_000,           period = "per_request", policy = "hard_kill" }
requests            = { max = 10_000_000,       period = "monthly",     policy = "progressive" }
egress_bytes        = { max = 100_000_000_000,  period = "monthly",     policy = "progressive" }
db_storage_bytes    = { max = 10_000_000_000,   period = "absolute",    policy = "block_writes_only" }
kv_storage_bytes    = { max = 5_000_000_000,    period = "absolute",    policy = "block_writes_only" }
memory_peak_mb      = { max = 128,              period = "absolute",    policy = "hard_kill" }
subrequests         = { max = 1_000,            period = "per_request", policy = "soft_block" }
concurrent          = { resource = "concurrent_requests", max = 50,     period = "absolute",    policy = "soft_block" }

[plans.pro.rate_limits]
requests = { max_per_second = 1_000, burst = 5_000, policy = "soft_block" }

[plans.pro.overage]
enabled = true
requests   = { per_million = 0.30 }
cpu_ms     = { per_million = 0.02 }
egress     = { per_gb = 0.09 }
db_reads   = { per_million = 0.50 }
db_writes  = { per_million = 1.00 }

[plans.enterprise]
description = "Enterprise tier — custom limits, SLA"
version = 1

[plans.enterprise.entitlements]
custom_domains = true
cron_jobs = true
websockets = true
priority_support = true
max_apps = -1                   # -1 = unlimited

[plans.enterprise.quotas]
cpu_ms_per_request  = { resource = "cpu_ms",   max = 300_000,  period = "per_request", policy = "hard_kill" }
wall_ms_per_request = { resource = "wall_ms",  max = 300_000,  period = "per_request", policy = "hard_kill" }
memory_peak_mb      = { max = 256,             period = "absolute",    policy = "hard_kill" }
subrequests         = { max = 10_000,          period = "per_request", policy = "soft_block" }
concurrent          = { resource = "concurrent_requests", max = 500,   period = "absolute",    policy = "soft_block" }
# No monthly quotas — governed by contract + spending limits

[plans.enterprise.rate_limits]
requests = { max_per_second = 10_000, burst = 50_000, policy = "soft_block" }

# ── Defaults ──

[defaults]
plan = "free"
enforcement_mode = "enforce"    # "enforce" | "dry_run" | "shadow"

# ── Apps ──

[apps.my_todo]
plan = "pro"

[apps.vip_client]
plan = "enterprise"

[apps.my_blog]
plan = "free"
[apps.my_blog.overrides.quotas]
requests = { max = 200_000 }

[apps.my_blog.spending]
limit_usd = 0.00
action = "block"
auto_resume = true

# ── Webhooks ──

[webhooks]
url = "https://example.com/appbase-events"
secret = "whsec_..."                           # HMAC-SHA256 signing secret
events = ["quota.*", "spending.*", "billing.*"] # Event name prefix matching
timeout_ms = 10_000
max_retries = 5

# ── Metering ──

[metering]
flush_interval_secs = 5        # Counter → SQLite flush interval
dedup_capacity = 1_000_000     # Max entries in dedup LRU set
dedup_ttl_hours = 24           # TTL for dedup entries

# ── Event Log ──

[event_log]
enabled = true
retention_days = 90
max_size_mb = 1024
flush_interval_ms = 100        # Batch write interval
channel_capacity = 10_000      # Bounded async channel size
```

### 3.2 Configuration Validation Rules

Validated at startup and on hot reload. Failures are fatal at startup; rejected on reload
with a warning log (old config remains active).

**Hot reload behavior** (`POST /_admin/config/reload` or `SIGHUP`):
- New plans: added to the registry immediately
- Changed plans: new version required; existing apps stay on old version until migrated
- Removed plans: rejected if any app references them
- Changed policies: take effect immediately for all apps referencing them
- Changed pricing: takes effect at next spend calculation cycle
- Rate limit changes: token buckets reinitialized (may briefly allow a burst)
- Atomic counters: **never** reset by config reload

| # | Rule | Error If Violated |
|---|---|---|
| V1 | Every policy in a quota must reference a defined `[policies.*]` | Unknown policy "{name}" |
| V2 | Every `resource` field in a quota must match a built-in or custom resource | Unknown resource "{name}" |
| V3 | Quota `max` must be a positive integer (or -1 for unlimited) | Invalid max value |
| V4 | Rate limit `burst` must be >= `max_per_second` | Burst must be >= sustained rate |
| V5 | Plan `version` must be a positive integer | Invalid plan version |
| V6 | `spending.limit_usd` must be >= 0 | Negative spending limit |
| V7 | Alert thresholds must be in [1, 100], sorted ascending, no duplicates | Invalid thresholds |
| V8 | Entitlement keys must match `^[a-z][a-z0-9_]*$` | Invalid entitlement key |
| V9 | Resource names must match `^[a-z][a-z0-9_]*$` | Invalid resource name |
| V10 | Policy thresholds must be in (0, 100], sorted ascending | Invalid policy threshold |
| V11 | Apps must reference existing plans | Unknown plan "{name}" |
| V12 | Override quotas must reference resources that exist in the plan or globally | Unknown resource in override |
| V13 | Webhook URL must be valid HTTPS (HTTP allowed only for localhost) | Insecure webhook URL |
| V14 | `[pricing]` entries must reference defined resources | Unknown resource in pricing |

Admin API endpoint for pre-flight validation:
```
POST /_admin/config/validate
Body: raw TOML content
Response: { valid: bool, errors: [{ rule: "V1", message: "...", location: "..." }] }
```

---

## 4. Architecture

### 4.1 System Overview

```
┌───────────────────────────────────────────────────────────────────────┐
│                           REQUEST PATH                                │
│                                                                       │
│   HTTP Request                                                        │
│        │                                                              │
│        ▼                                                              │
│   ┌──────────────┐  Fail-fast. No V8 cost. Per-app token bucket.     │
│   │ Rate Limiter  │──→ 429 + RateLimit-* headers + Retry-After       │
│   └──────┬───────┘                                                    │
│          │ pass                                                       │
│          ▼                                                            │
│   ┌──────────────────┐  Atomic gauge: increment on entry.            │
│   │ Concurrency Guard │──→ 429 "Max concurrent requests"             │
│   └──────┬───────────┘                                                │
│          │ pass                                                       │
│          ▼                                                            │
│   ┌─────────────────────┐  Boolean check from plan entitlements.     │
│   │ Entitlement Checker  │──→ 403 "Feature not available"            │
│   └──────┬──────────────┘                                             │
│          │ pass                                                       │
│          ▼                                                            │
│   ┌────────────────┐  Check all plan quotas. Return highest-severity │
│   │ Quota Enforcer  │  triggered action. Spending limit check here.  │
│   │                 │──→ 429 "Quota exceeded" / warn headers         │
│   └──────┬─────────┘                                                  │
│          │ allow / warn                                               │
│          ▼                                                            │
│   ┌───────────────────────────────────────────────────┐               │
│   │ V8 ISOLATE EXECUTION                               │              │
│   │  • Per-request CPU watchdog (V8 interrupt API)     │              │
│   │  • Per-request wall watchdog (async timer)         │              │
│   │  • Plugin ops increment per-request counters       │              │
│   │  • On CPU/wall limit → terminate → 503             │              │
│   └──────┬────────────────────────────────────────────┘               │
│          │ result + per-request op counters (accumulated in OpState)    │
│          ▼                                                            │
│   ┌────────────────┐                                                  │
│   │ Meter Recorder  │  Read per-request op counters from OpState     │
│   │                 │  Atomic update to per-app counters (hot)      │
│   │                 │  Event log enqueue (warm/cold, async)          │
│   └──────┬─────────┘                                                  │
│          │                                                            │
│          ▼                                                            │
│   ┌──────────────────────────┐                                        │
│   │ Response Header Injector  │  X-Quota-*, RateLimit-*, X-CPU-*     │
│   └──────┬───────────────────┘                                        │
│          │                                                            │
│          ▼                                                            │
│   ┌──────────────────┐                                                │
│   │ Concurrency Guard │  Decrement gauge                             │
│   └──────┬───────────┘                                                │
│          ▼                                                            │
│   HTTP Response                                                       │
│                                                                       │
├───────────────────────────────────────────────────────────────────────┤
│                       BACKGROUND SERVICES                             │
│                                                                       │
│   Usage Flusher      Every 5s: atomic swap + batch INSERT to SQLite  │
│   Event Logger       Batched fsync writes to append-only log file    │
│   Period Roller      At period boundaries: archive → reset counters  │
│   Storage Sampler    Every 60s: stat() db/kv files → update counters │
│   Spending Monitor   Every 60s: calculate spend, check limits        │
│   Alert Dispatcher   Webhook delivery with exponential backoff       │
│   Dedup Janitor      Every hour: evict expired keys from dedup set   │
│                                                                       │
├───────────────────────────────────────────────────────────────────────┤
│                         STORAGE LAYER                                 │
│                                                                       │
│   Atomic Counters ── In-memory, per-app per-resource                 │
│   Token Buckets ──── In-memory, per-app rate limit state             │
│   Concurrency Gauge ─ AtomicU32 per app (increment/decrement)        │
│   Dedup LRU Set ──── Bounded (1M entries), 24h TTL                   │
│   Usage Store ────── SQLite: period aggregates, history               │
│   Event Log ──────── Append-only file: raw events with hash chain    │
│   Plan Registry ──── Parsed from TOML, immutable per reload          │
│   Webhook Queue ──── In-memory bounded queue + dead letter table     │
│                                                                       │
└───────────────────────────────────────────────────────────────────────┘
```

### 4.2 Metering Pipeline

```
Request completes → Build UsageDelta
┌──────────────────────────────────────────────────────────────┐
│  UsageDelta {                                                │
│    request_id: "req_01JQRA7XYZ",                             │
│    app_id: "my_todo",                                        │
│    timestamp: "2026-03-30T14:22:01.123Z",                    │
│    deltas: [                                                 │
│      (cpu_ms, 4.2),   (wall_ms, 27.0),  (requests, 1),      │
│      (egress_bytes, 1024), (db_reads, 3), (subrequests, 1),  │
│    ]                                                         │
│  }                                                           │
└──────────────┬───────────────────────────────────────────────┘
               │
               ▼
   ┌─── For each (resource, value): ───┐
   │                                    │
   │  1. idempotency_key = "{req_id}_{resource}"
   │  2. if dedup_set.contains(key) → skip
   │  3. dedup_set.insert(key)
   │  4. counter[app][resource].fetch_add(value)  ←── ~10ns, lock-free
   │                                    │
   └────────────────────────────────────┘
               │
               ▼
   Enqueue full UsageDelta to event_log_channel
   (bounded mpsc, capacity 10,000; drop + count on full)
               │
               ▼ (async, background)
   EventLogger: batch write to append-only file
   UsageFlusher: every 5s, swap counters → INSERT to SQLite
```

### 4.3 Period Rollover

```
PeriodRoller (timer-driven)
   │
   ├── 1. Check: current_time > period_end for any app?
   │
   ├── 2. For each affected app (brief per-app lock, <1ms):
   │       a. Snapshot current atomic counters
   │       b. INSERT snapshot into usage_history table
   │       c. Zero monthly/daily counters (atomic store 0)
   │       d. Update period_start/period_end
   │
   ├── 3. Dispatch webhook: billing.period_end
   │
   └── 4. If app was spend-blocked and auto_resume=true: unblock
```

**Race condition:** During the brief per-app lock (step 2), incoming requests for that
app see a `QuotaDecision::Retry` and receive `503 Service Unavailable` with
`Retry-After: 1`. This happens at most once per billing period per app and lasts <1ms.
Other apps are unaffected.

### 4.4 Counter Overflow

All atomic counters are `AtomicU64`. Overflow analysis:
- At 1B requests/second: 584 years to overflow
- At 1 TB/second egress: 213 days to overflow

Defensive handling: counters saturate at `u64::MAX - 1` (using `fetch_update` with
checked addition). A `counter_overflow` alert fires if saturation is reached.

### 4.5 Crash Recovery

On startup after an unclean shutdown:
1. Load last-flushed counters from SQLite (warm tier)
2. Replay event log entries after the last flush timestamp (cold tier)
3. Rebuild atomic counters from the reconciled state
4. Resume normal operation

Data loss window: at most `flush_interval` (5 seconds) of counter updates. The event log
(fsynced more frequently) can recover most of this.

---

## 5. Event Schema

### 5.1 Usage Event

```json
{
  "event_id": "evt_01JQRA7XYZABC123",
  "idempotency_key": "req_01JQRA7XYZ_cpu_ms",
  "app_id": "my_todo",
  "resource": "cpu_ms",
  "value": 4,
  "timestamp": "2026-03-30T14:22:01.123Z",
  "period": "2026-03",
  "plan": "pro",
  "plan_version": 1,
  "metadata": {
    "request_id": "req_01JQRA7XYZ",
    "method": "todos.list"
  }
}
```

| Field | Type | Required | Description |
|---|---|---|---|
| `event_id` | ULID | Yes | Globally unique, sortable identifier |
| `idempotency_key` | String | Yes | Deduplication key (unique per resource per request) |
| `app_id` | String | Yes | Tenant identifier |
| `resource` | String | Yes | Resource name (must match defined resource) |
| `value` | u64 | Yes | Consumption amount (>= 0, integer — fractional ms stored as microseconds) |
| `timestamp` | ISO 8601 | Yes | Event time, always UTC, millisecond precision |
| `period` | String | Yes | Billing period (YYYY-MM) |
| `plan` | String | Yes | Plan name at time of event |
| `plan_version` | u32 | Yes | Plan version at time of event |
| `metadata` | Object | No | Arbitrary context for debugging |

### 5.2 Event Log Integrity

Each event log entry includes:
- The event payload (JSON)
- A SHA-256 hash: `H(n) = SHA256(H(n-1) || event_payload_bytes)`
- Entry sequence number

This hash chain makes tampering with historical events detectable. The chain is seeded
with `H(0) = SHA256("appbase-event-log-v2")`. Verification via
`GET /_admin/event_log/verify` scans the chain and reports any breaks (returns the
first broken entry and total entries verified).

### 5.3 Late-Arriving Events

Events with timestamps in a closed billing period:
1. Accepted into the event log (never rejected)
2. Flagged with `"late_arrival": true`
3. Do NOT update the archived period's counters automatically
4. Visible via `GET /_admin/reconciliation`
5. Can be applied via `POST /_admin/reconciliation/apply` (admin action, audit-logged)

### 5.4 Event Corrections

To correct a previously recorded event:

```json
{
  "event_id": "evt_01JQRA8CORRECTED",
  "correction_for": "evt_01JQRA7XYZABC123",
  "app_id": "my_todo",
  "resource": "cpu_ms",
  "value": 3.8,
  "reason": "Measurement included system overhead"
}
```

Corrections create a new event that supersedes the original. The net effect is
`new_value - original_value` applied to counters. Requires admin privilege.

---

## 6. Spending Limits & Budget Alerts

### 6.1 Motivation

Vercel's lack of spending limits was a well-documented problem, leading to unexpected
bills of thousands of dollars. Their eventual spend management system (2024) still
requires manual per-project unpausing after the limit is hit. Appbase addresses cost
predictability from day one.

### 6.2 How Spend Is Calculated

```
For each app, every check_interval_secs:

  spend_estimate_cents = 0
  for each resource with overage pricing:
    included = plan.quotas[resource].max  (or 0 if no quota)
    usage = current_counters[resource]
    overage = max(0, usage - included)
    spend_estimate_cents += overage * price_per_unit_cents

  spend_estimate_cents is stored and compared against limit
```

**Precision:** All monetary values are stored in integer cents to avoid floating-point
rounding errors. Displayed as dollars with 2 decimal places.

### 6.3 Configuration

```toml
# Platform-level defaults
[spending]
enabled = true
check_interval_secs = 60
default_alert_thresholds = [50, 75, 90, 100]

# Per-app override
[apps.my_blog.spending]
limit_usd = 50.00                # Maximum spend per billing period
action = "block"                  # What happens at 100%
auto_resume = true                # Resume at next period?
alert_thresholds = [25, 50, 75, 100]  # Override default thresholds
webhook_url = "https://..."       # Per-app webhook for spend alerts
```

### 6.4 Actions

| Action | At 100% Limit | Behavior |
|---|---|---|
| `block` | Block all requests | 429 with spending-limit error |
| `degrade` | Downgrade to free-tier limits | Requests allowed but throttled |
| `warn` | Add header, allow requests | `X-Spending-Warning` header |
| `webhook` | POST to URL, allow requests | External system decides |

### 6.5 Alert Flow

```
SpendingMonitor tick (every 60s)
   │
   ├── For each app with spending limit:
   │     calculate spend_estimate
   │     for each threshold [50, 75, 90, 100]:
   │       if spend >= threshold% of limit:
   │         if not already_alerted[app][threshold]:
   │           dispatch: spending.threshold webhook
   │           already_alerted[app][threshold] = true
   │
   └── At 100%:
         execute configured action (block/degrade/warn/webhook)
         dispatch: spending.blocked webhook
```

### 6.6 Resume

| Config | At Period Boundary |
|---|---|
| `auto_resume = true` | Automatically unblocked, counters reset |
| `auto_resume = false` | Stays blocked until `POST /_admin/apps/{id}/spending/resume` |

---

## 7. Credits & Prepaid Wallets

### 7.1 Concepts

Inspired by Lago's wallet system:

- **Wallet** — A container holding a credit balance in cents
- **Credit** — A monetary unit applied against usage charges before invoicing
- **Top-up** — Adding funds to a wallet (manual or automatic)
- **Expiry** — Optional: credits expire after a configurable date

### 7.2 Wallet Structure

```json
{
  "app_id": "my_todo",
  "balance_cents": 10000,
  "currency": "usd",
  "auto_topup": {
    "enabled": true,
    "threshold_cents": 1000,
    "amount_cents": 10000
  },
  "expires_at": null,
  "created_at": "2026-01-15T00:00:00Z",
  "transactions": [
    { "type": "topup", "amount_cents": 10000, "timestamp": "...", "note": "Initial deposit" },
    { "type": "deduction", "amount_cents": -2500, "timestamp": "...", "period": "2026-02", "note": "February usage" }
  ]
}
```

### 7.3 Credit Application Order

When calculating an invoice:
1. Calculate total usage charges (overage beyond included amounts)
2. Subtract credits from wallet (oldest transactions first)
3. Remaining amount = billable to payment method

Expired credits are skipped during application and logged as `credit.expired`.

### 7.4 Admin API

```
GET    /_admin/apps/{id}/wallet                — Balance and config
POST   /_admin/apps/{id}/wallet/topup          — Add credits { amount_cents, note }
POST   /_admin/apps/{id}/wallet/deduct         — Deduct credits { amount_cents, note }
GET    /_admin/apps/{id}/wallet/transactions   — Transaction history (paginated)
```

---

## 8. Response Headers & Error Responses

### 8.1 Standard Response Headers

Present on every successful response:

```http
HTTP/1.1 200 OK

# Request identity
X-Request-Id: req_01JQRA7XYZ

# Timing
X-CPU-Time-Ms: 0.67
X-Wall-Time-Ms: 27.01

# Plan
X-Plan: pro
X-Plan-Version: 1

# Quota (most constrained resource — lowest remaining percentage)
X-Quota-Resource: requests
X-Quota-Used: 42
X-Quota-Limit: 10000000
X-Quota-Remaining: 9999958
X-Quota-Reset: 2026-04-01T00:00:00Z

# Rate Limit (IETF draft-ietf-httpapi-ratelimit-headers)
RateLimit-Limit: 1000
RateLimit-Remaining: 997
RateLimit-Reset: 3

# Enforcement mode (only present if not "enforce")
X-Enforcement-Mode: dry_run
```

**Most constrained resource:** The `X-Quota-*` headers show whichever quota has the
lowest `remaining / limit` ratio. This gives developers a single signal to watch.

### 8.2 Warning Headers

When usage passes a warning threshold:

```http
X-Quota-Warning: requests at 82% (82000/100000); resets 2026-04-01
X-Quota-Warning: egress_bytes at 91% (910MB/1000MB); resets 2026-04-01
```

Multiple warnings can be present simultaneously (one per triggered resource).

### 8.3 Error Responses

All error responses follow JSON-RPC 2.0 format with structured `data`:

**Quota exceeded (429):**
```json
{
  "jsonrpc": "2.0",
  "error": {
    "code": -32029,
    "message": "Monthly request quota exceeded",
    "data": {
      "type": "quota_exceeded",
      "resource": "requests",
      "used": 100000,
      "limit": 100000,
      "reset": "2026-04-01T00:00:00Z",
      "plan": "free",
      "upgrade_url": "https://appbase.dev/pricing"
    }
  },
  "id": 1
}
```
HTTP headers: `Retry-After: 86400` (seconds until period reset)

**Rate limited (429):**
```json
{
  "jsonrpc": "2.0",
  "error": {
    "code": -32029,
    "message": "Rate limit exceeded",
    "data": {
      "type": "rate_limited",
      "limit_per_second": 10,
      "burst": 50,
      "retry_after_ms": 100
    }
  },
  "id": 1
}
```
HTTP headers: `Retry-After: 1`

**Spending limit (429):**
```json
{
  "jsonrpc": "2.0",
  "error": {
    "code": -32029,
    "message": "Spending limit reached",
    "data": {
      "type": "spending_limit",
      "current_spend_usd": "50.00",
      "limit_usd": "50.00",
      "auto_resume": true,
      "period_end": "2026-04-01T00:00:00Z"
    }
  },
  "id": 1
}
```

**Entitlement denied (403):**
```json
{
  "jsonrpc": "2.0",
  "error": {
    "code": -32030,
    "message": "Feature not available on your plan",
    "data": {
      "type": "entitlement_denied",
      "feature": "cron_jobs",
      "plan": "free",
      "required_plans": ["pro", "enterprise"],
      "upgrade_url": "https://appbase.dev/pricing"
    }
  },
  "id": 1
}
```

**Per-request limit (503):**
```json
{
  "jsonrpc": "2.0",
  "error": {
    "code": -32031,
    "message": "CPU time limit exceeded (10ms)",
    "data": {
      "type": "execution_limit",
      "resource": "cpu_ms",
      "used_ms": 10.2,
      "limit_ms": 10
    }
  },
  "id": 1
}
```

### 8.4 Error Code Catalog

All error codes used by the quota/metering system:

| Code | HTTP | Type | Description |
|---|---|---|---|
| `-32029` | 429 | `quota_exceeded` | Monthly/daily quota reached |
| `-32029` | 429 | `rate_limited` | Token bucket exhausted |
| `-32029` | 429 | `spending_limit` | Spending limit reached |
| `-32029` | 429 | `concurrency_limit` | Max concurrent requests |
| `-32030` | 403 | `entitlement_denied` | Feature not available on plan |
| `-32031` | 503 | `execution_limit` | Per-request CPU/wall limit (kill) |
| `-32031` | 503 | `period_rollover` | Brief unavailability during counter reset |

All codes are within the JSON-RPC 2.0 implementation-defined server error range
(-32000 to -32099). The `data.type` field disambiguates errors sharing the same code.
Clients should use `data.type` for programmatic handling, not the numeric code alone.

**Multiple simultaneous violations:** When multiple quotas are violated at the same time,
the enforcer returns the error for the most critical violation (highest-severity policy
action). If severity is equal, the resource with the lowest remaining percentage is
reported. All violated resources are listed in `data.all_violations`:

```json
{
  "data": {
    "type": "quota_exceeded",
    "resource": "requests",
    "all_violations": [
      { "resource": "requests", "used": 100000, "limit": 100000 },
      { "resource": "cpu_ms", "used": 10500, "limit": 10000 }
    ]
  }
}
```

The `data.type` field in the error response disambiguates errors sharing the same code.
Clients should use `data.type` (not just the numeric code) for programmatic error handling.

---

## 9. Admin API

All admin endpoints require authentication (`Authorization: Bearer <api_key>` or mTLS).
Destructive operations require `X-Confirm: true` header. Responses are JSON with a
standard envelope:

```json
{
  "data": { ... },           // Response payload (object or array)
  "meta": {                  // Present on list endpoints
    "total": 42,
    "limit": 20,
    "offset": 0,
    "next_cursor": "...",    // For cursor-based pagination
    "has_more": true
  },
  "warnings": [              // Present when action has side effects
    "App my_blog is now over quota for requests (200000/100000)"
  ]
}
```

**Concurrency control:** Mutating endpoints use optimistic concurrency via `ETag`/
`If-Match` headers. Example:
```
GET /_admin/apps/my_blog → ETag: "v3"
PUT /_admin/apps/my_blog/plan  If-Match: "v3"  → 200 OK (or 409 Conflict if changed)
```

### 9.1 Apps

| Method | Path | Description |
|---|---|---|
| GET | `/_admin/apps` | List apps with plan and usage summary |
| GET | `/_admin/apps/{id}` | App details (plan, entitlements, quotas, usage) |
| PUT | `/_admin/apps/{id}/plan` | Change plan `{ "plan": "pro", "version": 1 }` |
| PATCH | `/_admin/apps/{id}/overrides` | Set per-app quota overrides |
| DELETE | `/_admin/apps/{id}/overrides` | Remove all overrides |
| POST | `/_admin/apps/{id}/reset` | Reset current period counters (X-Confirm required) |
| DELETE | `/_admin/apps/{id}` | Evict isolate + clear all state (X-Confirm required) |
| PUT | `/_admin/apps/{id}/enforcement_mode` | Set mode `{ "mode": "dry_run" }` |

### 9.2 Usage & Metering

| Method | Path | Description |
|---|---|---|
| GET | `/_admin/apps/{id}/usage` | Current period usage (all resources) |
| GET | `/_admin/apps/{id}/usage?resource=cpu_ms` | Single resource breakdown |
| GET | `/_admin/apps/{id}/usage/history` | Historical periods (paginated) |
| GET | `/_admin/apps/{id}/usage/history/{period}` | Specific period detail |
| GET | `/_admin/apps/{id}/events` | Raw events (paginated, filterable) |
| GET | `/_admin/usage/summary` | Platform-wide current period summary |
| GET | `/_admin/usage/top?resource=cpu_ms&limit=10` | Top N apps by resource |

Query parameters for list endpoints: `?limit=N&offset=M&sort=field&order=asc|desc`
Event filtering: `?resource=X&from=ISO8601&to=ISO8601&method=X`

### 9.3 Plans

| Method | Path | Description |
|---|---|---|
| GET | `/_admin/plans` | List all plans |
| GET | `/_admin/plans/{name}` | Plan details (current version) |
| GET | `/_admin/plans/{name}/apps` | Apps on this plan |
| POST | `/_admin/plans/{name}/migrate` | Bulk migrate apps to latest version |
| POST | `/_admin/plans/{name}/simulate` | Simulate effect of plan on an app's current usage |

### 9.4 Spending & Billing

| Method | Path | Description |
|---|---|---|
| GET | `/_admin/apps/{id}/spending` | Current spend estimate |
| PUT | `/_admin/apps/{id}/spending` | Set spending config |
| POST | `/_admin/apps/{id}/spending/resume` | Resume spend-paused app |
| GET | `/_admin/apps/{id}/invoice/preview` | Preview next invoice |
| GET | `/_admin/apps/{id}/invoice/history` | Historical invoice snapshots (generated at period end) |

### 9.5 Credits

| Method | Path | Description |
|---|---|---|
| GET | `/_admin/apps/{id}/wallet` | Wallet balance and config |
| POST | `/_admin/apps/{id}/wallet/topup` | Add credits `{ amount_cents, note }` |
| POST | `/_admin/apps/{id}/wallet/deduct` | Deduct credits `{ amount_cents, note }` |
| GET | `/_admin/apps/{id}/wallet/transactions` | Transaction history |

### 9.6 Operations

| Method | Path | Description |
|---|---|---|
| GET | `/_admin/health` | System health (components: rate_limiter, flusher, event_log, ...) |
| GET | `/_admin/metrics` | Prometheus-format metrics |
| POST | `/_admin/flush` | Force flush counters to SQLite |
| POST | `/_admin/config/validate` | Validate TOML without applying |
| POST | `/_admin/config/reload` | Hot reload configuration |
| GET | `/_admin/event_log/verify` | Verify event log hash chain integrity |
| GET | `/_admin/reconciliation` | Late-arriving events report |
| POST | `/_admin/reconciliation/apply` | Apply late events to closed period |
| GET | `/_admin/webhooks/failed` | Failed webhook deliveries |
| POST | `/_admin/webhooks/{id}/replay` | Re-send a failed webhook |
| POST | `/_admin/webhooks/test` | Send a test event to verify connectivity |

### 9.7 Data Export

| Method | Path | Description |
|---|---|---|
| GET | `/_admin/export/usage?from=...&to=...&format=csv` | Export usage data |
| GET | `/_admin/export/events?from=...&to=...&format=jsonl` | Export raw events |

Supported formats: `csv`, `json`, `jsonl` (newline-delimited JSON).

### 9.8 Pagination

List endpoints support two pagination modes:

**Offset-based** (simple, for small datasets):
```
GET /_admin/apps?limit=20&offset=40&sort=app_id&order=asc
```

**Cursor-based** (stable, for large/streaming datasets):
```
GET /_admin/apps/{id}/events?limit=100&cursor=evt_01JQRA7XYZ
→ Response includes: { "data": [...], "next_cursor": "evt_01JQRB8ABC", "has_more": true }
```

Event log queries always use cursor-based pagination (offset is unreliable on append-only
data). The cursor is an opaque string (typically the last event_id).

### 9.9 Health Check Response

```json
{
  "status": "healthy",
  "version": "0.2.0",
  "uptime_seconds": 86423,
  "components": {
    "rate_limiter": { "status": "healthy", "apps_tracked": 42 },
    "usage_flusher": { "status": "healthy", "last_flush": "2026-03-30T14:21:56Z", "flush_lag_ms": 12 },
    "event_logger": { "status": "healthy", "queue_depth": 234, "queue_capacity": 10000 },
    "period_roller": { "status": "healthy", "next_rollover": "2026-04-01T00:00:00Z" },
    "spending_monitor": { "status": "healthy", "last_check": "2026-03-30T14:21:55Z" },
    "storage_sampler": { "status": "healthy", "last_sample": "2026-03-30T14:21:30Z" },
    "webhook_dispatcher": { "status": "healthy", "queue_depth": 0, "failed_count": 1 }
  }
}
```

---

## 10. Monitoring & Observability

### 10.1 Prometheus Metrics

Exported at `GET /_admin/metrics`:

```prometheus
# ── Per-app usage (labels: app, plan, resource) ──
appbase_resource_usage_total{app="my_todo",plan="pro",resource="requests"} 4231
appbase_resource_usage_total{app="my_todo",plan="pro",resource="cpu_ms"} 12400

# ── Enforcement decisions (labels: app, resource, decision) ──
appbase_enforcement_total{app="my_todo",resource="requests",decision="allow"} 4200
appbase_enforcement_total{app="my_todo",resource="requests",decision="warn"} 31
appbase_enforcement_total{app="my_todo",resource="requests",decision="block"} 0

# ── Rate limiting ──
appbase_rate_limited_total{app="my_todo"} 12
appbase_rate_limit_tokens{app="my_todo"} 47

# ── Spending ──
appbase_spend_estimate_cents{app="my_todo"} 2000
appbase_spend_limit_cents{app="my_todo"} 5000
appbase_spend_blocked{app="my_todo"} 0

# ── Metering pipeline health ──
appbase_meter_flush_duration_seconds{quantile="0.50"} 0.001
appbase_meter_flush_duration_seconds{quantile="0.99"} 0.008
appbase_meter_flush_errors_total 0
appbase_event_log_writes_total 98234
appbase_event_log_write_errors_total 0
appbase_event_log_size_bytes 1234567
appbase_event_log_drops_total 0
appbase_dedup_set_size 45000
appbase_dedup_set_evictions_total 1200

# ── Quota enforcement latency ──
appbase_quota_check_duration_seconds{quantile="0.50"} 0.000003
appbase_quota_check_duration_seconds{quantile="0.99"} 0.000012

# ── System ──
appbase_active_isolates 42
appbase_active_apps 15
appbase_counter_overflow_total 0
appbase_period_rollover_duration_seconds{quantile="0.99"} 0.0004

# ── Webhooks ──
appbase_webhook_sent_total{status="success"} 42
appbase_webhook_sent_total{status="failed"} 1
appbase_webhook_retry_total 3
appbase_webhook_dead_letter_total 0
```

### 10.2 Structured Logging

All enforcement decisions produce structured log entries:

```json
{
  "ts": "2026-03-30T14:22:01.123Z",
  "level": "warn",
  "target": "appbase::quota",
  "msg": "quota_threshold_reached",
  "fields": {
    "app_id": "my_blog",
    "resource": "requests",
    "current": 80000,
    "max": 100000,
    "usage_pct": 80,
    "threshold": 80,
    "action": "warn",
    "plan": "free",
    "plan_version": 1,
    "request_id": "req_01JQRA7XYZ",
    "enforcement_mode": "enforce"
  }
}
```

**Log levels:**

| Level | Events |
|---|---|
| `trace` | Every enforcement check result (allow) — very verbose |
| `debug` | Counter updates, dedup hits, token bucket state |
| `info` | Period rollover, plan changes, config reload, app created/deleted |
| `warn` | Warning thresholds reached, throttling applied, event log drops |
| `error` | Quota exceeded (block), rate limited, spend blocked, system errors |

### 10.3 Alert Rules

| Alert | Condition | Severity | Action |
|---|---|---|---|
| `quota_warning` | App at >= 80% of any quota | Warning | Webhook |
| `quota_exceeded` | App at 100% of any quota | Error | Webhook |
| `rate_limit_spike` | Rate limiting > 100/5min for an app | Warning | Webhook + log |
| `spend_threshold` | Spend reaches configured threshold | Warning | Webhook |
| `spend_blocked` | App blocked due to spending limit | Error | Webhook |
| `flush_failure` | Counter flush to SQLite failed | Critical | Log + webhook |
| `event_log_error` | Event log write failed | Critical | Log + webhook |
| `event_log_drops` | Event log channel full, events dropped | Warning | Log + webhook |
| `usage_spike` | Any resource > 3x 7-day rolling average | Warning | Webhook |
| `storage_high` | Storage > 90% of quota | Warning | Webhook |
| `counter_overflow` | Counter saturated at u64::MAX | Critical | Log + webhook |
| `webhook_failure` | Webhook delivery failed after all retries | Warning | Log |

---

## 11. Billing Integration

### 11.1 Supported Billing Models

| Model | Base Fee | Usage Charges | Hard Caps | Example |
|---|---|---|---|---|
| Flat subscription | Yes | No | Yes | Free tier ($0), basic ($10/mo) |
| Subscription + overage | Yes | Beyond included | No | Pro tier ($20/mo + usage) |
| Pure usage-based | No | All usage | Optional | API-only tier |
| Tiered (graduated) | Yes/No | Per-tier rates | Optional | Volume discount |
| Committed use (credits) | Prepaid | Deducted from wallet | Optional | Enterprise |

### 11.2 Invoice Preview

```json
{
  "app_id": "my_todo",
  "period": {
    "start": "2026-03-01T00:00:00Z",
    "end": "2026-03-31T23:59:59Z"
  },
  "plan": { "name": "pro", "version": 1 },
  "line_items": [
    {
      "type": "subscription",
      "description": "Pro plan — March 2026",
      "amount_cents": 2000
    },
    {
      "type": "overage",
      "resource": "requests",
      "quantity": 12_500_000,
      "included": 10_000_000,
      "overage_quantity": 2_500_000,
      "unit_price": "$0.30/million",
      "amount_cents": 75
    },
    {
      "type": "overage",
      "resource": "egress_bytes",
      "quantity_bytes": 150_000_000_000,
      "included_bytes": 100_000_000_000,
      "overage_bytes": 50_000_000_000,
      "unit_price": "$0.09/GB",
      "amount_cents": 450
    },
    {
      "type": "credit",
      "description": "Wallet credit applied",
      "amount_cents": -525
    }
  ],
  "subtotal_cents": 2525,
  "credits_applied_cents": 525,
  "tax_cents": 0,
  "tax_note": "Tax calculation delegated to payment processor",
  "total_cents": 2000,
  "generated_at": "2026-03-30T14:30:00Z",
  "status": "preview"
}
```

### 11.3 Billing Period Configuration

| Type | When Counters Reset | Best For |
|---|---|---|
| `calendar_month` | 1st at 00:00 UTC | Simplicity, standard SaaS |
| `anniversary` | Signup day each month | Per-customer billing dates |

### 11.4 Plan Changes (Proration)

| Scenario | Effective Limits | Billing |
|---|---|---|
| Upgrade mid-period | New limits immediately | Prorated charge: `(remaining_days / total_days) * price_diff` |
| Downgrade mid-period | Old limits until period end | New price starts next period, no refund |
| Cancel | Drops to free-tier limits | No refund for current period |

### 11.5 Overage

When `overage.enabled = true` on a plan:
- Monthly quotas become **soft limits** (usage beyond max is allowed)
- The enforcement policy is overridden to `allow` + `notify` beyond 100%
- Overage is billed at the configured per-unit rate
- Spending limits still apply as a hard ceiling on total cost

### 11.6 External Billing System Integration

Appbase generates usage data; external systems handle payment:

```
                    Appbase                      External
                  ┌──────────┐               ┌─────────────┐
  billing.period  │          │  GET /invoice  │             │
  _end webhook ──→│ Admin API│──→ preview ──→│ Stripe/Lago │
                  │          │               │             │
                  │ GET      │  usage data   │ Create      │
                  │ /export  │──→──→──→──→──→│ invoice     │
                  │          │               │             │
                  │ POST     │  on payment   │ Process     │
                  │ /wallet  │←──←──←──←──←──│ payment     │
                  │ /topup   │               │             │
                  └──────────┘               └─────────────┘
```

**Stripe integration pattern:**
1. `billing.period_end` webhook fires
2. Your service calls `GET /_admin/apps/{id}/invoice/preview`
3. Creates Stripe invoice with line items from preview
4. Stripe processes payment
5. Optionally: `POST /_admin/apps/{id}/wallet/topup` for credit-based model

**Lago integration pattern:**
1. During the period: forward usage events to Lago's event API
2. Lago handles aggregation, invoice generation, and payment
3. Appbase handles enforcement; Lago handles billing

---

## 12. Webhook Notifications

### 12.1 Event Catalog

| Event | Trigger | Payload Data |
|---|---|---|
| `quota.warning` | Usage at warning threshold | resource, used, limit, pct |
| `quota.exceeded` | Usage at 100% | resource, used, limit |
| `quota.recovered` | Usage drops below limit | resource, used, limit |
| `rate_limit.triggered` | Rate limiting activated | count, window_seconds |
| `spending.threshold` | Spend at alert threshold | current_cents, limit_cents, pct |
| `spending.blocked` | Spending limit reached | current_cents, limit_cents, action |
| `spending.resumed` | Spend-blocked app resumed | method (auto/manual) |
| `billing.period_end` | Billing period ended | period, usage_summary |
| `billing.invoice_ready` | Invoice preview available | period, total_cents |
| `plan.changed` | Plan changed | old_plan, new_plan, old_version, new_version |
| `app.created` | New app registered | app_id, plan |
| `app.deleted` | App removed | app_id |
| `credit.applied` | Credits deducted for usage | amount_cents, remaining_cents |
| `credit.expired` | Credits expired unused | amount_cents |
| `system.flush_error` | Counter flush failed | error_message |

### 12.2 Payload Format

```json
{
  "id": "whk_01JQRA7XYZABC123",
  "event": "quota.warning",
  "timestamp": "2026-03-30T14:22:01Z",
  "app_id": "my_blog",
  "data": {
    "resource": "requests",
    "used": 80000,
    "limit": 100000,
    "usage_pct": 80,
    "plan": "free",
    "period": "2026-03"
  }
}
```

### 12.3 Delivery & Security

- **Transport:** HTTPS POST with `Content-Type: application/json`
- **Signature:** `X-Appbase-Signature: sha256=<hex>` computed as `HMAC-SHA256(secret, body)`
- **Verification:** Receiver computes HMAC and compares (constant-time) to header
- **Secret rotation:** `POST /_admin/webhooks/rotate_secret` generates a new secret. Both old and new secrets are valid for a configurable grace period (default: 24 hours).
- **Timeout:** 10 seconds per attempt
- **Retries:** 5 attempts with exponential backoff (1s, 5s, 30s, 5m, 30m)
- **Dead letter:** After 5 failures, event stored in dead letter queue
- **Replay:** `POST /_admin/webhooks/{id}/replay`
- **Dedup:** Same event type + app + threshold fires at most once per billing period
- **Event filtering:** The `events` config uses prefix matching with `*` wildcard.
  `"quota.*"` matches `quota.warning`, `quota.exceeded`, `quota.recovered`.
  `"*"` matches all events. Exact names (e.g., `"quota.warning"`) match only that event.

---

## 13. Security & Abuse Prevention

### 13.1 Tenant Isolation

| Vector | Mitigation |
|---|---|
| One app consuming all CPU | Per-request CPU time limit (V8 interrupt) |
| One app consuming all memory | V8 isolate heap limit; OOM kills only that isolate |
| One app sending burst traffic | Per-app rate limit (token bucket) |
| One app opening many connections | Per-app concurrency limit |
| One app filling disk | Per-app storage quota (block_writes policy) |
| Cross-app counter contamination | Independent counter maps per app_id |

### 13.2 Admin API Security

- **Authentication:** API key (`Authorization: Bearer <key>`) or mTLS
- **Rate limiting:** Admin API has its own rate limit (default: 100 req/s)
- **Audit log:** Every admin action is recorded with timestamp, API key identity, action, parameters, and result
- **Destructive safeguards:** DELETE and reset endpoints require `X-Confirm: true`
- **Read-only mode:** Can run admin API in read-only mode for monitoring without risk

### 13.3 Anti-Gaming

| Attack | Prevention |
|---|---|
| Bypass metering from user code | Metering in Rust runtime, outside V8 sandbox |
| Manipulate CPU time measurement | `CLOCK_THREAD_CPUTIME_ID` owned by platform |
| Reset own counters | No user-facing counter reset API |
| Replay events to deflate usage | Idempotency keys prevent duplicate processing |
| Forge event timestamps | Timestamps set by platform, not user |

### 13.4 Denial-of-Wallet Prevention

An attacker targeting another user's app could exhaust their quota or spending limit:

| Layer | Protection |
|---|---|
| 1. Rate limit | Caps request throughput regardless of origin |
| 2. Per-IP rate limit (opt-in) | `[apps.X.ip_rate_limit] max_per_second = 5` |
| 3. Concurrency limit | Prevents slow-request resource exhaustion |
| 4. Spending limit | Caps total financial exposure per period |
| 5. Anomaly alerts | `usage_spike` alert on >3x baseline |

### 13.5 Data Integrity

| Mechanism | Purpose |
|---|---|
| `AtomicU64::fetch_add(Relaxed)` | Lock-free concurrent counter updates |
| SQLite WAL + `PRAGMA synchronous=NORMAL` | Durable flush with good performance |
| Event log hash chain | Tamper detection on historical records |
| Idempotency dedup set | Prevent double-counting |
| Checksums on exported data | Verify export integrity |

---

## 14. Developer Experience

### 14.1 For Platform Owners

| Capability | How |
|---|---|
| Define plans | TOML config: `[plans.X]` sections |
| Per-app customization | `[apps.X.overrides.quotas]` without new plan |
| Validate changes before deploy | `POST /_admin/config/validate` with TOML body |
| Test new limits safely | `enforcement_mode = "dry_run"` per app |
| Simulate plan effects | `POST /_admin/plans/{name}/simulate` |
| Hot reload | `POST /_admin/config/reload` or `kill -HUP <pid>` |
| Monitor everything | Prometheus metrics + structured logs |
| Export for analytics | CSV/JSONL export endpoints |
| Integrate billing | Invoice preview API + webhook events |

### 14.2 For App Developers

| Need | Solution |
|---|---|
| Know my current usage | `X-Quota-*` headers on every response |
| Know when I am close to limits | `X-Quota-Warning` headers at 80% |
| Know why my request failed | Structured error with resource, used, limit, reset, upgrade URL |
| Know when to retry | `Retry-After` header on 429 responses |
| Avoid surprise bills | Spending limits, budget alerts, hard caps on free tier |
| Access gated features | Clear entitlement error with required plan name |

### 14.3 Plugin Metering SDK

```rust
// Report usage for a custom resource
state.meter("email_sends", 1);

// Report multiple resources
state.meter("ai_tokens", 150);
state.meter("egress_bytes", result.size_bytes as u64);

// Pre-check quota before expensive work
match state.quota_available("ai_tokens") {
    QuotaAvailable::Yes(remaining) => { /* proceed */ },
    QuotaAvailable::No { used, limit } => {
        return Err(quota_exceeded_error("ai_tokens", used, limit));
    },
    QuotaAvailable::Unlimited => { /* no quota defined, proceed */ },
}
```

**SDK guarantees:**
- `meter()` never fails, never panics, never blocks
- Unknown resource names are silently ignored (with debug log)
- Thread-safe: safe to call from any async context
- `quota_available()` reads atomic counter — no I/O, no lock

---

## 15. Testing Strategy

### 15.1 Unit Tests

- Policy evaluation: verify correct action at each threshold
- Token bucket: verify rate/burst behavior, refill timing
- Quota enforcer: verify allow/warn/block decisions against counter values
- Config parser: verify validation rules (V1-V14) with valid and invalid inputs
- Invoice calculation: verify line items, overage, credit application, proration
- Idempotency: verify dedup set insert/lookup/eviction behavior

### 15.2 Integration Tests

- Full request flow: send requests through middleware stack, verify headers and enforcement
- Period rollover: advance clock, verify counter reset and archive
- Spending monitor: simulate usage growth, verify alert dispatch and blocking
- Crash recovery: kill process, restart, verify counters restored from SQLite + event log
- Hot reload: change config, reload, verify new limits applied (old apps unchanged)

### 15.3 Load Tests

- Throughput: verify quota enforcement adds < 1 microsecond to request latency at p99
- Counter contention: 100 concurrent requests to same app, verify atomic correctness
- Event log throughput: verify no drops under sustained 10K events/second
- Memory: verify dedup set stays within bounds under sustained traffic

### 15.4 Testing Utilities

- **Clock override:** Inject a mock clock for deterministic period rollover testing
- **Dry-run mode:** Test new quota configurations without affecting production traffic
- **Shadow mode:** Run two quota configs simultaneously, compare decisions
- **Test app:** A special `_test` app with short periods (1-minute) for rapid iteration

---

## 16. Implementation Plan

| Phase | Deliverable | Dependencies | Effort |
|---|---|---|---|
| **1** | Core types + config parser | — | S |
| **2** | Atomic counters (`MeterRegistry`) | Phase 1 | S |
| **3** | Token bucket rate limiter | Phase 1 | S |
| **4** | Quota enforcer | Phase 1, 2 | M |
| **5** | Tower middleware (full request path) | Phase 2, 3, 4 | M |
| **6** | Response headers | Phase 5 | S |
| **7** | Entitlement checker | Phase 1, 5 | S |
| **8** | SQLite persistent store + flush | Phase 2 | M |
| **9** | Event log (append-only + hash chain) | Phase 2 | M |
| **10** | Period rollover service | Phase 8 | S |
| **11** | Admin API (core endpoints) | Phase 2, 4, 8 | L |
| **12** | Spending monitor + alerts | Phase 8, 11 | M |
| **13** | Webhook dispatcher + delivery | Phase 12 | M |
| **14** | Prometheus metrics | Phase 2-12 | M |
| **15** | Invoice preview + billing | Phase 8, 12 | M |
| **16** | Credits/wallets | Phase 15 | M |
| **17** | Plugin metering SDK | Phase 2, 4 | S |
| **18** | Data export (CSV/JSONL) | Phase 8, 9 | S |
| **19** | Config hot reload | Phase 1 | S |
| **20** | Dry-run/shadow enforcement modes | Phase 4, 5 | S |
| **21** | Crash recovery | Phase 8, 9 | M |

Effort: S = 1-2 days, M = 3-5 days, L = 1-2 weeks

**Critical path:** Phases 1 → 2 → 4 → 5 → 8 → 11 (core enforcement + persistence + admin)

---

## 17. Operational Guidance

### 17.1 Capacity Planning

| Component | Memory Formula | Disk Formula |
|---|---|---|
| Atomic counters | `num_apps * num_resources * 8 bytes` (e.g., 1000 apps * 15 resources = 120 KB) | N/A |
| Token buckets | `num_apps * 32 bytes` (e.g., 1000 apps = 32 KB) | N/A |
| Dedup LRU set | `capacity * ~80 bytes` (1M entries = ~80 MB) | N/A |
| Usage store (SQLite) | Minimal (read cache) | `num_apps * num_resources * num_periods * ~100 bytes` |
| Event log | Bounded channel: `capacity * ~512 bytes` (10K = ~5 MB) | `events_per_day * ~200 bytes * retention_days` |
| Webhook queue | `capacity * ~1 KB` | Dead letter: `failed_count * ~1 KB` |

**Example:** 1000 apps, 15 resources, 10K req/s aggregate, 90-day retention:
- Memory: ~100 MB (dominated by dedup set)
- Disk: ~50 GB/year event log (with compression: ~10 GB/year)
- SQLite: ~500 MB for usage history

### 17.2 Data Retention

| Data | Default Retention | Configurable | Cleanup |
|---|---|---|---|
| Atomic counters | Current period only | No | Reset at period rollover |
| Usage history (SQLite) | 24 months | `[storage] history_retention_months` | Background job, monthly |
| Event log | 90 days | `[event_log] retention_days` | Background job, daily |
| Webhook dead letters | 30 days | `[webhooks] dead_letter_retention_days` | Background job, daily |
| Audit log (admin actions) | Permanent | No | Manual export + purge |
| Dedup set | 24 hours (TTL) | `[metering] dedup_ttl_hours` | Janitor, hourly |

### 17.3 Deleted App Behavior

When an app is deleted via `DELETE /_admin/apps/{id}`:
1. V8 isolate is evicted immediately
2. Atomic counters are dropped
3. Usage history is **retained** for billing purposes (marked `deleted = true`)
4. Event log entries are **retained** (immutable by design)
5. Webhooks for the deleted app are cancelled
6. The app_id cannot be reused for 30 days (prevents confusion in billing data)

### 17.4 Clock and Time Handling

- All internal timestamps use `Instant::now()` (monotonic) for duration measurement
- All external timestamps use `SystemTime` in UTC with millisecond precision
- Daily resets occur at 00:00 UTC regardless of server timezone
- NTP jumps: `Instant` is immune to clock adjustments. For `SystemTime`-based events,
  the system detects backward jumps > 5 seconds and logs a warning. Events with
  timestamps in the future (> 60 seconds ahead) are rejected.
- Monthly period boundaries are calculated using calendar arithmetic (e.g., March billing
  period = March 1 00:00:00 UTC to March 31 23:59:59.999 UTC)

### 17.5 Concurrency Gauge Correctness

The `concurrent_requests` gauge uses a RAII-style guard:

```rust
struct ConcurrencyGuard {
    gauge: Arc<AtomicU32>,
}

impl ConcurrencyGuard {
    fn acquire(gauge: Arc<AtomicU32>, limit: u32) -> Result<Self, ConcurrencyExceeded> {
        let prev = gauge.fetch_add(1, Ordering::AcqRel);
        if prev >= limit {
            gauge.fetch_sub(1, Ordering::Release);
            return Err(ConcurrencyExceeded);
        }
        Ok(Self { gauge })
    }
}

impl Drop for ConcurrencyGuard {
    fn drop(&mut self) {
        self.gauge.fetch_sub(1, Ordering::Release);
    }
}
```

The `Drop` implementation ensures the gauge is decremented even if the request handler
panics or the future is cancelled. This prevents gauge drift.

### 17.6 Graduated (Tiered) Pricing Calculation

When tiered pricing is configured:

```
tiers = [
  { up_to = 10_000_000,  per_million = 0.00 },   # Included
  { up_to = 50_000_000,  per_million = 0.25 },   # Tier 2
  { up_to = -1,          per_million = 0.15 },   # Tier 3
]

For usage = 75_000_000:
  Tier 1: min(75M, 10M) = 10M  → 10M * $0.00/M = $0.00
  Tier 2: min(75M - 10M, 50M - 10M) = 40M  → 40M * $0.25/M = $10.00
  Tier 3: 75M - 50M = 25M  → 25M * $0.15/M = $3.75
  Total: $13.75
```

Each tier is calculated independently (graduated model), not the entire volume at a
single tier rate (volume model). This matches Lago's graduated charge model and is the
most common approach in SaaS billing.

### 17.7 First Boot

On first startup with no existing database:
1. SQLite database is created with schema migrations
2. All apps start with zero usage counters
3. Billing period starts from current timestamp
4. Event log file is created
5. No history exists — `usage/history` endpoints return empty results

### 17.8 Accuracy vs Performance Trade-offs

| Decision | Trade-off | Rationale |
|---|---|---|
| Atomic counters (in-memory) | Fast but volatile | Sub-microsecond enforcement; 5s data loss window acceptable |
| 5-second flush interval | Slight staleness | Balance between disk I/O and durability |
| LRU dedup with 24h TTL | May miss duplicates after eviction | 1M capacity handles 11K distinct events/second |
| Relaxed atomic ordering | No sequential consistency | Monotonic counters only need eventual visibility |
| Event log async enqueue | May drop events under backpressure | Counters are source of truth; event log is supplementary |

### 17.9 Graceful Shutdown

On SIGTERM or SIGINT:

1. **Stop accepting new connections** (listener closed)
2. **Drain in-flight requests** (up to `graceful_shutdown_timeout_secs`)
3. **Flush atomic counters to SQLite** (final UsageFlusher run)
4. **Flush event log** (fsync remaining buffered events)
5. **Cancel pending webhooks** (they will retry on next startup from dead letter)
6. **Close SQLite connections**
7. **Exit**

If the timeout expires before drain completes, remaining requests are dropped and
a warning is logged. Counter data up to the last flush is preserved.

### 17.10 Performance Budget

The quota/metering system must impose minimal overhead on the request hot path:

| Operation | Budget | Mechanism |
|---|---|---|
| Rate limit check | < 100 ns | Atomic CAS on token bucket |
| Concurrency check | < 50 ns | Atomic fetch_add |
| Quota enforcement (all resources) | < 1 us | Sequential atomic loads |
| Meter recording (all resources) | < 500 ns | Sequential atomic fetch_add |
| Event log enqueue | < 200 ns | mpsc channel send (non-blocking) |
| Response header injection | < 500 ns | String formatting |
| **Total overhead per request** | **< 3 us** | |

For context: V8 isolate dispatch typically takes 50-500 us, and actual JS execution
takes 1-100 ms. The metering overhead is < 0.1% of request latency.

### 17.11 Trial Periods

Trial periods are supported via time-limited plan assignments:

```toml
[apps.new_customer]
plan = "pro"
trial_ends_at = "2026-04-30T23:59:59Z"     # Auto-downgrade after this
trial_downgrade_to = "free"                  # Plan to switch to
```

When `trial_ends_at` passes, the PeriodRoller automatically:
1. Changes the app's plan to `trial_downgrade_to`
2. Dispatches a `plan.changed` webhook with `reason: "trial_expired"`
3. Logs the transition

---

### 17.12 Comparison with Industry Platforms

| Capability | Appbase v2 | Cloudflare Workers | AWS Lambda | Vercel |
|---|---|---|---|---|
| Per-request CPU limit | Yes (kill) | Yes (10ms free, 30s paid) | No (billed) | No |
| Monthly quotas | Yes (configurable) | Daily (100K free) | Monthly (1M free) | Monthly |
| Rate limiting | Yes (token bucket) | No (handled by WAF) | Per-region concurrency | No |
| Spending limits | Yes (block/degrade) | No | No (use AWS Budgets) | Yes (added 2024) |
| Budget alerts | Yes (50/75/90/100%) | No | Via CloudWatch | Yes (50/75/100%) |
| Overage billing | Yes (configurable) | Automatic | Automatic | Automatic |
| Hard cap option | Yes (free tier) | Yes (free tier daily) | No | No (without spend mgmt) |
| Feature entitlements | Yes (per-plan) | Via plan tier | Via IAM | Via plan tier |
| Prepaid credits | Yes (wallets) | No | No | No |
| Usage event log | Yes (hash chain) | No (only analytics) | Via CloudWatch | No |
| Custom resources | Yes (plugin SDK) | No | No | No |
| Dry-run mode | Yes | No | No | No |

---

## 18. Future Considerations (Out of Scope for v2)

### 18.1 Distributed Metering (Multi-Node)

When Appbase scales beyond a single node:
- Each node maintains local atomic counters
- Periodic sync to central store (Redis or CockroachDB)
- Quota enforcement uses local counters (slightly stale) with periodic reconciliation
- Trade-off: up to `sync_interval` seconds of over-quota usage spread across nodes

### 18.2 Usage-Based Autoscaling

Auto-adjust plan limits based on patterns. Scale up during spikes, scale down during
quiet periods. Requires predictive modeling.

### 18.3 Cost Allocation Tags

Request-level labels (`team`, `environment`, `feature`) for internal chargeback:
```
X-Cost-Tags: team=backend,env=prod,feature=search
```

### 18.4 SLA Monitoring

Track uptime and latency SLAs per enterprise app. Auto-issue credits on SLA violations.

### 18.5 Marketplace Billing

Third-party plugin creators billing for their plugin usage through the platform.

### 18.6 Real-Time Usage Dashboard

WebSocket-based live usage dashboard for app developers, consuming the same atomic
counters used for enforcement.

---

## 19. Glossary

| Term | Definition |
|---|---|
| **App** | A tenant's deployed application, identified by `app_id` |
| **Billing period** | Time window for usage accumulation and invoicing |
| **Concurrency limit** | Maximum simultaneous in-flight requests per app |
| **Credit** | Prepaid monetary unit applied against usage charges |
| **Dead letter** | Webhook event that failed all delivery attempts |
| **Dedup set** | Bounded LRU cache of idempotency keys for duplicate prevention |
| **Dry run** | Enforcement mode that logs decisions but does not block |
| **Entitlement** | Boolean flag controlling access to a feature |
| **Event log** | Append-only record of all usage events (audit trail) |
| **Flush** | Periodic write of in-memory counters to persistent storage |
| **Grace period** | Time during which in-flight requests complete after limit is hit |
| **Hash chain** | Sequential SHA-256 linking of event log entries for tamper detection |
| **Idempotency key** | Unique string ensuring an event is processed at most once |
| **Metering** | Measuring and recording resource consumption |
| **Overage** | Usage beyond plan-included amounts, billed at per-unit rate |
| **Plan** | Named bundle of entitlements, quotas, rate limits, and policies |
| **Policy** | Set of threshold-action pairs defining enforcement behavior |
| **Proration** | Proportional charge adjustment for mid-period plan changes |
| **Quota** | Numeric cap on accumulated resource usage over a time window |
| **Rate limit** | Throughput cap enforced via token bucket algorithm |
| **Resource** | Measurable dimension of consumption (CPU, requests, storage, etc.) |
| **Shadow mode** | Run two configs simultaneously, compare enforcement decisions |
| **Token bucket** | Algorithm: bucket of N tokens, refilled at R/second, 1 consumed per request |
| **Wallet** | Container holding prepaid credit balance for an app |

---

## 20. References

- [Cloudflare Workers Pricing](https://developers.cloudflare.com/workers/platform/pricing/)
- [Cloudflare Workers Limits](https://developers.cloudflare.com/workers/platform/limits/)
- [AWS Lambda Pricing](https://aws.amazon.com/lambda/pricing/)
- [Vercel Spend Management](https://vercel.com/docs/spend-management)
- [Lago — Ingesting Usage Events](https://docs.getlago.com/guide/events/ingesting-usage)
- [Lago — Wallets & Prepaid Credits](https://docs.getlago.com/guide/wallet-and-prepaid-credits)
- [Lago — Why Billing Systems Are a Nightmare](https://www.getlago.com/blog/why-billing-systems-are-a-nightmare-for-engineers)
- [Stripe — Usage-Based Billing](https://docs.stripe.com/billing/subscriptions/usage-based)
- [IETF — RateLimit Header Fields (draft-ietf-httpapi-ratelimit-headers)](https://datatracker.ietf.org/doc/draft-ietf-httpapi-ratelimit-headers/)
- [RFC 7231 Section 7.1.3 — Retry-After](https://www.rfc-editor.org/rfc/rfc7231#section-7.1.3)
