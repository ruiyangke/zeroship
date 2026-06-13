# Plan: Billing & metering re-implementation — ISS-18 / ISS-31 (+ ISS-29/30)

**Status:** FINALIZED — decisions locked 2026-06-13 · **Tier:** T0 · **Date:** 2026-06-12 (design), 2026-06-13 (finalized)

---

## FINALIZED DESIGN (decisions locked 2026-06-13) — source of truth; supersedes the Open Questions below

The business model is **two independent revenue streams**, built as **two sequenced epics**.

### Stream 1 — Infra usage billing (THIS epic, PR1–6). "Infra cost amortized to creators."
- Creators pay the **platform** for the infrastructure their apps consume. This is the platform's cost-recovery revenue.
- **Tiered plans + CONFIGURABLE pricing policy** — a data-driven, operator-editable plan catalog (NOT hardcoded). Per tier: `base_fee`, `included_quota[metric]`, `overage_rate[metric]`, `spend_limit_default`.
- **Exceed the included quota → pay-as-you-go OVERAGE** (charges accrue; the app keeps running, NOT blocked). `charge = base_fee + Σ max(0, usage[m] − included[m]) × overage_rate[m]`.
- **Spend limit** (per-app configurable cap) drives enforcement, NOT the quota: **Warn** (~80%) → **Degrade** (soft cap = gateway throttle: tighten the app's concurrency + rate limit via `enforce.rs`, pushed by `ControlEvent::SpendState`; app stays up, cost accrual slows) → **Block** (hard cap = 402 before dispatch). Degrade is IN for v1.
- Tiers fall out naturally: **free tier** has `spend_limit ≈ base` ⇒ quota-capped by construction (abuse-bounded, **no card required** — cardless signup); **paid tiers** pay-go into overage up to their configured cap.
- **Stripe = billing rail, not pricing.** WE compute line items from our policy → Stripe **invoice items** on the platform's Customer → Stripe invoices/charges/dunning. No Stripe-side price objects, no price-sync.
- **Creator billing identity = a Stripe CUSTOMER** (object in the *platform's* Stripe account) **+ a saved PaymentMethod** via a Stripe-hosted setup flow (Checkout setup-mode / SetupIntent). **NOT a Stripe account.** A card is the gate to crossing the free quota (doubles as the upgrade prompt).
- **Metric set (v1):** the 5 platform counters (`requests, cpu_us, wall_us, egress_bytes, ingress_bytes`) + an open `env.meter` `custom: map<metric,u64>` for SDK-defined metrics. **Billing period:** calendar month (UTC).

### Stream 2 — Application fee on creator revenue (SEPARATE creator-payments epic, AFTER Stream 1).
- Creators charge **their** end-users via Stripe **Connect** (they connect their *own* Stripe via OAuth — **ISS-30, IN**, repurposed).
- The platform takes a **server-controlled, per-creator-configurable** application fee (**ISS-29 done right** — not the bypassable client-side default). Stamped server-side on the Connect charge; the creator's code cannot set or bypass it.
- **`FeePolicy`** (per creator, server-stored). Both models supported:
  ```rust
  enum FeePolicy {
      Fixed   { amount_cents: u64 },                                   // flat per-transaction
      Percent { percent: f64, cap_cents: Option<u64>, floor_cents: Option<u64> },
  }
  // fee = Fixed → amount_cents ;
  //       Percent → clamp(round(txn_cents × percent), floor.unwrap_or(0), cap.unwrap_or(MAX))
  ```
  Default for a new creator: `Percent { 15%, cap: none, floor: none }`. Extensible (a future `Fixed+Percent` hybrid is just another variant).
- `@zeroship/payments` is rewritten to use Connect + the server-stamped fee.

### Decisions resolved (these supersede "## Open questions" at the bottom)
1. **Price model** → tiered + configurable pricing policy; pay-go overage past quota.
2. **Stripe model** → compute-ourselves + invoice items (Stream 1); Connect + server-stamped fee (Stream 2).
3. **Degrade** → gateway throttle (concurrency + rate). In v1.
4. **Metrics** → 5 platform counters + open `env.meter` custom map (v1).
5. **Billing period** → calendar month (UTC).
6. **application_fee** → server-controlled, per-creator `FeePolicy {Fixed | Percent+cap+floor}`, default 15%. **NOT dropped** — the earlier draft's "drop the application_fee" is *reversed*: what was wrong was the *bypassable fixed client default*, not the fee itself.
7. **Onboarding** → infra billing: creator = Stripe Customer + card (no Stripe account); creator revenue: Connect (ISS-30).
8. **Sequencing** → Stream 1 (infra billing, PR1–6) FIRST; Stream 2 (Connect + fee) as its own epic after.

### Scope corrections vs the original draft below
- **ISS-29 + ISS-30 are NOT dropped** — they move to Stream 2 (the creator-payments epic), done right (server-controlled fee + real Connect onboarding). The body below that says "rewrite the SDK to remove the fee" / "drop application_fee" is **superseded** by this.
- **PR6 in the body = the Stream-1 reconciler** (usage → invoice items on the platform Customer). It does **not** stamp `application_fee` — that's Stream 2.
- **AGENTS.md is now STALE:** the "platform takes 15% / $100 → −$15 / platform only earns when creators earn" revenue model no longer describes the platform. The platform earns from (1) infra usage billing + (2) a configurable application fee. **Update AGENTS.md when this lands.**

---

## Problem — the metering/billing pipe is open with nothing feeding it

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
