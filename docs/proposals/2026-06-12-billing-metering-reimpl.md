# Plan: Billing & metering re-implementation — ISS-18 / ISS-31 (+ ISS-29/30)

**Status:** plan (for review) · **Tier:** T0 (business model non-functional) · **Date:** 2026-06-12

## Problem — "the platform takes 15%" cannot operate today

The metering/billing pipe is **open with nothing feeding it and nothing acting
on it**:

- **Producer:** `env.meter` is NOT registered (worker registers only
  db/kv/storage/auth). No worker emits a `UsageReport`. (ISS-18)
- **Transport/sink exists but is trivial:** `UsageReport { worker_id, counters:
  {app_id → AppUsage{requests,cpu_us,wall_us,egress_bytes,ingress_bytes}} }`
  (`crates/core/src/types.rs`) is POSTed to `control` `/internal/usage`
  (`internal.rs::report_usage`), which calls `registry.record_usage(app_id,
  resource, delta)` — a raw additive write. **No idempotency** (at-least-once
  retries double-count), **no aggregation / period rollover / pricing /
  spending-limit / billing**. `control/metering.rs` is a 1-line stub.
- **No enforcement:** nothing computes or enforces spending limits; an app can
  run unbounded. (ISS-31)
- **Plan is self-escalatable:** `plan_id` is free-text on the app, no server-side
  plan catalog or price model (CT-A1).
- **Fee not server-enforced:** the 15% is a client-side default in
  `@zeroship/payments` checkout, overridable/bypassable (ISS-29).
- **Stripe onboarding is a placeholder** (returns a hardcoded Express URL; no
  `account_links`) (ISS-30).

A complete-looking engine exists at **`crates/platform`** (~5,800 LOC) but it is
the **old monolithic tokio architecture** (its own control plane + axum HTTP
server + sqlx + reqwest + a v8 pool), workspace-`exclude`d, and **cannot run in
the zero-tokio compio stack**. Its *domain logic* is worth salvaging; its
*infrastructure* is superseded by the live `control`/`worker`/`gateway` crates.

## Strategy — port the domain logic into the live compio stack, then delete `crates/platform`

