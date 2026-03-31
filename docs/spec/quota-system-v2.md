# Appbase Quota, Metering & Billing System — v2 Design

> **Status:** Draft v2.2 | **Last Updated:** 2026-03-30 | **Author:** Platform Team
>
> **Revision History:**
> - v2.2 (2026-03-30): Round 2 review fixes. IETF RateLimit headers updated to
>   draft-ietf-httpapi-ratelimit-headers-10 (RateLimit combined field + RateLimit-Policy).
>   Replace bcrypt with SHA-256 for API key hashing (high-entropy keys don't need KDF).
>   Fix double-buffered rollover in-flight data race with epoch-based reclamation
>   (crossbeam-epoch). Assign distinct error codes -32029 to -32032. Rename spending
>   limit field to `limit` with currency inferred from app. Switch compare_exchange_weak
>   to compare_exchange (strong) in ConcurrencyGuard. Correct comparison table (AWS
>   Lambda 15-min timeout, Budgets ~12-24h delay, Vercel spend mgmt requires manual
>   resume). Flesh out multi-currency section fully. Fix AtomicPtr reclamation with
>   epoch-based RCU. Scale dedup capacity with deployment. Add 32-bit timestamp overflow
>   assertion. Tighten free tier wall time to 10s. Use secret_env for webhook secret.
>   Scale event log channel capacity. Fix pagination to use cursor-only for streaming.
>   Decouple trial expiry from PeriodRoller. Add rate limit on backfill API. Complete
>   glossary. Add app_id to dedup key. Add graceful metering degradation under memory
>   pressure. Add plan version rollback. Add concurrent config reload safety (RwLock).
>   Add storage quota 60-second enforcement gap mitigation.
> - v2.1 (2026-03-30): Address review feedback. Fix TOCTOU in ConcurrencyGuard (CAS loop),
>   pack token bucket state into single AtomicU64, correct memory orderings for ARM,
>   add SQLite write batching strategy, remove hash chain in favor of external anchoring,
>   add inline spend tracking, add RBAC with scoped API keys, specify webhook idempotency
>   contract, add double-buffered period rollover, add dunning/payment failure handling,
>   fix type safety (`Option<u64>` instead of `-1`), fix JSON examples, add NTP jump
>   handling for daily reset, fix event correction negative counter issue, correct
>   comparison table, add API versioning. Add missing concepts: multi-currency,
>   committed use discounts, usage backfill tooling, package/bundle pricing,
>   per-endpoint metering, cron/background job metering.
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
10. [Access Control & API Key Management](#10-access-control--api-key-management)
11. [Monitoring & Observability](#11-monitoring--observability)
12. [Billing Integration](#12-billing-integration)
13. [Webhook Notifications](#13-webhook-notifications)
14. [Security & Abuse Prevention](#14-security--abuse-prevention)
15. [Developer Experience](#15-developer-experience)
16. [Testing Strategy](#16-testing-strategy)
17. [Implementation Plan](#17-implementation-plan)
18. [Operational Guidance](#18-operational-guidance)
19. [Multi-Currency Support](#19-multi-currency-support)
20. [Package & Bundle Pricing](#20-package--bundle-pricing)
21. [Per-Endpoint Metering](#21-per-endpoint-metering)
22. [Metering for Cron & Background Jobs](#22-metering-for-cron--background-jobs)
23. [Usage Data Backfill Tooling](#23-usage-data-backfill-tooling)
24. [Future Considerations](#24-future-considerations)
25. [Glossary](#25-glossary)
26. [References](#26-references)

---

## 1. Overview

This document specifies the quota, metering, and billing system for the Appbase
multi-tenant app hosting platform. It governs resource definition, usage measurement,
limit enforcement, plan management, spending controls, billing integration, and
operational observability.

**TL;DR:** The system uses three layers — (1) in-memory atomic counters for real-time
enforcement at sub-microsecond latency, (2) periodic batched SQLite flushes for durability,
and (3) an append-only event log for audit/billing. Plans bundle entitlements, quotas, rate
limits, and policies. Spending limits prevent bill shock with inline per-request tracking.
The platform meters everything automatically; no SDK is needed for app developers. External
billing systems (Stripe, Lago) consume the usage data via Admin API and webhooks.

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

- Multi-region distributed metering (single-node; see Section 24 for future direction)
- Real-time payment processing (invoices are generated; payment is external)
- Self-service plan creation by app developers (platform owner only in v2)
- Tax calculation (delegated to payment processor)
- Per-user metering within an app (metering is per-app, not per-end-user)

---

## 2. Core Concepts

### 2.1 Concept Map

```
                    +----------+
                    |   PLAN   |  A named, versioned bundle
                    +----+-----+
                         | contains
          +--------------+--------------+
          v              v              v
   +-------------+ +----------+ +------------+
   | ENTITLEMENT | |  QUOTA   | | RATE LIMIT |
   | (boolean)   | | (cap/    | | (throughput |
   |             | |  window) | |  cap)       |
   +-------------+ +----+-----+ +-----+------+
                         |             |
                    references    references
                         |             |
                    +----v-----+       |
                    | RESOURCE |<------+
                    | (what is |
                    | consumed)|
                    +----+-----+
                         |
                    measured by
                         |
                    +----v-----+
                    | METERING |  Always on, independent
                    | (events  |  of plans
                    | +counters|
                    +----------+

   When a quota or rate limit is crossed -> POLICY defines what happens
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
| `gauge` | CAS loop inc/dec | Current instantaneous value | Concurrent connections |

**Memory ordering rules (ARM-correct):**

All atomic operations use explicit orderings for correctness on weakly-ordered
architectures (ARM/AArch64, e.g., AWS Graviton):

| Operation | Ordering | Rationale |
|---|---|---|
| Counter writes (`fetch_add`, `store`) | `Release` | Visible to subsequent `Acquire` loads on other cores |
| Counter reads (enforcement checks) | `Acquire` | See all prior `Release` writes from other cores |
| CAS loops (token bucket, concurrency) | `AcqRel` success, `Acquire` failure | Standard CAS read-modify-write |

`Relaxed` is insufficient on ARM: stores can be reordered with loads on other cores,
causing a quota check to see a stale value and incorrectly allow a blocked request.
`Acquire`/`Release` guarantees happens-before. The cost difference vs `Relaxed` is
negligible on x86 (strong ordering by default) and a few nanoseconds on ARM.

Note: `latest` uses `store(value, Release)` with a timestamp-guarded CAS for
newest-wins semantics. `count_unique` uses a HyperLogLog sketch, not an atomic counter.

**Built-in resources:**

| Resource | Unit | Category | Aggregation | Measurement Method |
|---|---|---|---|---|
| `cpu_ms` | ms | compute | sum | `CLOCK_THREAD_CPUTIME_ID` via `clock_gettime` |
| `wall_ms` | ms | compute | sum | `Instant::now()` delta |
| `memory_peak_mb` | MB | compute | max | V8 `GetHeapStatistics()` after execution |
| `requests` | count | compute | sum | +1 per RPC call |
| `concurrent_requests` | count | compute | gauge | RAII guard: CAS on entry, dec on exit (see 18.5) |
| `egress_bytes` | bytes | network | sum | `Content-Length` or chunked body byte count |
| `ingress_bytes` | bytes | network | sum | Request body byte count |
| `subrequests` | count | network | sum | `op_fetch` call counter |
| `db_reads` | count | database | sum | `op_db_find`/`op_db_get` counter |
| `db_writes` | count | database | sum | `op_db_insert`/`op_db_update`/`op_db_delete` counter |
| `db_storage_bytes` | bytes | storage | latest | `stat()` on SQLite file (sampled every 60s) + write-time estimate |
| `kv_reads` | count | database | sum | `op_kv_get`/`op_kv_list` counter |
| `kv_writes` | count | database | sum | `op_kv_set`/`op_kv_delete` counter |
| `kv_storage_bytes` | bytes | storage | latest | `stat()` on KV file (sampled every 60s) + write-time estimate |

**Storage quota enforcement gap mitigation:** Storage is sampled via `stat()` every 60s.
To close the gap: (1) each write op atomically adds estimated bytes to a
`storage_write_accumulator` (AtomicU64); enforcement uses `last_sampled + accumulator`.
(2) Accumulator resets on each `stat()` sample. (3) Estimates over-count (include page
overhead); deletes don't decrement (corrected by next sample). (4) When estimated size
crosses 90% of quota, an immediate `stat()` is triggered. This gives per-write
enforcement granularity with the 60s sample as the source of truth.

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
`{app_id}_{request_id}_{resource}` — the `app_id` prefix prevents cross-app
dedup collisions). A bounded LRU deduplication set tracks seen keys. Duplicates
are silently dropped at the hot tier. The event log also stores the key for
cold-tier deduplication during replay.

**Dedup capacity sizing:** The default 1M entries cycles through in ~6.7 seconds
at 10K unique keys/second (15 resources * ~667 req/s), rendering the 24h TTL
meaningless at high throughput — eviction is driven by LRU capacity, not TTL.
Operators **must** scale `dedup_capacity` with their deployment:

| Throughput | Recommended Capacity | Memory (~80 bytes/entry) |
|---|---|---|
| < 1K req/s | 1,000,000 (default) | ~80 MB |
| 1K-5K req/s | 5,000,000 | ~400 MB |
| 5K-10K req/s | 10,000,000 | ~800 MB |
| > 10K req/s | `throughput * 15 * 120` (2-minute window) | Scale accordingly |

The TTL serves as a secondary eviction policy for low-throughput deployments where
the LRU capacity is not reached. At high throughput, the effective dedup window
equals `capacity / (req_per_second * avg_resources_per_request)`.

**Backpressure:** The event log enqueue is a bounded channel. The default capacity of
10,000 fills in ~67ms at 150K events/s peak (10K req/s * 15 resources). Operators
should size `channel_capacity` based on expected peak throughput and acceptable drop
rate. A recommended formula: `channel_capacity = peak_events_per_second * flush_interval_ms / 1000 * 2` (2x headroom). For 10K req/s with 100ms flush:
`150000 * 0.1 * 2 = 30,000`. If the channel is full (event log writer is slow), the
metering pipeline:
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
  - When empty -> policy applies (usually "block" -> 429)
  - Bucket can never exceed capacity
```

**Why token bucket:** Handles bursty traffic gracefully while enforcing a sustained rate.
Used by Cloudflare Workers, AWS API Gateway, and nginx. Simpler than sliding window log
with comparable fairness properties.

**Implementation — packed single-AtomicU64 design:**

Two separate atomics (`tokens` and `last_refill`) cannot be updated atomically together,
creating a race where two threads double-refill. The solution packs state into one word:

```rust
/// Bit layout: [63..32] tokens (fixed-point *1000), [31..0] last_refill (epoch secs)
struct TokenBucket {
    state: AtomicU64,         // Packed: (tokens_fp << 32) | timestamp_secs
    capacity_fp: u32,         // = burst * 1000
    refill_rate_fp: u32,      // = max_per_second * 1000
}

impl TokenBucket {
    fn try_acquire(&self) -> bool {
        loop {
            let current = self.state.load(Ordering::Acquire);
            let tokens_fp = (current >> 32) as u32;
            let last_refill = current as u32;
            let now_secs = current_time_secs() as u32;
            let elapsed = now_secs.saturating_sub(last_refill);
            let refilled = (tokens_fp as u64)
                .saturating_add(elapsed as u64 * self.refill_rate_fp as u64)
                .min(self.capacity_fp as u64) as u32;
            if refilled < 1000 { return false; }
            let new_state = (((refilled - 1000) as u64) << 32) | (now_secs as u64);
            match self.state.compare_exchange_weak(
                current, new_state, Ordering::AcqRel, Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(_) => continue,
            }
        }
    }
}
```

Refill-and-consume is a single CAS, eliminating the TOCTOU race. This follows
Cloudflare's rate limiter and Linux kernel token bucket patterns.

**32-bit timestamp overflow:** The lower 32 bits store epoch seconds as `u32`, wrapping
at 2106-02-07. A startup assertion must verify:

```rust
fn assert_timestamp_safe() {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    assert!(now < (u32::MAX as u64 - 365 * 86400),
        "System clock within 1 year of u32 epoch overflow (2106). \
         Migrate token bucket to 64-bit timestamps.");
}
```

Checked at startup; `timestamp_overflow_warning` alert fires if within 5 years.

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
| `packages` | Option\<Vec\<PackageRef\>\> | Included package bundles (see Section 20) |
| `commitment` | Option\<CommitmentConfig\> | Minimum commitment terms (see Section 12.7) |

**Plan versioning:** Plans have a `version` field. When a plan definition changes in
config, the version must be explicitly incremented. Existing apps remain on their assigned
version until migrated via admin API. This prevents surprise behavior changes. Migration
can be done per-app or in bulk:

```
PUT  /v1/_admin/apps/{id}/plan         -- Single app migration
POST /v1/_admin/plans/{name}/migrate   -- Bulk: migrate all apps on this plan to new version
```

**Plan version rollback:** Apps can be rolled back to a previous version via
`PUT /v1/_admin/apps/{id}/plan { "plan": "pro", "version": 1 }` (single) or
`POST /v1/_admin/plans/{name}/migrate { "from_version": 2, "to_version": 1 }` (bulk).
Rollback behaves like a plan change: limits apply immediately, token buckets reinitialize,
`plan.changed` webhook fires with `reason: "version_rollback"`. Previous versions must exist
in config or SQLite `plan_versions` table; otherwise 409.

**Per-app overrides:** Individual quotas can be overridden without creating a custom plan:

```toml
[apps.my_blog.overrides.quotas]
requests = { max = 200000 }    # Override only this quota
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

All API endpoints are versioned under the `/v1/` prefix. See Section 9 for details.

```toml
# appbase.toml

[server]
port = 3000
host = "0.0.0.0"
graceful_shutdown_timeout_secs = 30   # Drain in-flight requests before exit

[admin]
cors_origins = ["https://dashboard.example.com"]  # CORS for browser dashboard
rate_limit = 100                      # Admin API rate limit (req/s)
read_only = false                     # Set true for monitoring-only instances

# ---- Access Control (RBAC) ----
# See Section 10 for full RBAC documentation
[admin.keys.root]
secret_env = "APPBASE_ROOT_KEY"       # Env var containing the key
role = "super_admin"

[admin.keys.billing_service]
secret_env = "APPBASE_BILLING_KEY"
role = "billing"
scopes = ["read:usage", "read:invoices", "write:wallets"]

[admin.keys.monitoring]
secret_env = "APPBASE_MONITORING_KEY"
role = "viewer"
scopes = ["read:usage", "read:health", "read:metrics"]

[admin.keys.team_ops]
secret_env = "APPBASE_OPS_KEY"
role = "operator"
scopes = ["read:*", "write:apps", "write:overrides"]
app_filter = ["team_a_*", "team_b_*"]  # Glob pattern: only these apps

[isolates]
max = 1000
idle_timeout_secs = 60

# ---- Billing ----

[billing]
period = "calendar_month"       # "calendar_month" | "anniversary"
default_currency = "usd"        # Default currency (see Section 19 for multi-currency)
timezone = "UTC"                # For daily resets and period boundaries
overage_enabled = false         # Global default; per-plan override

# ---- Spending Controls ----

[spending]
enabled = true
check_interval_secs = 10       # Background reconciliation interval
inline_tracking = true         # Per-request spend estimation (see Section 6)
default_alert_thresholds = [50, 75, 90, 100]
alert_channels = ["webhook"]

# ---- Pricing (flat per-unit; tiered pricing uses [[pricing.X.tiers]] array) ----
[pricing.requests]
per_million = 0.30
[pricing.cpu_ms]
per_million = 0.02
[pricing.egress]
per_gb = 0.09
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
# Tiered: [[pricing.requests.tiers]] with { up_to, per_million }. Final tier omits up_to.

# ---- Resources ----
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

# ---- Policies ----

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

# ---- Plans ----

[plans.free]
description = "Free tier -- development and small projects"
version = 1
[plans.free.entitlements]
custom_domains = false
cron_jobs = false
websockets = false
max_apps = 3
[plans.free.quotas]
cpu_ms              = { max = 10000,          period = "monthly",      policy = "warn_then_block" }
cpu_ms_per_request  = { resource = "cpu_ms",  max = 10,               period = "per_request",  policy = "hard_kill" }
wall_ms_per_request = { resource = "wall_ms", max = 10000,            period = "per_request",  policy = "hard_kill" }
requests            = { max = 100000,         period = "monthly",     policy = "warn_then_block" }
requests_daily      = { resource = "requests", max = 10000,           period = "daily",        policy = "warn_then_block" }
egress_bytes        = { max = 1000000000,     period = "monthly",     policy = "warn_then_block" }
db_storage_bytes    = { max = 500000000,      period = "absolute",    policy = "block_writes_only" }
memory_peak_mb      = { max = 128,            period = "absolute",    policy = "hard_kill" }
concurrent          = { resource = "concurrent_requests", max = 5,    period = "absolute",    policy = "soft_block" }
# ... (additional resource quotas follow same pattern for ingress, db_reads/writes, kv_*)
[plans.free.rate_limits]
requests = { max_per_second = 10, burst = 50, policy = "soft_block" }

[plans.pro]
description = "Pro tier -- production apps"
version = 1
[plans.pro.entitlements]
custom_domains = true
cron_jobs = true
websockets = true
max_apps = 25
[plans.pro.quotas]
cpu_ms              = { max = 30000000,       period = "monthly",     policy = "progressive" }
cpu_ms_per_request  = { resource = "cpu_ms",  max = 30000,            period = "per_request", policy = "hard_kill" }
requests            = { max = 10000000,       period = "monthly",     policy = "progressive" }
egress_bytes        = { max = 100000000000,   period = "monthly",     policy = "progressive" }
db_storage_bytes    = { max = 10000000000,    period = "absolute",    policy = "block_writes_only" }
concurrent          = { resource = "concurrent_requests", max = 50,   period = "absolute",    policy = "soft_block" }
[plans.pro.rate_limits]
requests = { max_per_second = 1000, burst = 5000, policy = "soft_block" }
[plans.pro.overage]
enabled = true
requests = { per_million = 0.30 }
cpu_ms = { per_million = 0.02 }
egress = { per_gb = 0.09 }

[plans.enterprise]
description = "Enterprise tier -- custom limits, SLA"
version = 1
[plans.enterprise.entitlements]
custom_domains = true
cron_jobs = true
websockets = true
priority_support = true
# max_apps omitted = unlimited (Option<u64>, absence means no limit)
[plans.enterprise.quotas]
cpu_ms_per_request = { resource = "cpu_ms", max = 300000, period = "per_request", policy = "hard_kill" }
memory_peak_mb     = { max = 256, period = "absolute", policy = "hard_kill" }
concurrent = { resource = "concurrent_requests", max = 500, period = "absolute", policy = "soft_block" }
# No monthly quotas -- governed by contract + spending limits
[plans.enterprise.rate_limits]
requests = { max_per_second = 10000, burst = 50000, policy = "soft_block" }

# ---- Defaults ----

[defaults]
plan = "free"
enforcement_mode = "enforce"    # "enforce" | "dry_run" | "shadow"

# ---- Apps ----

[apps.my_todo]
plan = "pro"

[apps.vip_client]
plan = "enterprise"

[apps.my_blog]
plan = "free"
[apps.my_blog.overrides.quotas]
requests = { max = 200000 }

[apps.my_blog.spending]
limit = 0.00                      # Currency inferred from app (or billing.default_currency)
action = "block"
auto_resume = true

# ---- Webhooks ----

[webhooks]
url = "https://example.com/appbase-events"
secret_env = "APPBASE_WEBHOOK_SECRET"          # Env var containing HMAC-SHA256 signing secret
events = ["quota.*", "spending.*", "billing.*"] # Event name prefix matching
timeout_ms = 10000
max_retries = 5

# ---- Metering ----

[metering]
flush_interval_secs = 5        # Counter -> SQLite flush interval
dedup_capacity = 1000000       # Max entries in dedup LRU set
dedup_ttl_hours = 24           # TTL for dedup entries

# ---- Event Log ----

[event_log]
enabled = true
retention_days = 90
max_size_mb = 1024
flush_interval_ms = 100        # Batch write interval
channel_capacity = 30000       # Bounded async channel; size per peak throughput (see 2.5)
```

### 3.2 Configuration Validation Rules

Validated at startup and on hot reload. Failures are fatal at startup; rejected on reload
with a warning log (old config remains active).

**Concurrent reload safety:** Config is behind `RwLock<Config>`. Request path holds a
read guard (cheap, concurrent). Reload acquires a write guard, atomically swapping the
entire config (no partial visibility). Concurrent reloads are serialized; a
reload-in-progress flag prevents SIGHUP from queueing unbounded reloads.

**Hot reload behavior** (`POST /v1/_admin/config/reload` or `SIGHUP`):
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
| V3 | Quota `max` must be a positive integer | Invalid max value |
| V4 | Rate limit `burst` must be >= `max_per_second` | Burst must be >= sustained rate |
| V5 | Plan `version` must be a positive integer | Invalid plan version |
| V6 | `spending.limit` must be >= 0 | Negative spending limit |
| V7 | Alert thresholds must be in [1, 100], sorted ascending, no duplicates | Invalid thresholds |
| V8 | Entitlement keys must match `^[a-z][a-z0-9_]*$` | Invalid entitlement key |
| V9 | Resource names must match `^[a-z][a-z0-9_]*$` | Invalid resource name |
| V10 | Policy thresholds must be in (0, 100], sorted ascending | Invalid policy threshold |
| V11 | Apps must reference existing plans | Unknown plan "{name}" |
| V12 | Override quotas must reference resources that exist in the plan or globally | Unknown resource in override |
| V13 | Webhook URL must be valid HTTPS (HTTP allowed only for localhost) | Insecure webhook URL |
| V14 | `[pricing]` entries must reference defined resources | Unknown resource in pricing |
| V15 | Tiered pricing final tier must omit `up_to` (unlimited) | Final tier must be unbounded |
| V16 | API key roles must be one of: `super_admin`, `admin`, `operator`, `billing`, `viewer` | Invalid role |

**Type safety note:** Unlimited values (e.g., `max_apps` on enterprise) are represented
as `Option<u64>` in Rust. Omitting a field means unlimited. The previous convention of
using `-1` for unlimited is **removed** as it is type-unsafe (mixing signed semantics
into unsigned fields). Configuration using `-1` will fail validation with:
`"Use Option<u64> (omit field) instead of -1 for unlimited"`.

Admin API endpoint for pre-flight validation:
```
POST /v1/_admin/config/validate
Body: raw TOML content
Response: { "valid": true, "errors": [{ "rule": "V1", "message": "...", "location": "..." }] }
```

---

## 4. Architecture

### 4.1 System Overview

```
REQUEST PATH (top to bottom):
  HTTP Request
    -> Rate Limiter (single-AtomicU64 CAS, 2.7) -> 429
    -> Concurrency Guard (CAS loop, 18.5)        -> 429
    -> Entitlement Checker                        -> 403
    -> Quota Enforcer + inline spend check        -> 429 / warn headers
    -> V8 Isolate Execution (CPU/wall watchdog)   -> 503 on limit
    -> Meter Recorder (atomic counters + inline spend + event log enqueue)
    -> Response Header Injector (X-Quota-*, RateLimit/RateLimit-Policy, X-CPU-*)
    -> Concurrency Guard Drop (decrement gauge)
  HTTP Response

BACKGROUND SERVICES:
  Usage Flusher (5s), Event Logger, Period Roller (double-buffered, epoch-reclaimed),
  Storage Sampler (60s), Spending Reconciler (10s), Alert Dispatcher,
  Dedup Janitor (hourly), Dunning Manager (12.8), TrialManager (60s),
  MemoryWatchdog (5s)

STORAGE LAYER:
  Atomic Counters (per-app per-resource), Token Buckets (packed AtomicU64),
  Concurrency Gauges (AtomicU32 CAS), Spend Accumulators (AtomicU64),
  Dedup LRU (1M, 24h TTL), SQLite (usage history), Event Log (append-only,
  externally anchored), Plan Registry, Webhook Queue + dead letter
```

### 4.2 Metering Pipeline

```
Request completes -> Build UsageDelta { request_id, app_id, timestamp, deltas[] }
  For each (resource, value):
    1. Dedup check: idempotency_key = "{app_id}_{req_id}_{resource}"
    2. counter[app][resource].fetch_add(value, Release)      ~10ns
    3. spend_accumulator[app].fetch_add(delta_cents, Release)
  Enqueue UsageDelta to event_log_channel (bounded mpsc, 10K; drop on full)
  Background: EventLogger batch-writes to file; UsageFlusher batch-upserts to SQLite
```

### 4.3 Period Rollover — Double-Buffered Design

The naive approach of locking per-app counters during rollover causes a brief 503 for
all apps whose periods end at the same time (e.g., all calendar_month apps at midnight
UTC on the 1st). This is unacceptable at scale.

**Double-buffered counters** eliminate this:

For each affected app (lock-free):
1. Allocate new counter set ("buffer B") for the new period
2. Atomically swap the active buffer pointer (`AtomicPtr` CAS) -- new requests
   immediately write to buffer B. No 503, no pause, no lock.
3. **Epoch-based reclamation (drain in-flight writers):** After the swap, in-flight
   requests may still write to buffer A. We use `crossbeam-epoch` for safe reclamation:
   - Each request pins the epoch before loading the active buffer pointer.
   - The rollover thread swaps the pointer, reads final values from old buffer A, then
     calls `guard.defer_destroy()` — the old buffer is freed only after all threads
     pinned in the prior epoch have unpinned (completed their request).
   - **Alternative (simpler):** drain-and-wait: sleep for `2 * max_request_latency`
     after swap. Bounded data loss (a few straggler writes) recoverable from event log.
4. Read final values from old buffer A (safe: all in-flight writers drained per step 3)
5. INSERT snapshot into usage_history, update period metadata
6. Dispatch `billing.period_end` webhook
7. If spend-blocked and `auto_resume=true`: unblock

**AtomicPtr reclamation safety:** Raw `AtomicPtr::swap` without reclamation causes
use-after-free. `crossbeam-epoch` provides lock-free RCU: request threads `pin()` on
entry, `unpin()` on exit (RAII). `defer_destroy` defers deallocation until all prior-epoch
threads complete.

For `calendar_month`, rollover processes apps in batches of 100 with 10ms sleep to
avoid SQLite write spikes.

### 4.4 SQLite Write Strategy at Scale

**Problem:** At 10K apps with 15 resources, a naive per-row INSERT every 5 seconds
produces 150K individual writes, which saturates SQLite's single-writer lock.

**Solution: Write coalescing with batched transactions.**

1. Snapshot all per-app counters (skip zero-delta entries -- typically 60-80%)
2. Single transaction with prepared statement reuse:
   ```sql
   BEGIN IMMEDIATE;
   INSERT INTO usage_current (app_id, resource, value, updated_at)
     VALUES (?1, ?2, ?3, ?4)
     ON CONFLICT(app_id, resource) DO UPDATE SET value = value + excluded.value,
       updated_at = excluded.updated_at;
   COMMIT;
   ```
3. One WAL fsync per flush. ~50ms for 60K upserts on NVMe SSD.

**Extreme scale (>10K apps):** Shard by app_id hash across multiple SQLite databases
(~2K apps per shard), or store per-app counter snapshots as single MessagePack BLOBs.

### 4.5 Counter Overflow

All atomic counters are `AtomicU64`. Overflow analysis:
- At 1B requests/second: 584 years to overflow
- At 1 TB/second egress: 213 days to overflow

Defensive handling: counters saturate at `u64::MAX - 1` (using `fetch_update` with
checked addition). A `counter_overflow` alert fires if saturation is reached.

### 4.6 Crash Recovery

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
  "idempotency_key": "my_todo_req_01JQRA7XYZ_cpu_ms",
  "app_id": "my_todo",
  "resource": "cpu_ms",
  "value": 4,
  "timestamp": "2026-03-30T14:22:01.123Z",
  "period": "2026-03",
  "plan": "pro",
  "plan_version": 1,
  "endpoint": "todos.list",
  "source": "request",
  "metadata": {
    "request_id": "req_01JQRA7XYZ",
    "method": "todos.list"
  }
}
```

| Field | Type | Required | Description |
|---|---|---|---|
| `event_id` | ULID | Yes | Globally unique, sortable identifier |
| `idempotency_key` | String | Yes | Deduplication key (`{app_id}_{request_id}_{resource}`; app_id prefix prevents cross-app collision) |
| `app_id` | String | Yes | Tenant identifier |
| `resource` | String | Yes | Resource name (must match defined resource) |
| `value` | u64 | Yes | Consumption amount (>= 0, integer — fractional ms stored as microseconds) |
| `timestamp` | ISO 8601 | Yes | Event time, always UTC, millisecond precision |
| `period` | String | Yes | Billing period (YYYY-MM) |
| `plan` | String | Yes | Plan name at time of event |
| `plan_version` | u32 | Yes | Plan version at time of event |
| `endpoint` | String | No | RPC method name for per-endpoint metering (see Section 21) |
| `source` | String | Yes | Event source: `"request"`, `"cron"`, `"background"` (see Section 22) |
| `metadata` | Object | No | Arbitrary context for debugging |

### 5.2 Event Log Integrity

Each event log entry includes:
- The event payload (JSON)
- Entry sequence number

**Integrity via periodic external digest anchoring:**

The event log uses **periodic external digest anchoring** rather than an internal hash
chain. An internal chain (`H(n) = SHA256(H(n-1) || payload)`) is security theater: an
attacker with write access can rewrite the entire chain. External anchoring provides
genuine tamper evidence, following AWS CloudTrail (S3 digest files) and Sigstore/Rekor.

Every 10,000 entries or 300 seconds, a SHA-256 digest of recent entries is computed and
posted to a configured external store (S3 Object Lock, Rekor, or a read-only-mounted
file). Anchors are also cached locally in SQLite for fast verification.

```toml
[event_log.anchoring]
enabled = true
interval_entries = 10000
interval_secs = 300
destinations = ["file:///var/appbase/anchors/", "https://rekor.example.com/api/v1/log"]
```

`GET /v1/_admin/event_log/verify` recomputes digests per anchor range and cross-checks
against external records.

### 5.3 Late-Arriving Events

Events with timestamps in a closed billing period:
1. Accepted into the event log (never rejected)
2. Flagged with `"late_arrival": true`
3. Do NOT update the archived period's counters automatically
4. Visible via `GET /v1/_admin/reconciliation`
5. Can be applied via `POST /v1/_admin/reconciliation/apply` (admin action, audit-logged)

### 5.4 Event Corrections

To correct a previously recorded event:

```json
{
  "event_id": "evt_01JQRA8CORRECTED",
  "correction_for": "evt_01JQRA7XYZABC123",
  "app_id": "my_todo",
  "resource": "cpu_ms",
  "value": 3,
  "original_value": 4,
  "reason": "Measurement included system overhead"
}
```

**Correction semantics:** The correction event records both the new value and the original
value explicitly. The counter adjustment is `new_value - original_value`. This prevents
negative counter drift that can occur when the original event was deduplicated or already
corrected.

**Safeguards:**
- If `original_value` does not match the value in the referenced event, the correction
  is rejected with a 409 Conflict error. The operator must re-fetch the current value.
- If the referenced event was already corrected, the correction is rejected (only one
  correction per event; create a new correction referencing the correction event instead).
- Counter floor: counters are clamped to zero. If a correction would produce a negative
  value, the counter is set to zero and a `counter_floor_clamped` warning is logged.

Corrections require admin privilege and are audit-logged.

---

## 6. Spending Limits & Budget Alerts

### 6.1 Motivation

Vercel's lack of spending limits was a well-documented problem, leading to unexpected
bills of thousands of dollars. Their eventual spend management system (2024) still
requires manual per-project unpausing after the limit is hit. Appbase addresses cost
predictability from day one.

### 6.2 Inline Spend Tracking

**Problem:** Checking spending only every 60 seconds creates a blind spot where up to
60K requests (at 1K req/s) can accumulate unbilled. For high-throughput apps, this means
significant overshoot before the spending monitor catches up.

**Solution:** Each request atomically updates a per-app spend accumulator
(`AtomicU64::fetch_add(delta_cents, Release)`). Pre-request, the Quota Enforcer checks
`spend_blocked[app].load(Acquire)`. When the accumulator exceeds the limit, the app is
blocked and a `spending.blocked` webhook fires.

The background SpendingReconciler (every 10s) recomputes exact spend from counter
snapshots and corrects drift. Inline tracking uses fixed-point integer arithmetic
(tenths of a cent, always under-counts -- safe direction). Max drift per 10s window:
~$6 at 1K req/s, corrected at next reconciliation.

### 6.3 Configuration

```toml
# Platform-level defaults
[spending]
enabled = true
check_interval_secs = 10           # Background reconciliation interval
inline_tracking = true             # Per-request spend estimation
default_alert_thresholds = [50, 75, 90, 100]

# Per-app override
[apps.my_blog.spending]
limit = 50.00                    # In app's currency (or billing.default_currency)
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
SpendingReconciler tick (every 10s)
   |
   +-- For each app with spending limit:
   |     reconcile spend_accumulator with calculated spend
   |     for each threshold [50, 75, 90, 100]:
   |       if spend >= threshold% of limit:
   |         if not already_alerted[app][threshold]:
   |           dispatch: spending.threshold webhook
   |           already_alerted[app][threshold] = true
   |
   +-- At 100%:
         execute configured action (block/degrade/warn/webhook)
         dispatch: spending.blocked webhook
```

### 6.6 Resume

| Config | At Period Boundary |
|---|---|
| `auto_resume = true` | Automatically unblocked, counters reset |
| `auto_resume = false` | Stays blocked until `POST /v1/_admin/apps/{id}/spending/resume` |

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
  "auto_topup": { "enabled": true, "threshold_cents": 1000, "amount_cents": 10000 },
  "expires_at": null,
  "transactions": [
    { "type": "topup", "amount_cents": 10000, "timestamp": "...", "note": "Initial deposit" },
    { "type": "deduction", "amount_cents": -2500, "period": "2026-02", "note": "Feb usage" }
  ]
}
```

### 7.3 Credit Application Order

When calculating an invoice:
1. Calculate total usage charges (overage beyond included amounts)
2. Subtract credits from wallet (oldest transactions first — FIFO)
3. Remaining amount = billable to payment method

Expired credits are skipped during application and logged as `credit.expired`.

### 7.4 Admin API

```
GET    /v1/_admin/apps/{id}/wallet                -- Balance and config
POST   /v1/_admin/apps/{id}/wallet/topup          -- Add credits { amount_cents, note }
POST   /v1/_admin/apps/{id}/wallet/deduct         -- Deduct credits { amount_cents, note }
GET    /v1/_admin/apps/{id}/wallet/transactions   -- Transaction history (paginated)
```

---

## 8. Response Headers & Error Responses

### 8.1 Standard Response Headers

Present on every successful response:

```http
X-Request-Id: req_01JQRA7XYZ
X-CPU-Time-Ms: 0.67              X-Wall-Time-Ms: 27.01
X-Plan: pro                       X-Plan-Version: 1
X-Quota-Resource: requests        X-Quota-Used: 42
X-Quota-Limit: 10000000          X-Quota-Remaining: 9999958
X-Quota-Reset: 2026-04-01T00:00:00Z
RateLimit: limit=1000, remaining=997, reset=3
RateLimit-Policy: 1000;w=1;burst=5000
X-Enforcement-Mode: dry_run      # Only present if not "enforce"
```

`X-Quota-*` shows the most constrained resource (lowest `remaining / limit` ratio).

**RateLimit header convention:** Per
[draft-ietf-httpapi-ratelimit-headers-10](https://datatracker.ietf.org/doc/draft-ietf-httpapi-ratelimit-headers/):
`RateLimit` is a combined field (`limit`, `remaining`, `reset` in seconds);
`RateLimit-Policy` describes the policy (`<limit>;w=<window>[;burst=<burst>]`).
The older separate `RateLimit-Limit`/`-Remaining`/`-Reset` headers are **not** used.
`X-Quota-*` headers are Appbase-specific (no IETF draft covers quota headers).

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
    "code": -32030,
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

Other error types follow the same JSON-RPC 2.0 structure with `data.type` discriminator:
- **Spending limit (429):** code `-32031`, `data.type = "spending_limit"` with `current_spend`, `limit`, `currency`, `auto_resume`, `period_end`
- **Concurrency limit (429):** code `-32032`, `data.type = "concurrency_limit"` with `current`, `limit`
- **Entitlement denied (403):** code `-32033`, `data.type = "entitlement_denied"` with `feature`, `plan`, `required_plans`, `upgrade_url`
- **Per-request limit (503):** code `-32034`, `data.type = "execution_limit"` with `resource`, `used_ms`, `limit_ms`

### 8.4 Error Code Catalog

All error codes used by the quota/metering system:

| Code | HTTP | Type | Description |
|---|---|---|---|
| `-32029` | 429 | `quota_exceeded` | Monthly/daily quota reached |
| `-32030` | 429 | `rate_limited` | Token bucket exhausted |
| `-32031` | 429 | `spending_limit` | Spending limit reached |
| `-32032` | 429 | `concurrency_limit` | Max concurrent requests |
| `-32033` | 403 | `entitlement_denied` | Feature not available on plan |
| `-32034` | 503 | `execution_limit` | Per-request CPU/wall limit (kill) |

Note: The previous `period_rollover` error code is eliminated by the double-buffered
rollover design (Section 4.3). Rollover no longer causes any request failures.

All codes are within the JSON-RPC 2.0 implementation-defined server error range
(-32000 to -32099). Each error type has a distinct code for unambiguous programmatic
handling. The `data.type` field provides a human-readable discriminator.

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

---

## 9. Admin API

All admin endpoints are served under the `/v1/` prefix for API versioning. Future
breaking changes will use `/v2/`, with the previous version supported for at least
12 months after deprecation.

All admin endpoints require authentication via scoped API keys (see Section 10).
Destructive operations require `X-Confirm: true` header. Responses are JSON with a
standard envelope:

**Offset-paginated endpoints** (small, bounded collections like apps, plans):
```json
{
  "data": [],
  "meta": { "total": 42, "limit": 20, "offset": 0, "has_more": true },
  "warnings": []
}
```

**Cursor-paginated endpoints** (unbounded/streaming like events, transactions):
```json
{
  "data": [],
  "meta": { "limit": 100, "next_cursor": "evt_01JQRA7XYZ", "has_more": true },
  "warnings": []
}
```

Each endpoint uses **one** pagination style, never both. The `meta` shape indicates
which style is in use. See Section 9.9 for details.

**Concurrency control:** Mutating endpoints use optimistic concurrency via `ETag`/
`If-Match` headers. Example:
```
GET /v1/_admin/apps/my_blog -> ETag: "v3"
PUT /v1/_admin/apps/my_blog/plan  If-Match: "v3"  -> 200 OK (or 409 Conflict if changed)
```

### 9.1 Apps

| Method | Path | Required Scope | Description |
|---|---|---|---|
| GET | `/v1/_admin/apps` | `read:apps` | List apps with plan and usage summary |
| GET | `/v1/_admin/apps/{id}` | `read:apps` | App details (plan, entitlements, quotas, usage) |
| PUT | `/v1/_admin/apps/{id}/plan` | `write:apps` | Change plan `{ "plan": "pro", "version": 1 }` |
| PATCH | `/v1/_admin/apps/{id}/overrides` | `write:overrides` | Set per-app quota overrides |
| DELETE | `/v1/_admin/apps/{id}/overrides` | `write:overrides` | Remove all overrides |
| POST | `/v1/_admin/apps/{id}/reset` | `write:apps` | Reset current period counters (X-Confirm required) |
| DELETE | `/v1/_admin/apps/{id}` | `admin:apps` | Evict isolate + clear all state (X-Confirm required) |
| PUT | `/v1/_admin/apps/{id}/enforcement_mode` | `write:apps` | Set mode `{ "mode": "dry_run" }` |

### 9.2 Usage & Metering

| Method | Path | Required Scope | Description |
|---|---|---|---|
| GET | `/v1/_admin/apps/{id}/usage` | `read:usage` | Current period usage (all resources) |
| GET | `/v1/_admin/apps/{id}/usage?resource=cpu_ms` | `read:usage` | Single resource breakdown |
| GET | `/v1/_admin/apps/{id}/usage/history` | `read:usage` | Historical periods (paginated) |
| GET | `/v1/_admin/apps/{id}/usage/history/{period}` | `read:usage` | Specific period detail |
| GET | `/v1/_admin/apps/{id}/events` | `read:events` | Raw events (paginated, filterable) |
| GET | `/v1/_admin/usage/summary` | `read:usage` | Platform-wide current period summary |
| GET | `/v1/_admin/usage/top?resource=cpu_ms&limit=10` | `read:usage` | Top N apps by resource |

Query parameters for list endpoints: `?limit=N&offset=M&sort=field&order=asc|desc`
Event filtering: `?resource=X&from=ISO8601&to=ISO8601&method=X&source=request|cron|background`

### 9.3 Plans

| Method | Path | Required Scope | Description |
|---|---|---|---|
| GET | `/v1/_admin/plans` | `read:plans` | List all plans |
| GET | `/v1/_admin/plans/{name}` | `read:plans` | Plan details (current version) |
| GET | `/v1/_admin/plans/{name}/apps` | `read:plans` | Apps on this plan |
| POST | `/v1/_admin/plans/{name}/migrate` | `admin:plans` | Bulk migrate apps to latest version |
| POST | `/v1/_admin/plans/{name}/simulate` | `read:plans` | Simulate effect of plan on an app's current usage |

### 9.4 Spending & Billing

| Method | Path | Required Scope | Description |
|---|---|---|---|
| GET | `/v1/_admin/apps/{id}/spending` | `read:usage` | Current spend estimate |
| PUT | `/v1/_admin/apps/{id}/spending` | `write:apps` | Set spending config |
| POST | `/v1/_admin/apps/{id}/spending/resume` | `write:apps` | Resume spend-paused app |
| GET | `/v1/_admin/apps/{id}/invoice/preview` | `read:invoices` | Preview next invoice |
| GET | `/v1/_admin/apps/{id}/invoice/history` | `read:invoices` | Historical invoice snapshots |

### 9.5 Credits

| Method | Path | Required Scope | Description |
|---|---|---|---|
| GET | `/v1/_admin/apps/{id}/wallet` | `read:wallets` | Wallet balance and config |
| POST | `/v1/_admin/apps/{id}/wallet/topup` | `write:wallets` | Add credits `{ amount_cents, note }` |
| POST | `/v1/_admin/apps/{id}/wallet/deduct` | `write:wallets` | Deduct credits `{ amount_cents, note }` |
| GET | `/v1/_admin/apps/{id}/wallet/transactions` | `read:wallets` | Transaction history |

### 9.6 Operations

| Method | Path | Required Scope | Description |
|---|---|---|---|
| GET | `/v1/_admin/health` | `read:health` | System health |
| GET | `/v1/_admin/metrics` | `read:metrics` | Prometheus-format metrics |
| POST | `/v1/_admin/flush` | `admin:ops` | Force flush counters to SQLite |
| POST | `/v1/_admin/config/validate` | `admin:config` | Validate TOML without applying |
| POST | `/v1/_admin/config/reload` | `admin:config` | Hot reload configuration |
| GET | `/v1/_admin/event_log/verify` | `read:events` | Verify event log integrity |
| GET | `/v1/_admin/reconciliation` | `read:events` | Late-arriving events report |
| POST | `/v1/_admin/reconciliation/apply` | `admin:events` | Apply late events to closed period |
| GET | `/v1/_admin/webhooks/failed` | `read:webhooks` | Failed webhook deliveries |
| POST | `/v1/_admin/webhooks/{id}/replay` | `write:webhooks` | Re-send a failed webhook |
| POST | `/v1/_admin/webhooks/test` | `write:webhooks` | Send a test event |

### 9.7 API Key Management

| Method | Path | Required Scope | Description |
|---|---|---|---|
| GET | `/v1/_admin/keys` | `admin:keys` | List all API keys (secrets redacted) |
| POST | `/v1/_admin/keys` | `admin:keys` | Create new API key |
| DELETE | `/v1/_admin/keys/{id}` | `admin:keys` | Revoke an API key |
| POST | `/v1/_admin/keys/{id}/rotate` | `admin:keys` | Rotate key (old key valid for grace period) |

### 9.8 Data Export

| Method | Path | Required Scope | Description |
|---|---|---|---|
| GET | `/v1/_admin/export/usage?from=...&to=...&format=csv` | `read:usage` | Export usage data |
| GET | `/v1/_admin/export/events?from=...&to=...&format=jsonl` | `read:events` | Export raw events |

Supported formats: `csv`, `json`, `jsonl` (newline-delimited JSON).

### 9.9 Pagination

Each endpoint uses exactly **one** pagination style, determined by the nature of its data.
Responses never mix offset and cursor fields.

**Offset-based** (bounded, small collections):
```
GET /v1/_admin/apps?limit=20&offset=40&sort=app_id&order=asc
Response meta: { "total": 42, "limit": 20, "offset": 40, "has_more": false }
```
Used by: `/apps`, `/plans`, `/keys`, `/exchange_rates`.

**Cursor-based** (unbounded, append-only, or streaming):
```
GET /v1/_admin/apps/{id}/events?limit=100&cursor=evt_01JQRA7XYZ
Response meta: { "limit": 100, "next_cursor": "evt_01JQRB8ABC", "has_more": true }
```
Used by: `/events`, `/wallet/transactions`, `/usage/history`, `/export/*`,
`/webhooks/failed`, `/reconciliation`.

The cursor is an opaque string (typically the last event_id or ULID). Offset-based
pagination is not supported on cursor endpoints (offset on append-only data is
unreliable and produces inconsistent results under concurrent writes).

### 9.10 Health Check Response

Returns `status`, `version`, `uptime_seconds`, and per-component health (rate_limiter,
usage_flusher, event_logger, period_roller, spending_monitor, storage_sampler,
webhook_dispatcher, dunning_manager, trial_manager, memory_watchdog) each with
`status` and component-specific metrics.

---

## 10. Access Control & API Key Management

### 10.1 Motivation

A single admin API key is insufficient for production use. Different services and team
members need different levels of access. This follows the principle of least privilege,
as implemented by Stripe (restricted keys), Cloudflare (API tokens with permissions),
and AWS (IAM policies).

### 10.2 Roles

| Role | Description | Default Scopes |
|---|---|---|
| `super_admin` | Full access, can manage keys | `*` (all scopes) |
| `admin` | Full access except key management | `read:*`, `write:*`, `admin:*` except `admin:keys` |
| `operator` | Manage apps and quotas, no billing | `read:*`, `write:apps`, `write:overrides` |
| `billing` | Read usage, manage wallets and invoices | `read:usage`, `read:invoices`, `write:wallets` |
| `viewer` | Read-only access | `read:*` |

### 10.3 Scopes

Scopes follow `action:resource` pattern. **Read:** `read:apps`, `read:usage`,
`read:events`, `read:plans`, `read:invoices`, `read:wallets`, `read:health`,
`read:metrics`, `read:webhooks`. **Write:** `write:apps`, `write:overrides`,
`write:wallets`, `write:webhooks`. **Admin:** `admin:apps`, `admin:plans`,
`admin:config`, `admin:events`, `admin:ops`, `admin:keys`.
Wildcard: `read:*` = all reads, `*` = everything.

### 10.4 App Filtering

Keys can be restricted to apps matching glob patterns via `app_filter = ["frontend_*"]`.
Non-matching apps: 404 on detail, 403 on write, omitted from lists.

### 10.5 Key Rotation

`POST /v1/_admin/keys/{id}/rotate` returns new key. Both old and new keys valid for
grace period (default: 24h, configurable via `key_rotation_grace_hours`).

### 10.6 Key Format

Prefix `abk_` + 32 random bytes (base62). Stored as `SHA-256(key)` hashes.

**Why SHA-256, not bcrypt:** API keys are 32 random bytes (192 bits entropy in base62) —
not human passwords, cannot be brute-forced. bcrypt at ~10ms/hash = 10 CPU-s/s at 100
req/s. SHA-256 on high-entropy input is pre-image resistant and adds < 1 us per lookup.
This follows Stripe, GitHub, and AWS for API key verification.

Plaintext is shown once at creation and never stored. Every admin API call is
audit-logged (timestamp, key ID, role, endpoint, parameters, status, IP).

---

## 11. Monitoring & Observability

### 11.1 Prometheus Metrics

Exported at `GET /v1/_admin/metrics`:

Key metric families (labels: app, plan, resource, decision, status as applicable):

```prometheus
appbase_resource_usage_total          # Per-app per-resource usage counters
appbase_enforcement_total             # Per-app enforcement decisions (allow/warn/block)
appbase_rate_limited_total            # Rate limit rejections per app
appbase_spend_estimate_cents          # Current spend estimate per app
appbase_spend_blocked                 # Whether app is spend-blocked (0/1)
appbase_meter_flush_duration_seconds  # Flush latency histogram (p50, p99)
appbase_event_log_drops_total         # Events dropped due to backpressure
appbase_quota_check_duration_seconds  # Enforcement check latency (p50, p99)
appbase_active_isolates               # Current V8 isolate count
appbase_webhook_sent_total            # Webhook delivery by status (success/failed)
appbase_admin_api_requests_total      # Admin API calls by key_id, method, path
appbase_admin_api_auth_failures_total # Auth failures by reason (invalid_key/insufficient_scope)
appbase_dunning_apps_in_grace_total   # Apps in dunning grace period
appbase_dunning_apps_suspended_total  # Apps suspended for non-payment
```

### 11.2 Structured Logging

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

### 11.3 Alert Rules

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
| `payment_failed` | Payment attempt failed (dunning) | Error | Webhook |

---

## 12. Billing Integration

### 12.1 Supported Billing Models

| Model | Base Fee | Usage Charges | Hard Caps | Example |
|---|---|---|---|---|
| Flat subscription | Yes | No | Yes | Free tier ($0), basic ($10/mo) |
| Subscription + overage | Yes | Beyond included | No | Pro tier ($20/mo + usage) |
| Pure usage-based | No | All usage | Optional | API-only tier |
| Tiered (graduated) | Yes/No | Per-tier rates | Optional | Volume discount |
| Committed use (credits) | Prepaid | Deducted from wallet | Optional | Enterprise |
| Package bundles | Add-on | Included in package | Yes | Feature packs (see Section 20) |

### 12.2 Invoice Preview

```json
{
  "app_id": "my_todo",
  "period": { "start": "2026-03-01T00:00:00Z", "end": "2026-03-31T23:59:59Z" },
  "plan": { "name": "pro", "version": 1 },
  "currency": "usd",
  "line_items": [
    { "type": "subscription", "description": "Pro plan -- March 2026", "amount_cents": 2000 },
    { "type": "commitment_base", "amount_cents": 5000, "note": "Usage below commitment floor" },
    { "type": "overage", "resource": "requests", "overage_quantity": 2500000, "amount_cents": 75 },
    { "type": "overage", "resource": "egress_bytes", "overage_bytes": 50000000000, "amount_cents": 450 },
    { "type": "credit", "description": "Wallet credit applied", "amount_cents": -525 }
  ],
  "subtotal_cents": 2525,
  "credits_applied_cents": 525,
  "total_cents": 2000,
  "status": "preview"
}
```

### 12.3 Billing Period Configuration

| Type | When Counters Reset | Best For |
|---|---|---|
| `calendar_month` | 1st at 00:00 UTC | Simplicity, standard SaaS |
| `anniversary` | Signup day each month | Per-customer billing dates |

### 12.4 Plan Changes (Proration)

| Scenario | Effective Limits | Billing |
|---|---|---|
| Upgrade mid-period | New limits immediately | Prorated charge: `(remaining_days / total_days) * price_diff` |
| Downgrade mid-period | Old limits until period end | New price starts next period, no refund |
| Cancel | Drops to free-tier limits | No refund for current period |

### 12.5 Overage

When `overage.enabled = true` on a plan:
- Monthly quotas become **soft limits** (usage beyond max is allowed)
- The enforcement policy is overridden to `allow` + `notify` beyond 100%
- Overage is billed at the configured per-unit rate
- Spending limits still apply as a hard ceiling on total cost

### 12.6 External Billing System Integration

Appbase generates usage data; external systems handle payment:

```
                    Appbase                      External
                  +----------+               +-------------+
  billing.period  |          |  GET /invoice  |             |
  _end webhook -->| Admin API|-->  preview -->| Stripe/Lago |
                  |          |               |             |
                  | GET      |  usage data   | Create      |
                  | /export  |-->-->-->-->-->-| invoice     |
                  |          |               |             |
                  | POST     |  on payment   | Process     |
                  | /wallet  |<--<--<--<--<--| payment     |
                  | /topup   |               |             |
                  +----------+               +-------------+
```

**Stripe integration pattern:**
1. `billing.period_end` webhook fires
2. Your service calls `GET /v1/_admin/apps/{id}/invoice/preview`
3. Creates Stripe invoice with line items from preview
4. Stripe processes payment
5. On success: `POST /v1/_admin/apps/{id}/wallet/topup` for credit-based model
6. On failure: Stripe fires `invoice.payment_failed` -> your service calls
   `POST /v1/_admin/apps/{id}/dunning/start` (see Section 12.8)

**Lago integration pattern:**
1. During the period: forward usage events to Lago's event API
2. Lago handles aggregation, invoice generation, and payment
3. Appbase handles enforcement; Lago handles billing

### 12.7 Minimum Commitment / Committed Use Discounts

Enterprise customers often commit to a minimum monthly spend in exchange for lower
per-unit rates. This follows the model used by AWS Reserved Instances, GCP Committed
Use Discounts, and Orb's minimum commitments.

**Configuration:**

```toml
[plans.enterprise_committed]
description = "Enterprise with $500/mo minimum commitment"
version = 1

[plans.enterprise_committed.commitment]
minimum_cents = 50000               # $500/month minimum
term_months = 12                    # Contract duration
start_date = "2026-01-01"          # Contract start
discount_pct = 20                   # 20% discount on all usage rates
rollover = false                    # Unused commitment does NOT roll over
```

**Invoice behavior:**
- If usage charges >= `minimum_cents`: bill usage charges (with discount applied)
- If usage charges < `minimum_cents`: bill `minimum_cents` (the commitment floor)
- The invoice preview shows a `commitment_adjustment_cents` line item when the
  floor applies

**Tracking:**
- `GET /v1/_admin/apps/{id}/commitment` returns commitment status, remaining term,
  utilization percentage
- `commitment.underutilized` webhook fires if utilization < 50% at mid-period

### 12.8 Dunning — Payment Failure Handling

When payment fails, the platform needs a clear escalation path. This follows Stripe's
dunning model (retry schedule + grace period + eventual suspension).

**Flow:** `payment_failed -> grace_period (3d) -> retry_1 (d3) -> retry_2 (d7) -> retry_3 (d14) -> suspended`

```toml
[billing.dunning]
enabled = true
grace_period_days = 3               # App runs normally during grace
retry_schedule_days = [3, 7, 14]    # Days after failure to retry
suspension_action = "degrade"       # "degrade" | "block" | "webhook"
auto_reinstate_on_payment = true    # Restore plan when payment succeeds
```

At each stage, webhooks fire (`payment.failed`, `payment.retry`, `payment.suspended`).
Suspension actions: `degrade` (free-tier limits), `block` (429 all requests), or
`webhook` (external system decides). On payment success,
`POST /v1/_admin/apps/{id}/dunning/resolve` reinstates the original plan.

**Admin API:** `GET/POST /v1/_admin/apps/{id}/dunning` (state), `/dunning/start`,
`/dunning/resolve`, `/dunning/override` (extend grace, skip to suspend).

### 12.9 Graduated (Tiered) Pricing Calculation

When tiered pricing is configured:

```
tiers = [
  { up_to: 10000000,  per_million: 0.00 },   # Included
  { up_to: 50000000,  per_million: 0.25 },   # Tier 2
  { per_million: 0.15 },                      # Tier 3 (unlimited, no up_to)
]

For usage = 75,000,000:
  Tier 1: min(75M, 10M) = 10M  ->  10M * $0.00/M = $0.00
  Tier 2: min(75M - 10M, 50M - 10M) = 40M  ->  40M * $0.25/M = $10.00
  Tier 3: 75M - 50M = 25M  ->  25M * $0.15/M = $3.75
  Total: $13.75
```

Each tier is calculated independently (graduated model), not the entire volume at a
single tier rate (volume model). This matches Lago's graduated charge model and is the
most common approach in SaaS billing.

---

## 13. Webhook Notifications

### 13.1 Event Catalog

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
| `payment.failed` | Payment attempt failed | period, amount_cents, reason |
| `payment.retry` | Payment retry attempted | attempt, final, amount_cents |
| `payment.suspended` | App suspended for non-payment | action, period |
| `payment.resolved` | Payment received after dunning | period, amount_cents |
| `plan.changed` | Plan changed | old_plan, new_plan, old_version, new_version |
| `app.created` | New app registered | app_id, plan |
| `app.deleted` | App removed | app_id |
| `credit.applied` | Credits deducted for usage | amount_cents, remaining_cents |
| `credit.expired` | Credits expired unused | amount_cents |
| `commitment.underutilized` | Commitment < 50% utilized at mid-period | utilization_pct |
| `system.flush_error` | Counter flush failed | error_message |

### 13.2 Payload Format

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

### 13.3 Delivery & Security

- **Transport:** HTTPS POST with `Content-Type: application/json`
- **Signature:** `X-Appbase-Signature: sha256=<hex>` computed as `HMAC-SHA256(secret, body)`
- **Verification:** Receiver computes HMAC and compares (constant-time) to header
- **Secret rotation:** `POST /v1/_admin/webhooks/rotate_secret` generates a new secret.
  Both old and new secrets are valid for a configurable grace period (default: 24 hours).
- **Timeout:** 10 seconds per attempt
- **Retries:** 5 attempts with exponential backoff (1s, 5s, 30s, 5m, 30m)
- **Dead letter:** After 5 failures, event stored in dead letter queue
- **Replay:** `POST /v1/_admin/webhooks/{id}/replay`
- **Dedup:** Same event type + app + threshold fires at most once per billing period
- **Event filtering:** The `events` config uses prefix matching with `*` wildcard.
  `"quota.*"` matches `quota.warning`, `quota.exceeded`, `quota.recovered`.
  `"*"` matches all events. Exact names (e.g., `"quota.warning"`) match only that event.

### 13.4 Receiver Idempotency Contract

Webhook receivers **must** handle duplicate deliveries idempotently. Appbase guarantees
at-least-once delivery but not exactly-once. Duplicates can occur due to:
- Network timeouts where the receiver processed the event but Appbase did not see the 2xx
- Retries after transient failures
- Manual replay via admin API

**Contract for receivers:**

1. **Use the `id` field as idempotency key.** Store processed IDs and skip duplicates.
2. **Respond 2xx within 10 seconds.** Non-2xx or timeout triggers retry.
3. **Process async if needed.** Return 200 immediately, process in background.
4. **Track IDs for 48+ hours** (covers retry window + manual replays).
5. **Verify HMAC signature** before processing (`X-Appbase-Signature` header).

---

## 14. Security & Abuse Prevention

### 14.1 Tenant Isolation

| Vector | Mitigation |
|---|---|
| One app consuming all CPU | Per-request CPU time limit (V8 interrupt) |
| One app consuming all memory | V8 isolate heap limit; OOM kills only that isolate |
| One app sending burst traffic | Per-app rate limit (token bucket) |
| One app opening many connections | Per-app concurrency limit |
| One app filling disk | Per-app storage quota (block_writes policy) |
| Cross-app counter contamination | Independent counter maps per app_id |

### 14.2 Admin API Security

- **Authentication:** Scoped API keys (see Section 10) or mTLS
- **Authorization:** RBAC with per-key scopes and optional app filtering
- **Rate limiting:** Admin API has its own rate limit (default: 100 req/s)
- **Audit log:** Every admin action is recorded with timestamp, key ID, key role,
  action, parameters, and result
- **Destructive safeguards:** DELETE and reset endpoints require `X-Confirm: true`
- **Read-only mode:** Can run admin API in read-only mode for monitoring without risk

### 14.3 Anti-Gaming

| Attack | Prevention |
|---|---|
| Bypass metering from user code | Metering in Rust runtime, outside V8 sandbox |
| Manipulate CPU time measurement | `CLOCK_THREAD_CPUTIME_ID` owned by platform |
| Reset own counters | No user-facing counter reset API |
| Replay events to deflate usage | Idempotency keys prevent duplicate processing |
| Forge event timestamps | Timestamps set by platform, not user |

### 14.4 Denial-of-Wallet Prevention

An attacker targeting another user's app could exhaust their quota or spending limit:

| Layer | Protection |
|---|---|
| 1. Rate limit | Caps request throughput regardless of origin |
| 2. Per-IP rate limit (opt-in) | `[apps.X.ip_rate_limit] max_per_second = 5` |
| 3. Concurrency limit | Prevents slow-request resource exhaustion |
| 4. Spending limit | Caps total financial exposure per period |
| 5. Anomaly alerts | `usage_spike` alert on >3x baseline |

### 14.5 Data Integrity

| Mechanism | Purpose |
|---|---|
| `Acquire`/`Release` atomic ordering | Correct lock-free counter updates on ARM and x86 |
| CAS loops for token bucket and concurrency | Race-free state transitions |
| SQLite WAL + `PRAGMA synchronous=NORMAL` | Durable flush with good performance |
| Event log with external digest anchoring | Tamper detection backed by external trust root |
| Idempotency dedup set | Prevent double-counting |
| Checksums on exported data | Verify export integrity |

---

## 15. Developer Experience

### 15.1 For Platform Owners

| Capability | How |
|---|---|
| Define plans | TOML config: `[plans.X]` sections |
| Per-app customization | `[apps.X.overrides.quotas]` without new plan |
| Scoped access control | RBAC with per-key scopes and app filters (Section 10) |
| Validate changes before deploy | `POST /v1/_admin/config/validate` with TOML body |
| Test new limits safely | `enforcement_mode = "dry_run"` per app |
| Simulate plan effects | `POST /v1/_admin/plans/{name}/simulate` |
| Hot reload | `POST /v1/_admin/config/reload` or `kill -HUP <pid>` |
| Monitor everything | Prometheus metrics + structured logs |
| Export for analytics | CSV/JSONL export endpoints |
| Integrate billing | Invoice preview API + webhook events |
| Handle payment failures | Dunning system with configurable escalation (Section 12.8) |

### 15.2 For App Developers

| Need | Solution |
|---|---|
| Know my current usage | `X-Quota-*` headers on every response |
| Know when I am close to limits | `X-Quota-Warning` headers at 80% |
| Know why my request failed | Structured error with resource, used, limit, reset, upgrade URL |
| Know when to retry | `Retry-After` header on 429 responses |
| Avoid surprise bills | Spending limits, budget alerts, hard caps on free tier |
| Access gated features | Clear entitlement error with required plan name |

### 15.3 Plugin Metering SDK

```rust
state.meter("ai_tokens", 150);                // Never fails, panics, or blocks
state.meter("egress_bytes", size as u64);     // Thread-safe, any async context
match state.quota_available("ai_tokens") {    // Atomic load (Acquire), no I/O
    QuotaAvailable::Yes(remaining) => { /* proceed */ },
    QuotaAvailable::No { used, limit } => { /* reject */ },
    QuotaAvailable::Unlimited => { /* no quota */ },
}
```

---

## 16. Testing Strategy

**Unit:** Policy evaluation, token bucket CAS (no double-refill), concurrency guard CAS
(no slot leaks), quota enforcer decisions, config validation (V1-V16), invoice
calculation (overage, credits, commitments), event correction (original_value matching,
counter floor), RBAC (scopes, app filtering, key rotation).

**Integration:** Full request flow with headers, double-buffered rollover (no 503),
inline spend tracking, crash recovery, hot reload, dunning flow, SQLite batching.

**Load:** Enforcement < 1us p99, 100 concurrent atomic correctness, 10K events/s no
drops, 60K row flush < 100ms, token bucket CAS under 1K concurrent requests.

**Utilities:** Mock clock, dry-run mode, shadow mode, `_test` app with 1-minute periods.

---

## 17. Implementation Plan

| Phase | Deliverable | Deps | Effort |
|---|---|---|---|
| 1 | Core types + config parser (Option<u64> for unlimited) | — | S |
| 2 | Atomic counters (correct orderings) + concurrency guard (CAS) | 1 | S |
| 3 | Token bucket (packed AtomicU64 CAS) | 1 | M |
| 4 | Quota enforcer + inline spend tracking | 1,2 | M |
| 5 | Tower middleware (full request path) + response headers | 2,3,4 | M |
| 6 | Entitlement checker | 1,5 | S |
| 7 | SQLite store + batched flush + crash recovery | 2 | M |
| 8 | Event log (append-only + external anchoring) | 2 | M |
| 9 | Period rollover (double-buffered) | 7 | M |
| 10 | Admin API (versioned /v1/) + RBAC + scoped keys | 2,4,7 | L |
| 11 | Spending monitor + alerts + webhooks | 7,10 | M |
| 12 | Invoice preview + billing + credits/wallets | 7,11 | M |
| 13 | Dunning + packages + commitments | 11,12 | M |
| 14 | Per-endpoint + cron/background metering | 2,4 | M |
| 15 | Multi-currency + backfill tooling + data export | 12 | M |
| 16 | Prometheus metrics + dry-run/shadow modes | all | M |

Effort: S = 1-2 days, M = 3-5 days, L = 1-2 weeks.
**Critical path:** 1 -> 2 -> 4 -> 5 -> 7 -> 10 (core enforcement + persistence + admin)

---

## 18. Operational Guidance

### 18.1 Capacity Planning

**Example:** 1000 apps, 15 resources, 10K req/s, 90-day retention:
- Memory: ~100 MB (dominated by 1M-entry dedup set at ~80 bytes/entry)
- Disk: ~50 GB/year event log (~10 GB compressed), ~500 MB SQLite history
- Per-app overhead: 120B counters + 16B token bucket + 8B spend accumulator

### 18.2 Data Retention

Atomic counters: current period. Usage history: 24 months (configurable). Event log:
90 days (configurable). Dead letters: 30 days. Audit log: permanent. Dedup set: 24h TTL.

### 18.3 Deleted App Behavior

When an app is deleted via `DELETE /v1/_admin/apps/{id}`:
1. V8 isolate is evicted immediately
2. Atomic counters are dropped
3. Usage history is **retained** for billing purposes (marked `deleted = true`)
4. Event log entries are **retained** (immutable by design)
5. Webhooks for the deleted app are cancelled
6. The app_id cannot be reused for 30 days (prevents confusion in billing data)

### 18.4 Clock and Time Handling

- All internal timestamps use `Instant::now()` (monotonic) for duration measurement
- All external timestamps use `SystemTime` in UTC with millisecond precision
- Daily resets occur at 00:00 UTC regardless of server timezone
- **NTP jump handling for daily reset:**
  - Forward jumps: If `SystemTime` jumps forward by more than 5 minutes, the daily
    reset timer recomputes the next reset time. If the jump skips past a reset point,
    the system performs an immediate catch-up reset with a `daily_reset_catchup` log
    entry. Counters are snapshotted at the jump-detected time (not the skipped time).
  - Backward jumps: If `SystemTime` jumps backward by > 5 seconds, a warning is logged.
    The daily reset timer uses the monotonic clock (`Instant`) as the primary trigger,
    with `SystemTime` only for labeling. This means a backward NTP adjustment does NOT
    cause a duplicate daily reset.
  - Events with timestamps > 60 seconds in the future are rejected.
- Monthly period boundaries are calculated using calendar arithmetic (e.g., March billing
  period = March 1 00:00:00 UTC to March 31 23:59:59.999 UTC)

### 18.5 Concurrency Guard — CAS-Based Acquire

The `concurrent_requests` gauge uses a compare-and-swap loop instead of
`fetch_add` + `fetch_sub`, eliminating the TOCTOU race where a slot could be
temporarily "leaked" between the add and the limit check.

**Problem with fetch_add:** `fetch_add(1)` then checking `if prev >= limit` and doing
`fetch_sub(1)` creates a TOCTOU race -- the gauge is transiently over-limit, causing
other threads to incorrectly reject requests.

**CAS-based solution:**

```rust
impl ConcurrencyGuard {
    fn acquire(gauge: Arc<AtomicU32>, limit: u32) -> Result<Self, ConcurrencyExceeded> {
        loop {
            let current = gauge.load(Ordering::Acquire);
            if current >= limit { return Err(ConcurrencyExceeded); }
            match gauge.compare_exchange(
                current, current + 1, Ordering::AcqRel, Ordering::Acquire,
            ) {
                Ok(_) => return Ok(Self { gauge }),
                Err(_) => continue,
            }
        }
    }
}
impl Drop for ConcurrencyGuard {
    fn drop(&mut self) { self.gauge.fetch_sub(1, Ordering::Release); }
}
```

`compare_exchange` (strong) is used instead of `_weak`. On current ARM (LDXR/STXR),
`_weak`'s theoretical advantage does not materialize — both emit the same loop. Strong
avoids spurious retries that buy nothing. `Drop` ensures decrement on panic/cancel.

### 18.6 First Boot

On first startup with no existing database:
1. SQLite database is created with schema migrations
2. All apps start with zero usage counters
3. Billing period starts from current timestamp
4. Event log file is created
5. No history exists — `usage/history` endpoints return empty results

### 18.7 Accuracy vs Performance Trade-offs

| Decision | Trade-off | Rationale |
|---|---|---|
| Atomic counters (in-memory) | Fast but volatile | Sub-microsecond enforcement; 5s data loss window acceptable |
| 5-second flush interval | Slight staleness | Balance between disk I/O and durability |
| LRU dedup with 24h TTL | May miss duplicates after eviction | 1M capacity handles 11K distinct events/second |
| Acquire/Release atomic ordering | Slightly more expensive than Relaxed on ARM | Required for correctness on weakly-ordered CPUs; negligible cost on x86 |
| Event log async enqueue | May drop events under backpressure | Counters are source of truth; event log is supplementary |
| Inline spend tracking | ~10ns overhead per request | Eliminates 60-second blind spot; negligible vs V8 execution time |

### 18.8 Graceful Shutdown

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

### 18.9 Performance Budget

The quota/metering system must impose minimal overhead on the request hot path:

| Operation | Budget | Mechanism |
|---|---|---|
| Rate limit check | < 100 ns | Single AtomicU64 CAS on token bucket |
| Concurrency check | < 50 ns | AtomicU32 CAS loop |
| Quota enforcement (all resources) | < 1 us | Sequential atomic loads (Acquire) |
| Inline spend check | < 20 ns | Single AtomicU64 load (Acquire) |
| Meter recording (all resources) | < 500 ns | Sequential atomic fetch_add (Release) |
| Inline spend update | < 20 ns | Single AtomicU64 fetch_add (Release) |
| Event log enqueue | < 200 ns | mpsc channel send (non-blocking) |
| Response header injection | < 500 ns | String formatting |
| **Total overhead per request** | **< 3 us** | |

For context: V8 isolate dispatch typically takes 50-500 us, and actual JS execution
takes 1-100 ms. The metering overhead is < 0.1% of request latency.

### 18.10 Graceful Metering Degradation Under Memory Pressure

The metering pipeline degrades gracefully under memory pressure rather than crashing:

| Level | Trigger (RSS) | Response |
|---|---|---|
| Normal | < 80% limit | Full pipeline |
| Warning | >= 80% | Shrink dedup LRU to 50%; log warning |
| Critical | >= 90% | Shrink dedup to 10%; disable event log enqueue; stop HyperLogLog |
| Emergency | >= 95% | Disable dedup (accept double-counting); counters + SQLite only |

`MemoryWatchdog` checks RSS every 5s (`[server] memory_limit_mb = 2048`). Degradation is
automatic/reversible. `appbase_memory_pressure_level` gauge tracks level (0-3).

### 18.11 Trial Periods

Trial periods are supported via time-limited plan assignments:

```toml
[apps.new_customer]
plan = "pro"
trial_ends_at = "2026-04-30T23:59:59Z"     # Auto-downgrade after this
trial_downgrade_to = "free"                  # Plan to switch to
```

Trial expiry is handled by a **dedicated TrialManager background task**, not the
PeriodRoller. The PeriodRoller runs on billing period boundaries (monthly/daily),
which may not align with trial end dates. The TrialManager checks trial deadlines
every 60 seconds independently:

1. Scan apps where `trial_ends_at <= now` and `trial_active = true`
2. Change the app's plan to `trial_downgrade_to`
3. Set `trial_active = false` to prevent re-processing
4. Dispatch a `plan.changed` webhook with `reason: "trial_expired"`
5. Log the transition

A `trial.expiring_soon` webhook fires 72 hours before expiry (checked once per hour).

### 18.12 Comparison with Industry Platforms

| Capability | Appbase v2 | CF Workers | AWS Lambda | Vercel |
|---|---|---|---|---|
| Per-request CPU limit | Yes (kill) | Yes (10ms free/30s paid) | 15 min max timeout (billed) | 10s-300s (plan-dependent) |
| Spending limits | Yes (inline, real-time) | No | Budgets (~12-24h delay, alerting only, no hard stop) | Yes (requires manual per-project unpause) |
| Hard cap option | Yes (any plan) | Free tier only | No (Budgets are alerts, not enforcement) | With spend mgmt (manual resume) |
| Prepaid credits | Yes (wallets) | No | Savings Plans / Reserved | No |
| Dunning | Yes | N/A | N/A | Automatic |
| Usage event log | Yes (ext. anchored) | Workers Analytics (sampled) | CloudWatch Logs | No |
| Custom resources | Yes (plugin SDK) | No | No | No |
| RBAC for admin API | Yes (scoped keys) | API tokens | IAM policies | Team roles (coarse) |
| Per-endpoint metering | Yes | No | Per-function only | No |
| Committed use discounts | Yes | No | Savings Plans / Compute SP | No |
| Dry-run/shadow mode | Yes | No | No | No |

*AWS Lambda: 15-min max timeout, billed per 1ms. AWS Budgets: 12-24h delay, SNS alerts
only (no hard enforcement without custom integration). Vercel Spend Mgmt: pauses
deployments at limit, requires manual per-project unpause (no auto-resume).*

---

## 19. Multi-Currency Support

### 19.1 Motivation

Customers in different regions expect to see prices and invoices in their local currency.
Stripe, Lago, and Orb all support multi-currency billing. Without multi-currency,
international customers see USD amounts that do not match their payment statements,
creating confusion and support burden.

### 19.2 Design Principles

1. **Single internal ledger currency:** All metering, counters, and spend accumulators
   operate in the platform's base currency (configurable, default: USD cents). This
   avoids floating-point drift from repeated conversions.
2. **Conversion at the boundary:** Currency conversion occurs at two points only:
   - **Display time** (invoice preview, spend estimate API, dashboard) — uses the
     rate locked at invoice generation time.
   - **Payment time** — handled by the external payment processor (Stripe/Lago), which
     applies its own FX rate at charge time.
3. **Per-app currency assignment:** Each app has an optional `currency` field. If omitted,
   `billing.default_currency` applies.

### 19.3 Configuration

```toml
[billing]
default_currency = "usd"

# Exchange rates: base currency -> target currency multiplier.
# Updated via config or Admin API. Rates are point-in-time snapshots.
[billing.exchange_rates]
eur = 0.92
gbp = 0.79
jpy = 149.50
last_updated = "2026-03-30T00:00:00Z"

[apps.eu_customer]
currency = "eur"

[apps.jp_customer]
currency = "jpy"
```

### 19.4 Exchange Rate Management

Rates defined in `[billing.exchange_rates]` or updated via Admin API:
`GET/PUT /v1/_admin/exchange_rates`, `GET /v1/_admin/exchange_rates/history`.
Rate updates are audit-logged. Stale rates (> 48h) trigger `exchange_rate_stale` alert.
Automated FX provider integration (Open Exchange Rates, ECB) is a future enhancement.

### 19.5 Invoice Behavior

For non-default currency apps: (1) calculate line items in base currency, (2) convert
to app currency at generation-time rate, (3) record both `amount_base_cents` and
`amount_local` per line, (4) record `exchange_rate` and `rate_timestamp` on invoice.

```json
{
  "app_id": "eu_customer", "currency": "eur",
  "exchange_rate": 0.92, "rate_timestamp": "2026-03-30T00:00:00Z",
  "line_items": [{ "type": "subscription", "amount_base_cents": 2000, "amount_local": 1840 }],
  "total_local": 1840
}
```

### 19.6 Spend Limits and Credits in Multi-Currency

Spending limits (`limit`) are in the app's currency; the inline accumulator tracks base
currency internally and converts the limit at each reconciliation tick. Wallet credits
are in the app's currency; applied at the invoice's locked rate. Cross-currency credit
transfers are not supported in v2.

### 19.7 Supported Currencies

ISO 4217 currencies accepted by the payment processor. Minimum: USD, EUR, GBP, JPY,
CAD, AUD, CHF, CNY, INR, BRL. Zero-decimal currencies (JPY, KRW) use integer amounts;
detected from a built-in ISO 4217 table.

### 19.8 Limitations

Rate updates do not retroactively change existing invoices. FX rounding is toward the
platform (round up charges, round down credits). Base-currency ledger is source of truth.

---

## 20. Package & Bundle Pricing

### 20.1 Motivation

Many SaaS platforms (AWS, Cloudflare, Vercel) offer add-on packages that bundle
additional quota or features for a fixed price, independent of the base plan.
Examples: "AI Package" ($10/mo for 1M AI tokens), "Storage Pack" ($5/mo for 10GB).

### 20.2 Configuration

```toml
[packages.ai_starter]
description = "AI Starter Pack"
price_cents = 1000
[packages.ai_starter.quotas]
ai_tokens = { max = 1000000 }
[packages.ai_starter.entitlements]
ai_models_v2 = true

[packages.storage_10gb]
description = "Extra 10GB Storage"
price_cents = 500
stackable = true                      # Can purchase multiple
[packages.storage_10gb.quotas]
db_storage_bytes = { max = 10000000000 }
```

Apps assign packages: `packages = ["ai_starter", "storage_10gb", "storage_10gb"]`.
Quotas are **additive** with plan quotas. Entitlements are **OR-merged**. Packages
appear as separate invoice line items. Admin API: `GET/PUT /v1/_admin/apps/{id}/packages`.

---

## 21. Per-Endpoint Metering

### 21.1 Motivation

Operators need visibility into which RPC methods consume the most resources, for
cost attribution, optimization, and potential per-endpoint pricing. This follows
the per-route analytics offered by Cloudflare Workers and AWS API Gateway.

### 21.2 How It Works

Every usage event includes an `endpoint` field (auto-populated from JSON-RPC `method`).
Per-endpoint aggregates are stored in SQLite (`endpoint_usage` table, keyed by
`app_id, endpoint, resource, period`). Flushed alongside per-app counters using
in-memory HashMaps (informational, not enforcement-critical).

**Admin API:** `GET /v1/_admin/apps/{id}/usage/endpoints` (all, filtered, or top N).
Per-endpoint pricing is a future consideration (see Section 24).

---

## 22. Metering for Cron & Background Jobs

### 22.1 Motivation

Apps may run scheduled tasks (cron jobs) or background processes that consume resources
outside of the request-response cycle. These must be metered with the same accuracy
as HTTP requests.

### 22.2 Event Source Tagging

Every usage event has a `source` field:

| Source | Description | Trigger |
|---|---|---|
| `request` | Normal HTTP request-response | RPC call from client |
| `cron` | Scheduled job execution | Timer-triggered by platform |
| `background` | Background task (e.g., webhooks, async processing) | Platform-initiated |

### 22.3 Cron Job Metering

Cron jobs get a synthetic `request_id` (`cron_{app_id}_{job_name}_{timestamp}`), run
through the same V8 + metering pipeline, and tag events with `source: "cron"`. Quota
enforcement applies identically -- cron usage counts toward monthly quotas.

### 22.4 Background Jobs

Background work (webhooks, async processing) is tagged `source: "background"` and
metered identically. All sources share the same quota pool (prevents gaming). Operators
can filter by source in per-endpoint analytics (Section 21).

---

## 23. Usage Data Backfill Tooling

### 23.1 Motivation

When new resources are defined, pricing changes, or events are missed due to system
issues, operators need to backfill or re-derive usage data. This follows the pattern
used by Orb (invoice void + re-rate) and Lago (event replay).

### 23.2 Backfill API

`POST /v1/_admin/backfill/events` accepts an array of events with `source: "backfill"`,
each with idempotency keys. Options: `apply_to_counters` (default: false for dry-run),
`apply_to_invoices` (default: false). Closed-period events handled per Section 5.3.
Requires `admin:events` scope.

**Rate limiting:** The backfill endpoint is rate-limited to prevent accidental counter
corruption or SQLite write saturation from runaway scripts:
- **Default:** 10 requests/second, burst 50 (configurable via `[admin.backfill_rate_limit]`)
- **Max batch size:** 1,000 events per request
- Exceeding either limit returns 429 with `Retry-After`
- Each backfill request is audit-logged with event count and affected app_ids

### 23.3 Re-Rating

`POST /v1/_admin/apps/{id}/invoice/rerate` with `period` and optional `pricing_override`
returns a preview diff (original vs rerated totals). Confirm with
`POST .../invoice/rerate/apply` using the returned preview token.

---

## 24. Future Considerations (Out of Scope for v2)

### 24.1 Distributed Metering (Multi-Node)

When Appbase scales beyond a single node:
- Each node maintains local atomic counters
- Periodic sync to central store (Redis or CockroachDB)
- Quota enforcement uses local counters (slightly stale) with periodic reconciliation
- Trade-off: up to `sync_interval` seconds of over-quota usage spread across nodes

### 24.2 Usage-Based Autoscaling

Auto-adjust plan limits based on patterns. Scale up during spikes, scale down during
quiet periods. Requires predictive modeling.

### 24.3 Cost Allocation Tags

Request-level labels (`team`, `environment`, `feature`) for internal chargeback:
```
X-Cost-Tags: team=backend,env=prod,feature=search
```

### 24.4 SLA Monitoring

Track uptime and latency SLAs per enterprise app. Auto-issue credits on SLA violations.

### 24.5 Marketplace Billing

Third-party plugin creators billing for their plugin usage through the platform.

### 24.6 Real-Time Usage Dashboard

WebSocket-based live usage dashboard for app developers, consuming the same atomic
counters used for enforcement.

### 24.7 Per-Endpoint Pricing

Extend per-endpoint metering (Section 21) with configurable per-endpoint pricing rules.

---

## 25. Glossary

| Term | Definition |
|---|---|
| **Acquire/Release** | Memory ordering guarantees for atomic operations; Acquire ensures visibility of prior Release writes across CPU cores |
| **Anniversary billing** | Billing period resets on the customer's signup date each month |
| **AtomicPtr** | Atomic pointer used for lock-free buffer swaps; requires epoch-based reclamation to prevent use-after-free |
| **Backfill** | Retroactive insertion of usage events for missed data |
| **Backpressure** | Flow control mechanism; when the event log channel is full, events are dropped rather than blocking enforcement |
| **CAS (Compare-And-Swap)** | Atomic read-modify-write operation; the foundation of lock-free token buckets and concurrency guards |
| **Commitment** | Minimum monthly spend guarantee for discounted rates |
| **Concurrency guard** | CAS-based RAII guard that tracks concurrent in-flight requests per app |
| **Counter floor** | Clamping a counter to zero when a correction would produce a negative value |
| **Credit (wallet)** | Prepaid monetary unit applied against usage charges before invoicing (FIFO order) |
| **crossbeam-epoch** | Rust crate for epoch-based memory reclamation; used to safely free old counter buffers after rollover |
| **Dead letter queue** | Storage for webhook deliveries that failed all retry attempts |
| **Dedup set** | Bounded LRU cache of idempotency keys (keyed by `{app_id}_{request_id}_{resource}`) |
| **Digest anchor** | SHA-256 digest stored externally for tamper detection |
| **Double buffer** | Two counter sets swapped atomically during rollover, with epoch-based draining of in-flight writers |
| **Dry-run mode** | Enforcement decisions are computed and logged but never applied; all requests allowed |
| **Dunning** | Payment failure recovery: grace period, retries, escalation |
| **Entitlement** | Boolean flag controlling access to a feature |
| **Epoch-based reclamation** | Memory management technique where freed objects are deferred until all threads from the prior epoch have completed |
| **Event log** | Append-only file of usage events for audit trail and billing replay |
| **Gauge** | Atomic counter for instantaneous values (e.g., concurrent connections) that can increment and decrement |
| **Graduated pricing** | Tiered pricing where each tier's rate applies only to usage within that tier's range |
| **HyperLogLog** | Probabilistic data structure for approximate distinct counting |
| **Idempotency key** | Unique identifier (`{app_id}_{request_id}_{resource}`) ensuring each event is counted exactly once |
| **Inline spend tracking** | Per-request spend estimation via atomic accumulator |
| **LL/SC** | Load-Linked/Store-Conditional; ARM's mechanism for atomic CAS operations |
| **Overage** | Usage beyond included amounts, billed per-unit |
| **Package** | Add-on bundle of quota/entitlements at fixed price |
| **Plan** | Named bundle of entitlements, quotas, rate limits, policies |
| **Plan version** | Monotonically increasing integer; apps pin to a version until explicitly migrated |
| **Policy** | Threshold-action pairs defining enforcement behavior |
| **Proration** | Partial-period billing adjustment when a plan changes mid-cycle |
| **Quota** | Numeric cap on accumulated resource usage within a time window |
| **Rate limit** | Throughput cap enforced via token bucket algorithm |
| **RBAC** | Role-Based Access Control; admin API keys have roles and scopes |
| **Re-rating** | Recalculating an invoice with updated pricing or corrected usage data |
| **Resource** | A measurable dimension of consumption (CPU, requests, storage, etc.) |
| **Scope** | RBAC permission unit (e.g., `read:usage`); follows `action:resource` pattern |
| **Shadow mode** | Current plan enforced normally while a second plan is evaluated for comparison |
| **Spending limit** | Per-app cap on total monetary cost per billing period; denominated in the app's currency |
| **Token bucket** | Rate limiting algorithm; packed into single AtomicU64 with CAS-based refill + consume |
| **TrialManager** | Dedicated background task (independent of PeriodRoller) that checks and enforces trial expiry |
| **ULID** | Universally Unique Lexicographically Sortable Identifier; used for event IDs |
| **WAL** | Write-Ahead Log; SQLite journaling mode for concurrent reads during writes |

---

## 26. References

- [Cloudflare Workers Pricing](https://developers.cloudflare.com/workers/platform/pricing/)
- [Cloudflare Workers Limits](https://developers.cloudflare.com/workers/platform/limits/)
- [Cloudflare Rate Limiting Rules](https://developers.cloudflare.com/waf/rate-limiting-rules/)
- [AWS Lambda Pricing](https://aws.amazon.com/lambda/pricing/)
- [AWS Savings Plans](https://aws.amazon.com/savingsplans/)
- [AWS CloudTrail Log File Integrity](https://docs.aws.amazon.com/awscloudtrail/latest/userguide/cloudtrail-log-file-validation-intro.html)
- [Vercel Spend Management](https://vercel.com/docs/spend-management)
- [Lago — Ingesting Usage Events](https://docs.getlago.com/guide/events/ingesting-usage)
- [Lago — Wallets & Prepaid Credits](https://docs.getlago.com/guide/wallet-and-prepaid-credits)
- [Lago — Graduated Charges](https://docs.getlago.com/guide/plans/charges/graduated)
- [Lago — Why Billing Systems Are a Nightmare](https://www.getlago.com/blog/why-billing-systems-are-a-nightmare-for-engineers)
- [Stripe — Usage-Based Billing](https://docs.stripe.com/billing/subscriptions/usage-based)
- [Stripe — Restricted API Keys](https://docs.stripe.com/keys#limit-access)
- [Stripe — Smart Retries (Dunning)](https://docs.stripe.com/billing/revenue-recovery/smart-retries)
- [Orb — Minimum Commitments](https://docs.withorb.com/guides/concepts/minimum-commitments)
- [Orb — Event Backfill](https://docs.withorb.com/guides/events-and-metrics/backfill)
- [Sigstore Rekor Transparency Log](https://docs.sigstore.dev/logging/overview/)
- [IETF — RateLimit Header Fields draft-10 (RateLimit + RateLimit-Policy)](https://datatracker.ietf.org/doc/draft-ietf-httpapi-ratelimit-headers/10/)
- [RFC 7231 Section 7.1.3 — Retry-After](https://www.rfc-editor.org/rfc/rfc7231#section-7.1.3)
- [crossbeam-epoch — Epoch-Based Memory Reclamation](https://docs.rs/crossbeam-epoch/)
- [ISO 4217 — Currency Codes](https://www.iso.org/iso-4217-currency-codes.html)
- [Rust Atomics and Locks (Mara Bos) — Chapter 3: Memory Ordering](https://marabos.nl/atomics/memory-ordering.html)