### Inventory: port vs discard
| `crates/platform` module | LOC | Disposition |
| --- | --- | --- |
| `metering/meter.rs` (atomic counters) | 343 | **Port** → worker producer + control aggregator (runtime-agnostic core; swap the tokio flush task for a compio task) |
| `metering/flusher.rs` (flush loop) | 67 | **Port** → compio interval task |
| `metering/rollover.rs` (period rollover) | 239 | **Port** → control aggregation |
| `metering/store/{memory,sqlite}.rs` | — | **Rewrite** → Postgres via `compio-postgres` (sqlite/memory were the monolith's stores) |
| `billing/pricing.rs` | 216 | **Port** (pure logic) → control pricing + a server-side **plan catalog** (fixes CT-A1) |
| `billing/spend_action.rs` (`SpendAction::{Allow,Warn,Degrade,Block}`) | 28 | **Port** (pure enum + thresholds) |
| `billing/reconciler.rs` (usage → invoice items) | 213 | **Port** → control reconciler (tokio→compio; reqwest→`cyper`; reuse the live `stripe_*`) |
| `enforcement/{quota,rate_limit,concurrency}.rs` | ~500 | **Reconcile** with the gateway's existing `enforce.rs` (rate-limit + concurrency already live there) — port only the **quota/spend** enforcement that's missing |
| `core/{types,meter_store,billing,event_log}.rs` | ~300 | **Port** the types/event-log shapes that survive |
| `control/` (sqlx_registry), `server/` (v8pool, middleware, HTTP) | ~2,000 | **Discard** — superseded by live `control`/`worker`/`gateway` |
| — final step — | | **Delete `crates/platform`** + drop it from `exclude` |

## Target architecture (across the live crates)

```
 Worker (V8)                         Control plane                    Stripe
 ───────────                         ─────────────                    ──────
 env.meter.* (MeterPlugin)  ──┐
 + auto counters (req/cpu/    │ batch  POST /internal/usage   ┌─ aggregate (period
   wall/egress/ingress)       ├──────► (idempotent: report_id │   rollover) → usage
 per-worker Meter (atomic)    │  every  + per-worker seq)     │   tables (PG)
 + compio flush task ─────────┘  ~10s                         ├─ price (plan catalog
                                                              │   + pricing.rs)
 Gateway / Worker  ◄── ControlEvent::SpendState(app, action) ┤─ spend-limit engine
   enforce SpendAction at the edge  (Allow/Warn/Degrade/Block)│   (SpendAction)
   (429/degrade over limit)                                   └─ reconciler (compio
                                                                  interval) ──► invoice
                                                                  items / metered subs
                                                                  + application_fee 15%
```

### 1. Producer — `env.meter` + the worker meter (ISS-18, CT-B2)
- Register **`MeterPlugin`** (`crates/plugin-meter`, new, mirroring plugin-kv) →
  `env.meter.increment(metric, n)` / counters, scoped per-`app_id` (the worker
  already knows the app). Native, small surface; the `@zeroship/*` packages wrap
  it. SDK-defined metrics + the platform auto-counters (requests/cpu/wall/egress/
  ingress, already conceptually in `AppUsage`) share one per-worker `Meter`.
- Port `metering/meter.rs`: a per-worker atomic-counter table keyed by
  `(app_id, metric)`; a **compio flush task** (port `flusher.rs`) drains a
  snapshot every ~10 s and POSTs a `UsageReport` to control.
- **Idempotency (CT-B2):** extend `UsageReport` with a `report_id` (uuidv7) + a
  monotonic per-`worker_id` `sequence`. Control dedups on `(worker_id, sequence)`
  so an at-least-once retry never double-counts. The flush snapshot is
  reset-after-ack (or carried forward on failure).
- Extend `AppUsage` to carry a `custom: map<metric,u64>` alongside the fixed
  platform counters, so SDK metrics flow without a wire change per metric.

### 2. Aggregation — `control/metering.rs` reimplemented
- Replace the stub + the raw `record_usage`: ingest `UsageReport`s idempotently,
  **aggregate per `(app_id, billing_period)`** with period rollover (port
  `rollover.rs`), persisted in Postgres via `compio-postgres`.
- Data model (new Liquibase changeset): `usage_aggregates(app_id, period_start,
  metric, total, updated_at)` + `usage_reports_seen(worker_id, sequence)` (dedup)
  + the plan catalog + spend-state tables below. Per-tenant RLS like the rest.

### 3. Pricing + plan catalog (ISS-31, CT-A1)
- **Server-side plan catalog** (`plans(plan_id, name, price_model_json,
  included_quotas_json, spend_limit_default)`), seeded by the control bootstrap.
  The app's `plan_id` becomes an FK into it — **no more free-text self-escalation**
  (an app can't pick an unpriced/oversized plan).
- Port `pricing.rs`: compute the period charge from `usage_aggregates × the plan's
  price model` (included quota + overage rates).

### 4. Spending-limit enforcement (ISS-31)
- Port `billing/spend_action.rs` (`SpendAction::{Allow,Warn,Degrade,Block}`) + the
  threshold logic. Control evaluates each app's period spend vs its limit on each
  aggregation tick and, on a state change, pushes
  **`ControlEvent::SpendState{app_id, action}`** through the existing
  worker/gateway event/route-pull channel.
- The **gateway/worker enforce at the edge**: `Block` → 402/429 before dispatch,
  `Degrade` → reduced concurrency/CPU limits, `Warn` → header/log only. Reuse the
  gateway's `enforce.rs` concurrency/rate-limit machinery; add the spend gate.

### 5. Reconciler + fee (ISS-31 reconciler, ISS-29 fee, ISS-30 onboarding)
- Port `reconciler.rs` as a **compio interval task** in control: at period close,
  reconcile `usage_aggregates` → Stripe **metered subscription usage records /
  invoice items** (via `cyper`, reusing `stripe_handlers`/`stripe_store`).
- **ISS-29 (fee):** move checkout-session creation server-side; control stamps
  `application_fee_percent` from a platform-held rate (the plan catalog), so the
  15% is server-enforced and unbypassable. Rewrite `@zeroship/payments` checkout
  to receive only a session URL (pre-launch — break the SDK).
- **ISS-30 (onboarding):** replace the placeholder with a real
  `/v1/account_links` Account-Link flow + verify the returning `acct_` belongs to
  the creator before binding payouts. (Adjacent; can be a parallel slice.)

## Correctness invariants
- **Zero tokio.** Every ported piece runs on compio: flush/reconciler =
  `compio::time` interval tasks; storage = `compio-postgres`; Stripe HTTP =
  `cyper`. `sqlx`/`reqwest`/`axum` from `crates/platform` are dropped.
- **At-least-once producer → idempotent aggregation** via `(worker_id, sequence)`
  dedup + `report_id`. A worker crash/restart resets its sequence from the last
  control-acked value (control returns the high-water mark on ingest).
- **Money is computed server-side only.** No client input sets price, fee, plan,
  or limit. Plan is an FK; fee is platform-held; usage is worker-attested +
  control-aggregated.
- **Period boundaries are explicit** (UTC month or the plan's cycle); rollover is
  atomic per app.

## Build sequence (PRs)
1. **Schema + types:** Liquibase changeset (usage_aggregates, dedup, plans,
   spend_state) + extend `UsageReport`/`AppUsage` (report_id, sequence, custom).
2. **Producer:** `crates/plugin-meter` (`env.meter`) + the worker per-worker Meter
   + compio flush task → POST /internal/usage. (ISS-18 producer)
3. **Aggregation:** reimplement `control/metering.rs` (idempotent ingest +
   rollover + Postgres). (ISS-18 aggregation)
4. **Plan catalog + pricing** (CT-A1, ISS-31 pricing).
5. **Spend-limit engine + edge enforcement** (`SpendAction` → `ControlEvent` →
   gateway/worker). (ISS-31 enforcement)
6. **Reconciler + server-side fee** (ISS-31 reconciler, ISS-29). [Stripe
   onboarding ISS-30 can land in parallel.]
7. **Delete `crates/platform`** + drop from `Cargo.toml` `exclude`; update
   `billing-metering.md` to "shipped".

## Testing (faithful, no shims)
- Unit: ported `pricing`/`spend_action`/`rollover` keep/port their existing
  `crates/platform` tests.
- Integration (ephemeral PG, like the e2e harnesses): the **full pipe** — an app
  calls `env.meter` + drives traffic → worker flushes → control aggregates
  (assert idempotent under duplicate reports) → price → cross a spend limit →
  assert `SpendState` reaches the gateway and an over-limit request is
  blocked/degraded → reconcile → assert Stripe invoice-item calls (mock Stripe).
- A `tests/e2e_metering_billing.sh` over the real multi-node stack.

## Open questions (operator)
1. **Price model granularity** — per-metric overage rates in the plan catalog, or
   a simpler tier model for v1? (Affects `pricing.rs` port scope.)
2. **Billing period** — calendar month vs Stripe subscription cycle vs per-app
   anchor?
3. **Degrade semantics** — what does `Degrade` do concretely (lower concurrency?
   CPU cap? cold-evict)? Needs a product decision.
4. **Stripe model** — metered subscriptions (usage records) vs monthly invoice
   items? (Reconciler shape differs.)
5. **Metrics set** — ship the 5 platform counters + open `env.meter` custom
   metrics in v1, or also the richer "25+ metrics" from the old drafts?
6. **ISS-30 onboarding** — fold into this epic or run as a parallel Stripe slice?
