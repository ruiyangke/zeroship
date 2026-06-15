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

> **All six resolved** by the FINALIZED DESIGN section at the top of this doc
> (2026-06-13). Retained for provenance only.

---

## Implementation plan: PR4–PR7 (build blueprint)

> Implements the FINALIZED DESIGN (locked 2026-06-13). PR1–PR3 already landed:
> `UsageReport{report_id,sequence,custom}` + `AppUsage.custom` (`crates/core/src/types.rs`),
> the `plugin-meter` producer + compio flush, and the idempotent aggregator
> (`crates/control/src/metering.rs` — `IngestLedger`, `Metering`, `period_start_unix`,
> `current_period_totals`) over `usage_aggregates`/`usage_reports_seen` (changeset 0037).
> Latest changeset = `0037`; new ones start at `0038`.
>
> **Status: HARDENED (critic→reviser, 3 rounds) — ready for PR4 implementation.**

### Cross-cutting decisions made here (sub-decisions the locked design left to the build)

- **D1 — Spend state is PULLed on `RouteEntry`, not pushed.** Live code has NO
  `ControlEvent` delivery path: `ControlEvent` (core) is defined but unconsumed;
  the gateway pulls `/internal/routes` every `poll_interval_secs`
  (`gateway/src/sync.rs::sync_once`) and the worker pulls `/internal/versions`.
  So the *faithful* mechanism is: control writes `spend_state` to a column, it
  rides on the pulled `RouteEntry`, and the gateway's `RouteCache::update`
  threads it onto `CompiledRoute`. We STILL add `ControlEvent::SpendState` to the
  wire enum (the design names it and it is the future push channel), and control
  constructs it on every transition for the audit log + a future SSE channel —
  but enforcement does not depend on push delivery. Steady-state latency = one
  poll interval (same as a plan change today), correct for a spend cap evaluated
  on a ~minute aggregation tick.
- **D2 — `plan_id` becomes a typed_id FK (`pln_<base62>`), free-text dropped.**
  Pre-launch: no alias. `AppRecord.plan_id`, `RouteEntry.plan_id`,
  `AppVersionInfo.plan_id` stay `String` but now hold a `pln_…` id that must
  exist in the catalog. `runtime_limits_for_plan` (registry.rs:490) is deleted;
  limits come from the catalog row.
  - **typed_id prefix is NOT yet registered.** `crates/core/src/typed_id.rs`
    declares prefixes as `pub const` string constants (`USER_PREFIX="usr"`,
    `APP_PREFIX="app"`, `WAKE_PREFIX="wak"`, `APP_OAUTH_CLIENT_PREFIX="oac"`,
    …) and the `all_prefixes`/`*_prefix_is_three_chars` tests enumerate them.
    PR4 **adds** `pub const PLAN_PREFIX: &str = "pln";` (3 chars, matching the
    R16-API2 convention) + a `new_plan_id()` helper (`generate(PLAN_PREFIX)`)
    and extends the `all_prefixes` test. The catalog `upsert` mints ids via
    `new_plan_id()`; built-in tiers seed fixed ids (`pln_free`, `pln_pro`,
    `pln_unlimited` — these are sentinel ids, exempt from base62 decode, matched
    verbatim — OR mint real ones and reference them from `bootstrap_builder`).
    Decision: **mint real `pln_<base62>` ids at seed time** and have the seeder
    return them so the console-app `set_plan` references the real id (no sentinel
    special-case in the parser).
- **D3 — money is integer cents, not millicents.** The ported `pricing.rs`
  (`crates/platform/src/billing/pricing.rs`) uses **millicents** throughout
  (`PricingRule::Flat{rate_millicents,per_units}`, `PricingTier.rate_millicents`,
  `cost()` returns `total_millicents`); the catalog and reconciler standardize on
  **cents** (Stripe's unit) with `u128` intermediate math, rounding half-up at the
  line-item boundary. The port renames every `*_millicents` field to `*_cents` and
  re-scales the seed rates (the platform seed table used e.g. `300 millicents /
  1_000_000 requests`; convert to cents at seed time, NOT in the hot path).
- **D4 — creator billing identity lives on a NEW `creator_billing` table** keyed
  by `creator_id`, NOT on `apps`. `creator_accounts` already holds the Stream-2
  Connect `acct_…` and is keyed `creator_id UUID PRIMARY KEY REFERENCES
  zeroship.users(id)` (changeset `0004`); so a `creator_id` **is a user id**. The
  Stream-1 platform Customer `cus_…` is distinct and gets its own
  `creator_billing` table (also keyed by the same `creator_id`/user id).
  - **Creator→app ownership join (H1 — the schema has NO `apps.creator_id`).**
    Changeset `0031` states it explicitly: *"This schema has no `apps.creator_id`
    column — the only data-level owner signal is `app_members` itself."*
    Ownership flows through `zeroship.app_members(app_id, user_id, role)` with
    `role='owner'`. Therefore the reconciler's "sum over the creator's apps" is:
    ```sql
    SELECT m.user_id AS creator_id, m.app_id
    FROM zeroship.app_members m
    WHERE m.role = 'owner'
    ```
    An app has **at most one** owner row (0031's backfill + `create_app` bind a
    single owner; PK is `(app_id, user_id)` but the `owner` role is singular by
    construction). The reconciler iterates owner rows, groups by `user_id`, and
    bills that `creator_id`. Apps with **no** owner row (e.g. the system console,
    `0036 apps.system=true`) are **skipped** (no billable creator). `creator_billing`
    rows are created lazily on first `billing/setup` for that `creator_id`.

---

### PR4 — configurable tiered pricing catalog

**Goal.** Replace free-text `plan_id` (CT-A1 self-escalation) with an
operator-editable, server-side plan catalog. Per tier: `base_fee_cents`,
`included_quota[metric]`, `overage_rate_cents[metric]` (per unit, with a
`per_units` divisor), `spend_limit_default_cents`. Compute a period charge from
`usage_aggregates`.

#### (a) Files to create/modify
- **NEW `crates/control/src/pricing.rs`** — ported, unit-pure, DB-free:
  - `PricingRule::{Flat{rate_cents,per_units}, Tiered{tiers:Vec<PricingTier>}}`
    and `PricingTier{up_to:Option<u64>, rate_cents, per_units}` — ported from
    `crates/platform/src/billing/pricing.rs` (millicents→cents, D3).
  - `struct PlanPrice { base_fee_cents: u64, included: HashMap<String,u64>,
    overage: HashMap<String,PricingRule>, spend_limit_default_cents: u64 }`.
  - `fn charge_cents(&PlanPrice, usage: &HashMap<String,i64>) -> ChargeBreakdown`
    implementing `charge = base + Σ max(0, usage[m] − included[m]) × rate[m]`.
    `ChargeBreakdown { base_cents, lines: Vec<LineItem{metric, billable_units,
    cents}>, total_cents }` — the reconciler (PR6) consumes `lines`.
  - Keep/port existing `pricing.rs` unit tests (unit converted); ADD
    `overage_only_charges_above_included` and
    `included_quota_fully_covers_usage_yields_base_only`.
- **NEW `crates/control/src/plan_catalog.rs`** — the PG-backed catalog:
  - `struct PlanCatalog { registry: Registry }`.
  - `struct Plan { id: String /* pln_… */, name: String, price: PlanPrice,
    runtime: AppRuntimeLimits, archived: bool }`.
  - `get/list/upsert/archive` (upsert/archive operator/master-key gated). JSON
    columns (`price_model_json`, `included_quota_json`, `runtime_limits_json`)
    deserialize into the pure types.
- **MODIFY `crates/control/src/registry.rs`**
  - DELETE `runtime_limits_for_plan` (~490–513). `get_versions` (~363–399) JOINs
    `plans` and builds `AppRuntimeLimits` from the plan row; missing plan ⇒
    conservative free-tier fallback (worker never gets `None,None,None`).
  - `create_app`/`set_plan` validate the id exists+unarchived (FK at DB; Rust path
    returns clean `InvalidInput` not a raw FK violation). NOTE `set_plan`
    (registry.rs:341) is today a bare `UPDATE zeroship.apps SET plan_id=$1` with NO
    validation — PR4 adds the catalog existence/archived check before the UPDATE.
    `set_plan` seeds the app's effective `spend_limit_cents` from
    `spend_limit_default_cents` if no explicit override (writes the
    `app_spend_state` row created in PR5; for PR4-alone the seed can be deferred to
    PR5's first `evaluate_all`).
  - DELETE the two doc-comment references to `runtime_limits_for_plan` in
    `registry.rs` callers (`bootstrap_console.rs:9–10,83–85`) so no dangling symbol
    reference remains after the fn is removed.
- **MODIFY `crates/control/src/bootstrap_console.rs`** — change `CONSOLE_PLAN_ID`
  (line 85) from `"enterprise"` to the seeded unlimited-tier `pln_…` id, and add a
  `seed_plans()` call ahead of the console-app upsert (see "Seeding + console-app
  FK ordering" below). Update the file's doc comment (lines 8–10, 83–85).
- **MODIFY `crates/control/src/api.rs`** + route registration: `GET /api/plans`
  (BillingRead), `GET /api/plans/:id`, `PUT/DELETE /api/plans/:id` (master-key /
  BillingWrite on Resource::Any). DELETE has no DB DELETE — it archives
  (`archived=true`) so existing `apps.plan_id` FKs + historical `billing_runs`
  stay resolvable.

#### (b) Schema changeset — `db/changelog/changesets/0038_plan_catalog.sql`
```sql
--liquibase formatted sql
--changeset zeroship:plan-catalog splitStatements:true
CREATE TABLE zeroship.plans (
    id                        TEXT        PRIMARY KEY,          -- pln_<base62>
    name                      TEXT        NOT NULL,
    base_fee_cents            BIGINT      NOT NULL DEFAULT 0,
    price_model_json          JSONB       NOT NULL,             -- {metric: PricingRule}
    included_quota_json       JSONB       NOT NULL DEFAULT '{}',-- {metric: u64}
    runtime_limits_json       JSONB       NOT NULL,             -- AppRuntimeLimits
    spend_limit_default_cents BIGINT      NOT NULL DEFAULT 0,
    archived                  BOOLEAN     NOT NULL DEFAULT false,
    created_at                TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at                TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
ALTER TABLE zeroship.apps
    ADD CONSTRAINT apps_plan_fk FOREIGN KEY (plan_id) REFERENCES zeroship.plans(id);
--rollback ALTER TABLE zeroship.apps DROP CONSTRAINT apps_plan_fk;
--rollback DROP TABLE zeroship.plans;

--changeset zeroship:plan-catalog-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.plans TO zeroship_control';
  END IF;
END $g$;
```
`plans` is a global (non-tenant) catalog — NO RLS (operator config; control is
BYPASSRLS). Grant `SELECT,INSERT,UPDATE` to `zeroship_control` (no DELETE —
archive, never hard-delete, so historical `billing_runs` keep a resolvable plan).

**Seeding + console-app FK ordering (verified against live bootstrap).** The
console app row is upserted by **`crates/control/src/bootstrap_console.rs`** (the
`INSERT INTO zeroship.apps … ON CONFLICT` at ~line 381), NOT `bootstrap_builder.rs`,
and it uses the free-text `CONSOLE_PLAN_ID = "enterprise"` const
(`bootstrap_console.rs:85`). Under the PR4 FK (`apps.plan_id → plans.id`) two
things MUST happen, in this order, BEFORE that upsert runs:
1. A `seed_plans()` step (new, called at the START of the control bootstrap, ahead
   of `bootstrap_console`) upserts the built-in tiers and returns their minted
   `pln_…` ids. Tiers: `free` (spend_limit ≈ base ⇒ quota-capped, no card),
   `pro`, `unlimited`/enterprise (no CPU/wall cap — what the console needs). The
   runtime_limits_json for each reproduces the matrix that
   `runtime_limits_for_plan` (registry.rs:490, deleted in PR4) hardcoded today
   (free=50ms/5s/64MB, pro=30s/30s/256MB, unlimited=None/None/None).
2. **`CONSOLE_PLAN_ID` changes from the free-text `"enterprise"` to the seeded
   `pln_…` id** of the unlimited tier (resolve it from `seed_plans()`'s return, or
   make the seed mint a deterministic id and reference it). Otherwise the console
   `INSERT` violates the new FK and bootstrap fails. This is a required edit in
   `bootstrap_console.rs` + its doc comment (lines 8–10, 83–85 reference the now
   deleted `runtime_limits_for_plan`).

The `seed_plans()` upsert is idempotent (ON CONFLICT (id) DO UPDATE), so a
re-bootstrap is a no-op.

#### (d) TDD test plan
- **Unit (DB-free):** ported pricing tests + two overage cases; breakdown sums to
  total. **Regression:** `overage_only_charges_above_included` (catches a port bug
  that prices from zero).
- **Integration (`CONTROL_TEST_DB` :5440):** `create_app_with_unknown_plan_id_is_rejected`
  (closes CT-A1), `set_plan_to_archived_plan_is_rejected`,
  `get_versions_derives_limits_from_catalog_not_hardcode`, `charge_from_real_aggregates`.

**Build/test gate (must be green to land PR4):** `cargo build -p zeroship-control
-p zeroship-core` + `cargo test -p zeroship-control` (catalog/pricing units +
integration; integration needs PG on :5440) + `cargo test -p zeroship-core`
(typed_id `all_prefixes` now includes `pln`). `clippy -p zeroship-control` clean.

---

### PR5 — spend engine + edge enforcement

**Goal.** Each tick: compute period spend vs `spend_limit_cents`; derive
`SpendState` (Warn ~80% → Degrade soft cap → Block hard cap); persist; surface on
the pulled `RouteEntry`; gateway throttles (Degrade) or 402s (Block) pre-dispatch.
Hysteresis on recovery.

#### (a) Files to create/modify
- **MODIFY `crates/core/src/types.rs`** — ADD `enum SpendState { Allow, Warn,
  Degrade, Block }` (snake_case, Default=Allow) — folds the ported `SpendAction`
  (delete it, no alias); ADD `ControlEvent::SpendState { app_id, state }`; ADD
  `#[serde(default)] RouteEntry.spend_state` (update the gateway/worker fixtures).
- **NEW `crates/control/src/spend.rs`** — PURE
  `derive_state(spend_cents: u64, limit_cents: u64, &SpendThresholds, prev: SpendState) -> SpendState`.
  `SpendThresholds { warn_pct: 80, degrade_pct: 95, block_pct: 100, deadband_pct: 5 }`.
  The state machine is defined on `pct = if limit==0 { u64::MAX } else { spend*100/limit }`
  (integer math; `limit==0` ⇒ a free/cardless plan ⇒ any spend is `pct=∞` ⇒ Block):
  - **Upward (entering a more-restrictive state) — at the threshold:**
    `pct >= block_pct` ⇒ Block; else `pct >= degrade_pct` ⇒ Degrade; else
    `pct >= warn_pct` ⇒ Warn; else Allow. Compute this as `raw_state`.
  - **Downward (relaxing) — only past the deadband, to avoid flapping:** never
    relax by more than one step per evaluation isn't required; what matters is the
    boundary. A transition to a LESS-restrictive state than `prev` is only
    permitted when `pct` has dropped below `(threshold_of(prev) - deadband_pct)`.
    Concretely: `Degrade→Warn` requires `pct < degrade_pct - 5 = 90`;
    `Warn→Allow` requires `pct < warn_pct - 5 = 75`; `Block→Degrade` requires
    `pct < block_pct - 5 = 95`. If `raw_state` is less restrictive than `prev` but
    the deadband condition is NOT met, **hold `prev`** (this is the anti-flap).
  - **Raised-limit immediate recovery (NOT subject to deadband).** The deadband
    guards against oscillation at a FIXED limit. When the *limit itself changes*
    (creator raises it, or new plan), the percentage drops by construction and the
    deadband would wrongly pin the app in Block/Degrade. So: `evaluate_all` passes
    the `limit_cents` used at `prev`'s computation (persisted alongside `state` in
    `app_spend_state`); if `limit_cents != prev_limit_cents` (a real limit change,
    not just accrual), `derive_state` **ignores the deadband for that tick** and
    returns `raw_state` directly. Result: raising the cap recovers immediately on
    the next ~60s tick; accrual oscillation near a fixed cap does not flap.
  - PG `SpendEngine { registry, catalog }`:
    - `evaluate_all() -> Vec<(app_id, old: SpendState, new: SpendState)>` — for
      each app: read `current_period_totals` (PR1–3 `Metering`), price via the
      plan's `PlanPrice::charge_cents` (PR4) to get `spend_cents`, resolve the
      effective `limit_cents` (`app_spend_state.spend_limit_cents` else the plan's
      `spend_limit_default_cents`), call `derive_state`, and on a transition
      UPDATE `app_spend_state` (state, spend_cents, the limit used) + INSERT a
      `spend_state_history` row. Returns only the apps that transitioned.
    - `set_limit(app_id, Option<u64>)` — upsert `app_spend_state.spend_limit_cents`
      (`None` clears the override → plan default). Used by the PR-A4 endpoint.
- **NEW `crates/control/src/cron/spend_reconcile.rs`** — compio interval (~60s):
  `evaluate_all` → audit + construct `ControlEvent::SpendState` per transition.
  Registered in `cron/mod.rs::spawn_all` via `compio::runtime::spawn(...).detach()`
  (the established pattern — `spawn_all` currently spawns `audit_retention::run`
  and `orphaned_app_reaper::run` this way; add a third spawn for
  `spend_reconcile::run(Arc::clone(&state), DEFAULT_TICK_SECS)`).
- **MODIFY `crates/control/src/api.rs`** + route registration — the creator-facing
  override endpoint (M4): `PUT /api/apps/:id/spend-limit` body
  `{ "cents": <u64|null> }` → authz `app_owner` on `Resource::App(id)` (the same
  app-membership gate `set_plan`/env endpoints use) → `SpendEngine::set_limit(app,
  Option<cents>)` (upserts `app_spend_state.spend_limit_cents`; `null` clears the
  override back to the plan default). `GET /api/apps/:id/spend-limit` returns the
  effective limit + current state. A creator CANNOT raise the limit beyond the
  plan's `spend_limit_default_cents` unless their plan permits it (validated
  against the catalog row); money stays server-bounded.
  - **The override is REDUCTION-ONLY by design.** It is bounded above by the
    plan default, so a creator can only LOWER their effective cap — never raise
    it above what the plan grants. Raising headroom = upgrading the plan (an
    operator/billing-gated action), not editing this override. There is no
    privilege-escalation path and deliberately no separate `spend_limit_max`
    column: the plan default IS the ceiling.
- **MODIFY `registry.rs::get_routes`** — JOIN `app_spend_state` onto
  `RouteEntry.spend_state`.
- **MODIFY `gateway/src/sync.rs`** — `RouteCache::update` threads `spend_state` onto
  `CompiledRoute`; on a per-app state flip it calls
  `state.rate_limiters.set_degraded(app, on)` and
  `state.concurrency.set_degraded(app, on)` (see the enforce.rs mechanism below).
- **MODIFY `gateway/src/enforce.rs`** — three changes, designed to FIT the live
  immutable-bucket registries (NOT the imagined mutable "divide the limit"):
  - `check_spend(state: SpendState) -> Result<(), HttpResponse>` — pure match:
    `Block` → `Err(402 SPEND_LIMIT)`, all others `Ok(())`.
  - **Degrade on `ConcurrencyRegistry`.** Today `ConcurrencyRegistry` holds a
    single global `limit: u32` and a per-app `gauges: HashMap<Uuid, AtomicU32>`
    (no per-app limit). Add `degraded: RwLock<HashSet<Uuid>>` +
    `set_degraded(app, on)`/`is_degraded(app)`. In `acquire_concurrency`, compute
    the effective ceiling as `if is_degraded(app) { (self.limit / DEGRADE_FACTOR).max(1) } else { self.limit }`
    and compare the gauge against THAT. No bucket rebuild — the existing CAS loop
    just reads a smaller ceiling. `DEGRADE_FACTOR: u32 = 8`.
  - **Degrade on `RateLimitRegistry`.** Today each app's `TokenBucket` is built
    once with immutable `capacity`/`refill_rate` (`TokenBucket::new(rate,burst)`),
    so we CANNOT mutate the bucket. Mechanism: add `degraded: RwLock<HashSet<Uuid>>`
    + `set_degraded(app, on)`. `check_rate_limit` calls `bucket.try_acquire()` as
    today when not degraded; when degraded it calls a new
    `bucket.try_acquire_n(DEGRADE_FACTOR)` — i.e. a degraded request consumes
    `DEGRADE_FACTOR` tokens instead of 1, which throttles effective throughput by
    `1/DEGRADE_FACTOR` against the SAME bucket without rebuilding it (the
    `1000`-scaled token math in `try_acquire` generalizes: consume
    `DEGRADE_FACTOR * 1000` tokens, require `>= DEGRADE_FACTOR*1000` available).
    `clear_degraded` removes the app from the set; the bucket then refills/serves
    at its normal rate immediately (no flap, recovery is instant).
  - **Per-rule limits (`PerRuleRateLimitRegistry`) are untouched** — they are
    creator-defined business limits keyed `(app_id, rule_idx, bucket)` and fire
    first in the request path; spend-degrade composes ON TOP (a Degraded app is
    additionally throttled by the global degraded bucket), which is the intended
    "tighten everything" semantics.
- **MODIFY `gateway/src/router/dispatch.rs`** — in `handle_dispatch` (line 1308,
  whose first body stmt is `enforce::check_rate_limit(&state.rate_limiters, app_id)`)
  AND `handle_subscription_dispatch` (line 1188, whose `check_rate_limit` is at
  line 1201), BEFORE `check_rate_limit`: `check_spend(route.spend_state)?`. Warn
  adds `x-zs-spend-warn: 1` header (pass); Degrade passes (the throttle is applied
  by the registries below, see H2 mechanism); Block 402s.
  - **In-flight requests on a flip to Block.** Block 402s only NEW requests
    (the gate runs at the top of `handle_dispatch`). Requests already past the
    gate hold a `ConcurrencyGuard` (RAII, released on drop) and run to completion
    — there is no mid-flight cancellation. This is intentional: a spend cap is a
    soft money bound on a ~minute tick, not a kill switch; bounding *new* work is
    sufficient and avoids tearing down live responses/streams.

#### (b) Schema changeset — `0039_spend_state.sql`
```sql
--changeset zeroship:app-spend-state splitStatements:true
CREATE TABLE zeroship.app_spend_state (
    app_id            UUID PRIMARY KEY REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    spend_limit_cents BIGINT,                    -- creator OVERRIDE; NULL = use plan default
    eval_limit_cents  BIGINT NOT NULL DEFAULT 0, -- EFFECTIVE limit used at last derive_state
                                                 -- (override else plan default); the
                                                 -- raised-limit-recovery comparison reads this
    state             TEXT NOT NULL DEFAULT 'allow',
    spend_cents       BIGINT NOT NULL DEFAULT 0,
    period_start      TIMESTAMPTZ NOT NULL,
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE TABLE zeroship.spend_state_history (
    app_id     UUID NOT NULL REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    from_state TEXT NOT NULL, to_state TEXT NOT NULL,
    spend_cents BIGINT NOT NULL, limit_cents BIGINT,
    at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
--rollback DROP TABLE zeroship.spend_state_history;
--rollback DROP TABLE zeroship.app_spend_state;

--changeset zeroship:app-spend-state-rls splitStatements:true
-- Verbatim 0037/0025 pattern: ENABLE + FORCE + a single tenant_isolation
-- USING policy on the app_id key bound to the zeroship.tenant_app GUC. A
-- request with the GUC unset reads `current_setting(...,true) => NULL`, so
-- `app_id = NULL` is NULL ⇒ no rows (fail-closed). Control is BYPASSRLS so the
-- engine still aggregates fleet-wide.
ALTER TABLE zeroship.app_spend_state    ENABLE ROW LEVEL SECURITY;
ALTER TABLE zeroship.app_spend_state    FORCE  ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON zeroship.app_spend_state
    USING (app_id = current_setting('zeroship.tenant_app', true)::uuid);
ALTER TABLE zeroship.spend_state_history ENABLE ROW LEVEL SECURITY;
ALTER TABLE zeroship.spend_state_history FORCE  ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON zeroship.spend_state_history
    USING (app_id = current_setting('zeroship.tenant_app', true)::uuid);
--rollback DROP POLICY IF EXISTS tenant_isolation ON zeroship.spend_state_history;
--rollback DROP POLICY IF EXISTS tenant_isolation ON zeroship.app_spend_state;

--changeset zeroship:app-spend-state-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.app_spend_state    TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT         ON zeroship.spend_state_history TO zeroship_control';
  END IF;
END $g$;
```
RLS on both tables (app_id-keyed, the 0037 fail-closed pattern reproduced
above); control is BYPASSRLS; least-priv grants to `zeroship_control`
(`app_spend_state` is mutated by the engine ⇒ `SELECT,INSERT,UPDATE`; history is
append-only ⇒ `SELECT,INSERT`).

#### (d) TDD test plan
- **Unit:** `derive_state` table-driven. **Regression:**
  `degrade_recovers_only_after_deadband` (no flap), `block_recovers_immediately_when_limit_raised`.
- **Integration (real PG):** `evaluate_all_persists_and_returns_transitions`.
- **Faithful e2e (gateway path, not a shim):** feed `RouteEntry{spend_state:Block}`
  via the REAL `RouteCache::update`, drive a request through the real
  `handle_dispatch`, assert **402** before any worker proxy
  (`over_limit_request_blocked_at_gateway` — fails today, passes after wire-in);
  `degraded_route_tightens_concurrency` (drive `DEGRADE_FACTOR+1` concurrent
  requests against a Degraded app whose normal limit would admit them all, assert
  the surplus is rejected — fails if `set_degraded` is a no-op);
  `degrade_clears_immediately_on_recovery` (flip Degrade→Allow via
  `RouteCache::update`, assert full throughput restored on the next request — the
  bucket is not rebuilt, so no warm-up).

**Build/test gate (must be green to land PR5):** `cargo build -p zeroship-core
-p zeroship-control -p zeroship-gateway` + `cargo test -p zeroship-gateway`
(enforce unit tests incl. the new degrade-token-cost cases + the faithful
dispatch e2e) + `cargo test -p zeroship-control` (spend engine + integration on
:5440) + `cargo test -p zeroship-core` (RouteEntry/ControlEvent fixtures). Update
the `crates/core/tests/types_test.rs` `ControlEvent`/`RouteEntry` round-trip
fixtures for the new `SpendState` variant + `spend_state` field. `clippy` clean.

---

### PR6 — reconciler + Stripe billing rail

**Goal.** End of calendar month (UTC): per creator, sum `usage_aggregates` across
their apps → `charge_cents` per app via the PR4 catalog → Stripe **invoice items**
on the platform's **Customer**. Idempotent per period. Card via Checkout
setup-mode. NO Connect / application_fee (Stream-2).

#### (a) Files to create/modify
- **NEW `crates/control/src/stripe_client.rs`** — thin `cyper`-based Stripe REST
  client. The `cyper::Client` + `compio::time::timeout` idiom is established in
  control today: GET in `api.rs::fetch_worker_logs:694` and **POST with a body +
  `content-type` header** in `bootstrap_builder.rs:263` and `oauth_handlers.rs:432`
  (`client.post(url)?.header("content-type", …)?.body(bytes).send()`). Stripe wants
  `application/x-www-form-urlencoded`, so set that content-type and form-encode the
  body bytes (no reqwest). Define a `trait StripeApi` (so tests inject a recording
  fake) implemented by the real `cyper` client. Headers: `Authorization: Bearer
  <STRIPE_SECRET_KEY>`, `Idempotency-Key` on every mutating call. Methods:
  `create_customer`, `create_checkout_setup_session` (mode=setup),
  `create_invoice_item`, `create_and_finalize_invoice`. Map non-2xx to a new
  `StripeError::Api{status,code}`. Base URL overridable (an `AppState.stripe_base_url`
  field defaulting to `https://api.stripe.com`) for the test mock.
- **NEW `crates/control/src/cron/billing_reconcile.rs`** — **NOT a port.**
  `crates/platform/src/billing/reconciler.rs` is the *spend-limit* reconciler
  (computes `SpendAction` from usage-vs-limit — that logic ports to PR5's
  `spend.rs`, see PR7 checklist); there is **no Stripe invoice-item code in
  `crates/platform`** (grep: invoice logic exists nowhere under
  `crates/platform/src`). So `billing_reconcile.rs` is **new code** built against
  the PR4 catalog + the new `stripe_client.rs`. It is a
  compio interval (~hourly): compute the closed (previous) calendar month
  (`period_start` via the same `period_start_unix` arithmetic as
  `control/metering.rs`, one month back). **Creator→app resolution (H1):** read
  ownership from `app_members WHERE role='owner'` (there is NO `apps.creator_id`),
  group `app_id`s by `user_id` ⇒ that user_id is the `creator_id`; skip apps with
  no owner row (e.g. the system console). Per creator: per-owned-app
  `Metering::period_totals(app_id, period_start)` → `charge_cents` (PR4 catalog) →
  invoice-item lines stamped with `metadata.creator_id` (so the existing webhook's
  `extract_creator_id` resolves it back). **Idempotency** (two layers, airtight
  under at-least-once):
  1. `billing_runs(creator_id, period_start)` PK + `INSERT … ON CONFLICT DO NOTHING`
     — claim the run BEFORE any Stripe call; `rows_affected()==0` ⇒ already billed
     this period ⇒ skip entirely (no Stripe call at all).
  2. Deterministic Stripe `Idempotency-Key = "billrun:{creator_id}:{period_start_unix}"`
     on the invoice-create call (and per-item keys
     `"billitem:{creator_id}:{app_id}:{period_start_unix}"`) — so EVEN IF the
     process crashes after the `billing_runs` INSERT commits but before Stripe
     responds, a retry replays the SAME key and Stripe returns the original object
     rather than creating a duplicate. Record `stripe_invoice_id`+amount with an
     `UPDATE billing_runs … WHERE creator_id=$ AND period_start=$` after the Stripe
     call succeeds (the row already exists from step 1).
  - **Crash-window note:** the only non-idempotent window is "INSERT committed,
    Stripe key never sent" — covered by layer 2's deterministic key on the next
    tick (the run row exists so step 1 says "billed", but a `stripe_invoice_id IS
    NULL` row is re-driven: the reconciler re-attempts the Stripe call with the
    same idempotency key for any `billing_runs` row whose `stripe_invoice_id` is
    still NULL, making the whole path replay-safe).
  Registered in `cron/mod.rs::spawn_all` via `compio::runtime::spawn(...).detach()`.
- **MODIFY `stripe_store.rs`** — `creator_billing` upserts: `get_customer`/`set_customer`.
- **MODIFY `stripe_handlers.rs`** — ADD `POST /api/creators/:id/billing/setup`
  (Stream-1: ensure a `cus_…` exists for the creator via `stripe_client`, persist
  it to `creator_billing`, return a Checkout setup-mode session URL). Extend the
  EXISTING webhook handler (do NOT add a second one) to also handle
  `setup_intent.succeeded` (sets `creator_billing.default_pm_set=true`) and
  `invoice.payment_failed` (audit + future dunning). **Webhook signature: reuse
  the existing `verify_stripe_signature` (`stripe_handlers.rs:236`, HMAC-SHA256
  over the `Stripe-Signature` header) — do NOT reinvent it**; the new event types
  ride the same already-verified ingest path (`extract_creator_id` at
  `stripe_handlers.rs:365` resolves `metadata.creator_id`). The infra-billing
  Connect `onboard`/`callback` placeholder (`stripe_handlers.rs:83`,
  `/api/creators/:id/stripe/onboard`) is **Stream-2 and left untouched** by this
  epic.
- **MODIFY `lib.rs` (`AppState`) + `main.rs`** — ADD `stripe_secret_key:
  SecretString` (+ optional `stripe_base_url` test override). Required in prod;
  empty only under `insecure_dev`.

#### (b) Schema changeset — `0040_creator_billing.sql`
```sql
--changeset zeroship:creator-billing splitStatements:true
CREATE TABLE zeroship.creator_billing (
    -- creator_id is a USER id (mirrors creator_accounts.creator_id, which is
    -- `REFERENCES zeroship.users(id)` in 0004). FK to users(id) so a deleted
    -- user's billing row cascades.
    creator_id         UUID PRIMARY KEY REFERENCES zeroship.users(id) ON DELETE CASCADE,
    stripe_customer_id TEXT,                 -- cus_… (platform account)
    default_pm_set     BOOLEAN NOT NULL DEFAULT false,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE TABLE zeroship.billing_runs (
    creator_id        UUID NOT NULL REFERENCES zeroship.users(id) ON DELETE CASCADE,
    period_start      TIMESTAMPTZ NOT NULL,  -- the billed month (UTC, first-of-month 00:00)
    amount_cents      BIGINT NOT NULL,
    stripe_invoice_id TEXT,                  -- NULL until the Stripe call succeeds
    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (creator_id, period_start)   -- the idempotency key (no double-bill)
);
--rollback DROP TABLE zeroship.billing_runs;
--rollback DROP TABLE zeroship.creator_billing;

--changeset zeroship:creator-billing-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.creator_billing TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.billing_runs    TO zeroship_control';
  END IF;
END $g$;
```
Grants `SELECT,INSERT,UPDATE` to `zeroship_control` (`billing_runs` needs UPDATE
for the `stripe_invoice_id`/amount write-back after the Stripe call). Control-only
bookkeeping, keyed by `creator_id` (a user id, not an app id) ⇒ **no per-tenant
app RLS** — mirrors `usage_reports_seen`/`creator_accounts`.

#### (d) TDD test plan
- **Unit:** NEW line-item tests (no port — there is no invoice logic in
  `crates/platform`): given a `PlanPrice` + a usage map, assert `charge_cents`
  produces the expected invoice-item lines; assert the deterministic
  `Idempotency-Key` strings (`billrun:`/`billitem:` formats) are stable for a fixed
  `(creator,app,period)`. Stripe calls go through an injected `StripeApi` trait
  (the `stripe_client` is the prod impl; a recording fake is the unit impl).
- **Integration (real PG + LOCAL mock-Stripe HTTP server — the REAL `cyper` client
  hits a localhost server speaking Stripe JSON, NOT a stubbed client):**
  `reconcile_creates_invoice_items_per_app_from_real_aggregates`;
  **`reconcile_is_idempotent_per_period`** (run twice ⇒ items created exactly once,
  second run no-op — fails if dedup guard missing → double-bill);
  `stripe_client_uses_cyper_and_sends_idempotency_key`; `setup_session_creates_customer_once`;
  `reconcile_groups_apps_by_owner_via_app_members` (two apps owned by the same
  user_id ⇒ one creator's invoice spans both apps — fails if the owner join is
  wrong); `crashed_run_with_null_invoice_id_is_redriven` (pre-insert a
  `billing_runs` row with `stripe_invoice_id IS NULL`, run reconcile, assert the
  Stripe call fires with the SAME deterministic idempotency key and the row is
  completed — covers the commit-then-crash window).

**Build/test gate (must be green to land PR6):** `cargo build -p zeroship-control`
+ `cargo test -p zeroship-control` (reconciler units + the real-`cyper`→localhost
mock-Stripe integration on :5440). The mock-Stripe server asserts it received a
`Stripe-Signature`-free outbound (we only SEND; signature is inbound) and a
non-empty `Idempotency-Key` on every mutating call. `clippy` clean.

---

### PR7 — delete `crates/platform`

**Goal.** Remove the dead tokio monolith + its workspace `exclude`.

- **Confirm-ported checklist (all true before deleting):** `billing/pricing.rs`
  (the `*_millicents` pricing types + `compute_cost`)→PR4 `pricing.rs` (re-scaled
  to cents, D3); `billing/spend_action.rs` (a re-export of `core/billing.rs::
  SpendAction`) + `core/billing.rs::SpendAction`→`core::types::SpendState`
  +`spend.rs::derive_state` (PR5); `billing/reconciler.rs` — note this is the
  **spend-limit** reconciler (`SpendingLimit`/`SpendAction`/`reconcile`),
  NOT a Stripe reconciler → its logic→PR5 `spend.rs`+`cron/spend_reconcile.rs`
  (the PR6 `cron/billing_reconcile.rs`+`stripe_client.rs` are NEW, not ports — no
  invoice code exists in `crates/platform`); `metering/{meter,flusher,
  rollover}.rs`→plugin-meter+control/metering (PR1–3); enforcement→gateway/enforce
  +PR5 spend gate; `core/{types,meter_store,event_log}`,`control/`,`server/`→discard
  (confirm `grep -r "zeroship_platform\|crates/platform"` returns only the
  `exclude` entry + this proposal).
- **DELETE** `crates/platform/`; **MODIFY root `Cargo.toml`** (remove `exclude`);
  **MODIFY `docs/reference/billing-metering.md`** → "shipped"; **MODIFY `AGENTS.md`**
  revenue model (replace the stale "platform takes 15% / $100 → −$15" block with
  "infra usage billing + configurable application fee (Stream 2)").
- **Test:** `cargo build --workspace` + `cargo test -p zeroship-control
  -p zeroship-gateway -p zeroship-core` green; `grep` gate (no `zeroship_platform`/
  `crates/platform` references remain outside git history).

---

### Ordering / dependencies
```
PR4 (catalog + pricing) ──┬─→ PR5 (spend engine: needs charge_cents + plan limits)
                          └─→ PR6 (reconciler: needs charge_cents + catalog)
PR5 ⟂ PR6 (independent; either order after PR4)
PR7 LAST (needs PR4+PR5+PR6 ported)
```
PR4 is the keystone. Changeset order: `0038`(PR4) → `0039`(PR5) → `0040`(PR6).
**Per-PR invariant audit:** zero tokio (compio intervals; `cyper` Stripe;
`compio-postgres`); typed_id `pln_…`; no back-compat alias (free-text `plan_id`,
`runtime_limits_for_plan`, platform `SpendAction` DELETED not aliased); money
server-side only; RLS fail-closed on app-keyed tables; least-priv grants.

---

## Billing v2 — metering-as-infra + compute-unit pricing

> **Status:** BLUEPRINT (design only). Reshapes the *shipped* PR1–PR6 pipeline
> (plugin-meter producer + `control/pricing.rs` per-metric overage) into two
> locked refactors. **Decisions A & B below are LOCKED — design to them.**
> Worktree `appbase-billing` @ `feat/billing-metering`. Latest changeset `0040`.
>
> **Refactor A — metering is INFRASTRUCTURE.** Delete the creator-facing
> `env.meter` API; emit raw metric events from the db/kv/storage native
> primitives at their op boundary (platform-measured, unforgeable). The worker
> keeps emitting the 5 platform counters.
>
> **Refactor B — compute-unit pricing.** Replace per-metric cents-rates with a
> global metric→weight cost model + per-plan `{included_units, fx, base_fee}`;
> accumulate integer `compute_units` (CU), convert to cents **once** at the end.
>
> **What does NOT change (read + confirmed against the live call sites):** the
> control aggregator (`metering.rs` — `IngestLedger`/`Metering`, idempotent
> `(worker_id,sequence)` dedup, `usage_aggregates` UPSERT, `period_totals`),
> the spend state machine (`spend.rs::derive_state` bands/deadband/raised-limit
> recovery), the gateway edge enforcement (`enforce.rs` degrade/block), and the
> Stripe rail (`stripe_client.rs`, `billing_reconcile.rs` two-layer idempotency,
> webhook). This is a **producer + pricing** reshape only. `usage_aggregates`
> stays **raw-metric-keyed** (`0037`: `PRIMARY KEY (app_id, period_start, metric)`)
> — verified; no schema change there, units are derived at pricing time.

> **Deliberate billing-semantics change (MINOR-3 — not a regression).** Refactor
> B replaces the original Stream-1 *per-metric* free quota `included_quota[metric]`
> with a **single global `included_units`** applied to the summed compute-unit
> total. This is intentional and follows directly from the CU model: once every
> metric is converted into a common unit (CU) and accumulated into one total
> before pricing, a per-metric quota no longer has a natural home — the included
> allowance is now "N free CU per period," not "N free ops of metric X." Plans
> that previously expressed `included_quota[requests]` etc. are re-expressed as a
> single `included_units` budget. Flagged here so a reader comparing against the
> earlier `included_quota[metric]` design does not mistake the drop for a lost
> feature.

### A0 — Why these shapes (cross-cutting)

- The CU is named **`compute_units` / CU**, never "token" — `token` collides
  with PAT/JWT/`token_id`/`token_handlers.rs` throughout the tree.
- The cost model (metric→weight) is **global** (one fleet-wide table), the price
  lever (`fx = cents_per_unit`) is **per-plan**. Separating "how much compute a
  byte costs" (engineering) from "how many cents a unit sells for" (business)
  is the whole point of Refactor B and kills the per-tier rounding error class.
- Spend cap stays **dollar/cents-denominated** (creator mental model): enforced
  as `total_units × fx` vs the cents cap — i.e. the existing
  `spend.rs::evaluate_all` keeps comparing `spend_cents` to `limit_cents`; only
  the way `spend_cents` is COMPUTED changes (CU×fx instead of Σ overage lines).

---

### A — Metering as infrastructure (producer reshape)

#### A1 — The `Meter`'s new home + injection mechanism (THE #1 UNKNOWN, resolved)

**Home: a new `crates/metering` crate.** Today the `Meter` + `flush` + the
`v8_class` increment surface all live in `crates/plugin-meter`. Refactor A
deletes the `env.meter` creator API but KEEPS the `Meter` and `flush` — and
those must now be a dependency of `plugin-db`/`plugin-kv`/`plugin-storage`
(producers) AND the worker (owner + flusher). A *plugin* crate cannot be the
shared home: `plugin-db` depending on `plugin-meter` (a sibling plugin) is a
layering inversion, and `plugin-meter` as a namespace plugin ceases to exist
once `env.meter` is deleted. So:

- **NEW `crates/metering`** — owns `Meter` (the atomic per-`(app_id,metric)`
  table, `increment`/`record_request`/`drain`/`merge`, `FIXED_METRICS`,
  `is_fixed_metric`), `SequenceSource`, `build_report`, and the compio
  `flush` task (`spawn_flush_task`, `FlushConfig`, `DEFAULT_FLUSH_INTERVAL`).
  These move **verbatim** from `plugin-meter/src/{meter,flush}.rs` (a `git mv`
  in spirit; the code is runtime-agnostic and already zero-tokio/`cyper`).
  Depends only on `zeroship-core` (for `AppUsage`/`UsageReport`) + `compio` +
  `cyper` + `uuid`. NO dependency on `zeroship-runtime` (it carries no V8).
- **DELETE `crates/plugin-meter` entirely** — `lib.rs` (`MeterPlugin`,
  `NativePlugin` impl, `namespace()=="meter"`), `v8_class.rs` (`MeterHandle`,
  `mint_meter`, the `#[v8_class] increment`), and the now-moved `meter.rs`/
  `flush.rs`. No `env.meter` namespace, no alias (pre-launch).
- **Why a crate, not `crates/core`:** `core` is inter-service wire types +
  typed_id + observability with a deliberately tiny dependency set; pulling
  `cyper` + a compio flush loop into it bloats a foundational crate every
  service links. A dedicated `crates/metering` keeps the flush/transport out
  of `core` while still being depended on by the three plugin crates + worker.
  (`AppUsage`/`UsageReport` stay in `core` — they are wire types the control
  plane also deserializes.)

**The injection vehicle: a `MeterHandle` value type (NOT the old v8_class).**
Reuse the name `MeterHandle` for a plain Rust struct in `crates/metering`:

```rust
// crates/metering/src/lib.rs  (design)
#[derive(Clone)]
pub struct MeterHandle { meter: Arc<Meter>, app_id: String }
impl MeterHandle {
    pub fn record(&self, metric: &str, n: u64) { self.meter.increment(&self.app_id, metric, n); }
}
```

It is the `Arc<Meter>` + the server-injected `app_id` bound together so a
producer emits without re-deriving the app or being able to meter another app
(the exact structural guarantee the deleted `env.meter` v8_class gave, now
applied to platform primitives instead of user code).

**How it threads in — the live registration path (traced, named):**

1. The worker owns the one process-wide `Arc<Meter>`. It is constructed in
   `crates/worker/src/main.rs` (~line 489–502, where `spawn_flush_task` is
   called today) and stored in the `METER` thread-local on every ntex worker
   thread via `cache::init_cache(max_size, KernelConfig{ meter, .. })`
   (`crates/worker/src/cache.rs:46,64,83`). **Unchanged** — `KernelConfig.meter`
   already carries it; `spawn_flush_task` now comes from `zeroship_metering`.
2. `create_plugins()` (`cache.rs:112`) is the SINGLE construction site for all
   plugins on an isolate. It already reads `METER.with(|m| m.borrow().clone())`
   to mint the (deleted) `MeterPlugin`. Refactor A re-points that `Arc<Meter>`:
   it is passed into the **constructors** of `DbPlugin`, `KvPlugin`,
   `StoragePlugin` instead. The `MeterPlugin` push (`cache.rs:135-140`) is
   deleted.
3. Each plugin's `build_instance(scope, app_id)` (runtime plugin trait,
   `crates/runtime/src/core/plugin.rs:76`) **already receives `app_id`** —
   resolved by the runtime from `SharedState.env_vars["APP_ID"]`
   (`plugin.rs:225-228`). At mint time the plugin combines its stored
   `Arc<Meter>` with that `app_id` into a `MeterHandle` and stamps it onto the
   v8_class instance (kv/db) or a thread-local (storage). **No new injection
   point is needed in the runtime** — the existing `build_instance(app_id)`
   surface is exactly the hook. This is the key finding: the plumbing already
   exists; only the *destination* of the `Arc<Meter>` moves from `MeterPlugin`
   to the three data plugins.

**Per-plugin wiring (matching each plugin's existing shape):**

- **plugin-kv** (`v8_class`-backed): `KvPlugin::with_backend(backend)` →
  `KvPlugin::with_backend_and_meter(backend, Arc<Meter>)`. `build_instance`
  (`lib.rs:78`) calls `mint_kv(scope, backend, meter, app_id)`; `mint_kv`
  (`v8_class.rs:364`) stamps a `MeterHandle` field onto the `Kv` struct
  (`v8_class.rs:46`, alongside `backend`/`app_id`). Each `dispatch_*` resolve
  arm emits (see A2).
- **plugin-db** (`v8_class` + 27 flat callbacks): `DbPlugin::new(url)` →
  `DbPlugin::new(url, Arc<Meter>)`. `build_instance` (`lib.rs:272`) →
  `mint_db(scope, app_id, meter)`. The emit lives at the shared exec boundary
  (`exec.rs`, see A2), so the `MeterHandle` is most naturally placed in the
  per-app `context` (`crates/plugin-db/src/context.rs`, the thread-local the
  exec layer already uses for schema/tx-client lookups keyed by `app_id`) —
  registered once per app in `DbPlugin::register`/first-touch, read by
  `exec_query`/`exec_mutation`. This keeps the exec functions' signatures
  untouched (they already take `app_id: &str`, enough to fetch the handle).
- **plugin-storage** (flat callbacks reading a `thread_local! STORAGE_BACKEND`):
  add a parallel `thread_local! STORAGE_METER: RefCell<Option<Arc<Meter>>>`,
  populated in `StoragePlugin::register` (`lib.rs:121-124`, beside the backend
  set) from a meter the plugin now stores. `StoragePlugin::with_backend(backend)`
  → `with_backend_and_meter(backend, Arc<Meter>)`. The callbacks
  (`callbacks.rs`) read it the same way they read the backend; `app_id` comes
  from `get_app_id(&state)` (`callbacks.rs:57`), the meter from the new
  thread-local, combined per-call.

#### A2 — Emit points + metric taxonomy (after the op succeeds)

Raw metric names (lowercase, snake; share the `usage_aggregates.metric TEXT`
row shape — no DDL per metric; they are NOT in `FIXED_METRICS`, so they flow
through `AppUsage.custom` (`core/types.rs:183`) transparently):

| Plugin | Emit site (live fn) | Metric(s) | Counts | Bytes known? |
| --- | --- | --- | --- | --- |
| db | `exec.rs::exec_query` (read path, `:121`) success | `db_reads` +1; `db_rows_read` += `rows.len()` | one query op; rows returned | rows in hand; bytes optional (skip v1) |
| db | `exec.rs::exec_mutation` / `exec_mutation_with_emit` (`:161`,`:288`) success | `db_writes` +1; `db_rows_written` += affected | one mutation; affected rows | affected-row Vec in hand |
| db | `exec.rs::exec_count` (`:135`) success | `db_reads` +1 | one count op | n/a |
| kv | `dispatch.rs` `spawn_kv_op!` `Ok(v)` arm for get/list (`:87`,`:220`) | `kv_reads` +1 | one read op | n/a v1 |
| kv | `Ok(v)` arm for set/delete/incr/setIfAbsent/expire/persist | `kv_writes` +1 | one write op | n/a v1 |
| storage | `callbacks.rs::put` `Ok(size)` arm (`:134`) | `storage_ops` +1; `storage_bytes` += `size` | one put; bytes written | `size` is the resolve value |
| storage | `callbacks.rs::get` `Ok(Some((bytes,meta)))` arm (`:177`) | `storage_ops` +1; `storage_egress_bytes` += `meta.size` | one get; bytes read | `meta.size` in hand |
| storage | `put_stream`/`get_stream` finalize | `storage_ops` +1; `storage_bytes` += final `size` | one streamed op | final `size` known at resolve |
| storage | `delete`/`list` `Ok` arm | `storage_ops` +1 | one op | n/a |

**Emit discipline (locked):** emit **only in the success (`Ok`) arm**, AFTER the
backend op returns — never on validation error, never before the op. A failed
op is not billable. The emit is a synchronous lock-free atomic bump
(`MeterHandle::record` → `Meter::increment`), so it adds no await and cannot
fail the op. The five platform counters (requests/cpu_us/wall_us/egress_bytes/
ingress_bytes) keep flowing via the worker's unchanged
`cache::record_request` (`cache.rs:159`, called from `handler.rs:278,363`).

**Taxonomy decision:** ship the op-count + bytes metrics above in v1
(`db_reads, db_writes, db_rows_read, db_rows_written, kv_reads, kv_writes,
storage_ops, storage_bytes, storage_egress_bytes`). `db_bytes`/`kv_bytes` are
deferred — neither exec nor the kv backend trait surfaces a wire-byte count at
the resolve boundary today, and adding it means threading a size through the
backend traits (out of scope; a metric with no weight is simply free).

#### A3 — `env.meter` removal + worker ownership (what survives vs dies)

- **Survives** (moves to `crates/metering`): `Meter`, `record_request`,
  `drain`, `merge`, `increment`, `SequenceSource`, `build_report`, `flush`
  (the whole compio flush task). The worker keeps owning the singleton
  (`KernelConfig.meter` + `METER` thread-local) and spawning the one flush task.
- **Dies** (deleted, no alias): the entire `env.meter` namespace —
  `MeterPlugin` (`NativePlugin` impl, `namespace()=="meter"`, `build_instance`
  minting the handle), the `MeterHandle` **v8_class** + `mint_meter` +
  `increment` `#[v8_method]` (`plugin-meter/src/v8_class.rs`), and the
  `MAX_INCREMENT`/`validate_metric`/`read_count` argument plumbing. The
  namespace registration in `create_plugins` (`cache.rs:135-140`) is removed,
  so `env` loses `meter` (one fewer `NativePlugin` in the vector; the
  `create_plugins_*` worker tests assert the surviving namespaces).
- **`Cargo.toml`:** add `crates/metering` to the workspace; `worker`,
  `plugin-db`, `plugin-kv`, `plugin-storage` depend on it; drop
  `plugin-meter` from the workspace + every dependent. `crates/cli` (the
  `zeroship serve` mirror of `create_plugins`) loses its `MeterPlugin` push too.

#### A4 — e2e probe re-point (drive primitives, not env.meter)

- **`examples/metering-probe/src/index.ts`** — remove
  `env.meter.increment("probe_hits")` (the API is gone). The handler instead
  drives a measurable db + kv + storage op per request (e.g.
  `await env.kv.incr("probe_hits")` then `await env.db.<coll>.insert({...})`
  then a small `env.storage.put(...)`), so the platform-emitted `kv_writes`/
  `db_writes`/`storage_ops` are the metrics the harness asserts on. The
  response still echoes a per-request shape for the smoke check (drop
  `meter_total_in_process`; the in-process value is no longer creator-visible).
  The probe needs a `default.schema` (one tiny collection) so `env.db` is
  installed — add it.
- **`tests/e2e_metering_billing.sh`** — Stage 2/3 assertions move from
  "custom metric `probe_hits` aggregated" to "`kv_writes`/`db_writes`/
  `storage_ops` aggregated ≥ N". The metering-test plan seed (`:165`) gains a
  `metric_weights` consistent with the new pricing (see B); the
  `price_model_json` shape changes (see B5). The probe app's plan must enable
  kv + storage backends in the worker (`--kv-url`, `--storage-url`) — the
  compose harness already wires db; confirm kv/storage are configured for the
  probe's worker or the namespaces are absent and the ops no-op.

---

### B — Compute-unit pricing (cost-model / price decoupling)

#### B1 — The new charge model

```text
total_units = Σ_m  usage[m] × weight[m]            (integer; weight via per_units divisor)
charge_cents = base_fee_cents + round_ONCE( max(0, total_units − included_units) × fx )
                                                   (fx = cents_per_unit)
```

- `weight[m]` is an integer "units per op" with a `per_units` divisor so it
  stays integer: a weight `{ units: 1, per_units: 1000 }` for `egress_bytes`
  means 1 CU per 1000 bytes. Accumulate `Σ floor-free` as an exact rational
  and only floor/round into integer CU at the boundary (reuse the existing
  `u128` + `div_round_half_up` + gcd/LCM machinery already in `pricing.rs`,
  now applied to UNITS not cents).
- Integer CU accumulate across all metrics into ONE total; cents conversion
  happens exactly **once** (`× fx`, one `round_half_up`). This is the rounding
  fix: the shipped `pricing.rs` rounds per-metric-line (one `rule_cost` per
  metric, summed) — billing-v2 rounds once over the summed CU.
- A metric absent from the weight table contributes **0 units** (free) —
  preserves the live "unknown metric is free, not an error" semantics
  (`pricing.rs::charge_cents` `unwrap_or(0)`).

#### B2 — `pricing.rs` reshape (types + signatures)

NEW shapes (replace `PricingRule`/`PricingTier`/`PlanPrice`/`LineItem` — pre-launch, DELETE the old ones, no alias):

```rust
// crates/control/src/pricing.rs  (design)
pub struct MetricWeight { pub units: u64, pub per_units: u64 }   // per_units>0; CU per per_units ops
pub type WeightTable = HashMap<String, MetricWeight>;            // GLOBAL cost model

pub struct PlanPrice {
    pub base_fee_cents: u64,
    pub included_units: u64,        // CU included before overage
    pub fx_cents_per_unit_milli: u64, // fx as milli-cents-per-CU (integer; see B-note)
    pub spend_limit_default_cents: u64,
}
pub struct ChargeBreakdown {
    pub base_cents: u64,
    pub total_units: u64,           // Σ usage×weight (audit)
    pub billable_units: u64,        // max(0, total_units − included)
    pub total_cents: u64,
}
pub fn total_units(weights: &WeightTable, usage: &HashMap<String,i64>) -> u64;
pub fn charge_cents(price: &PlanPrice, usage: &HashMap<String,i64>, weights: &WeightTable)
    -> ChargeBreakdown;
```

- **`fx` precision note:** `cents_per_unit` is often < 1 cent, so store `fx`
  as an integer milli-cent (or pico-cent) per CU and divide once at the end —
  same `div_round_half_up` boundary. Pick the scale so the cheapest realistic
  unit price (e.g. $0.30 / 1M requests with weight 1/req ⇒ 0.00003 ¢/CU) is
  representable. Recommend `fx_pico_cents_per_unit: u64` (10^-12 cent) — gives
  ample headroom and one clean `round_once`. (The exact scale is a B-impl
  detail; the blueprint locks "integer sub-cent fx, divide once".)
- `charge_cents` keeps its **call signature shape compatible** with both live
  callers by appending `weights`: `charge_cents(&plan.price, &usage, &weights)`.
  Both call sites read only `breakdown.total_cents` (confirmed:
  `spend.rs:293-294`, `billing_reconcile.rs:315-316`) — so they change by one
  argument and nothing else. The per-metric `LineItem` vec is GONE; the
  reconciler already bills **one invoice item per app = `total_cents`**
  (`billing_reconcile.rs:303,320`), never per-metric, so no invoice-shape change.
- Keep + adapt the unit tests; ADD regressions:
  `units_accumulate_then_round_to_cents_once`,
  `reweighting_history_changes_charge` (same raw usage + different weight table
  ⇒ different CU ⇒ proves auditability), `included_units_cover_usage_yields_base_only`,
  `unknown_metric_zero_weight_is_free`.

#### B3 — The weight-table home: a DB table `metric_weights` (recommended)

**Recommendation: a `metric_weights` DB table, NOT a config/seed const.** The
weight table is the global cost model — it must be operator-editable at runtime
(same governance as the plan catalog) and visible to BOTH the spend engine and
the reconciler (two crates/cron tasks). A compiled const would require a
redeploy to re-weight; a seed-only row could not be edited. A small table
mirrors `plans` exactly (global, non-tenant, no RLS, control BYPASSRLS):

```sql
-- 0041_metric_weights.sql (NEW changeset; see B5 on why new, not edit 0038)
CREATE TABLE zeroship.metric_weights (
    metric     TEXT PRIMARY KEY,
    units      BIGINT NOT NULL,          -- CU per per_units ops
    per_units  BIGINT NOT NULL CHECK (per_units > 0),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
-- GRANT SELECT,INSERT,UPDATE TO zeroship_control (DO-block guard, 0037 pattern)
```

- Loaded once per `evaluate_all` / per reconcile tick (cheap; a handful of
  rows) and passed as `&WeightTable` into `charge_cents`. A new
  `pricing_store.rs` (or a method on the existing `PlanCatalog`) reads it.
- A global FX default lives beside the weights (a singleton row in a tiny
  `pricing_config(fx_default, ...)` table, or a sentinel `metric=''` — prefer
  a 1-row `pricing_config` table for clarity); each plan's `fx` defaults from
  it at seed and may override per-plan.
- Operator endpoints: `GET/PUT /api/metric-weights` + `GET/PUT /api/pricing-config`
  (master-key/BillingWrite), symmetric with the shipped `/api/plans`.

#### B4 — `plan_catalog` + seed changes

- **`plan_catalog.rs`** `Plan { price: PlanPrice }` now serializes the new
  `PlanPrice` (`base_fee_cents`, `included_units`, `fx`, `spend_limit_default`).
  The JSONB columns change meaning: `price_model_json` no longer holds the
  per-metric `overage` map. Two options (B5 picks): either repurpose
  `price_model_json` to hold `{ included_units, fx }` or collapse to scalar
  columns. **Recommend scalar columns** `included_units BIGINT`,
  `fx_pico_cents_per_unit BIGINT` (drop `price_model_json`/`included_quota_json`
  JSONB) — the price model is now scalar, so JSONB earns nothing and a typed
  column is auditable + CHECK-constrainable. `from_row` / `upsert`
  (`plan_catalog.rs:111-160,194-208`) adapt.
- **`bootstrap_console.rs::builtin_plans`** (`:302`) re-expressed in CU:
  - `free`: `included_units` small/zero, `spend_limit_default 0` (quota-capped,
    no card) — unchanged intent.
  - `pro`: `base_fee 500`, a generous `included_units`, `fx` from the global
    default, `spend_limit_default 5_000`. The old per-metric `pro_overage`
    (`:334-337`) is deleted; the relative cost of requests-vs-cpu-vs-egress now
    lives in the **global weight table**, not the plan.
  - `unlimited`: `included_units 0`, `spend_limit_default 0` (uncapped),
    no caps. The seeder seeds `metric_weights` + `pricing_config` too (ahead of
    `bootstrap_console`, same ordering as `seed_plans`).

#### B5 — Changeset decision: **NEW `0041` + reshape `0038` in place** (split)

Pre-launch / unshipped, so back-compat is not a constraint — but the choice is
about migration *hygiene* for dev/test DBs that already ran `0038`:

- **`metric_weights` + `pricing_config`** → **NEW `0041_metric_weights.sql`**
  (+ `0042` if splitting config). They are new tables; a new changeset is the
  only correct shape.
- **The `plans` column reshape** (drop the `price_model_json`/`included_quota_json`
  JSONB; add `included_units`/`fx` scalar columns) → **edit `0038` IN PLACE.**
  Rationale: `0038` is unshipped (no production tenants — see top-of-doc
  pre-launch stance), Liquibase changesets are content-hashed so an in-place
  edit forces a clean re-migrate on dev/test DBs (drop volume + re-run, the
  established compose `migrate` flow), and leaving a vestigial JSONB column then
  ALTER-ing it away in `0041` is exactly the "ALTER existing tables" dead-code
  the pre-launch stance forbids. **So: reshape `0038`'s `CREATE TABLE plans`
  columns in place; add `0041` for the genuinely-new weight tables.** (If the
  operator prefers append-only changesets even pre-launch, the fallback is an
  `0041` that `ALTER TABLE plans DROP/ADD` — documented but NOT recommended.)
- `usage_aggregates` (`0037`) — **NO change.** Confirmed raw-metric-keyed;
  units are derived at pricing time, preserving re-weightability.

#### B6 — Charge call sites (both change by one argument)

- **`spend.rs::evaluate_all`** (`:233`, charge at `:293`): load the
  `&WeightTable` once before the per-app loop (alongside the batched fleet
  usage read at `:241-259`), pass it to `charge_cents(&plan.price, &usage,
  &weights)`. `spend_cents = breakdown.total_cents` (unchanged downstream:
  `derive_state`, deadband, persist — all untouched). The dollar cap comparison
  (`limit_cents`, `:297-303`) is unchanged — `total_units × fx` is already
  folded into `total_cents`.
- **`billing_reconcile.rs::reconcile`** (charge at `:315`): same one-arg change;
  load `&WeightTable` once per tick before the creator loop. `breakdown.total_cents`
  → one invoice item per app (`:320`), unchanged. The mock-Stripe integration
  + idempotency tests (`:519+`) adapt their `PlanPrice` fixtures to the new
  shape + pass a weight table.

---

### Sequencing (A vs B) + test plan

**Order: B (pricing) first, then A (producer).** They are code-disjoint —
**A** touches plugins/worker/runtime/metering + the example; **B** touches
control pricing/catalog/schema. But they share the PG `zeroship_billing_test`
(`:5440`) so their *integration* tests cannot run concurrently — sequence them.
B first because:

1. B is self-contained in `control` + one changeset; it can land + be green
   without touching the runtime. A's e2e (A4) asserts the *whole* pipe
   (primitive emit → aggregate → **price** → cap), so it wants B's CU pricing
   already in place to assert meaningful charges.
2. A is the larger blast radius (new crate, delete a crate, 3 plugin ctors,
   worker, CLI) — landing it on top of an already-correct pricing layer means
   the e2e re-point (A4) tests the final shape once, not twice.

(If parallelism is wanted, B's unit tests + A's unit/plugin tests are
DB-free and can run concurrently; only the two integration suites serialize on
`:5440`.)

**Per-refactor TDD (real path, no shims — `feedback_faithful_e2e_tests`):**

- **B — pricing (unit, DB-free):** RED→GREEN `units_accumulate_then_round_to_cents_once`
  (fails against the shipped per-metric-round `charge_cents`);
  `reweighting_history_changes_charge`; `unknown_metric_zero_weight_is_free`;
  `included_units_cover_usage_yields_base_only`.
- **B — catalog/engine (integration, real PG `:5440`):**
  `charge_from_real_aggregates_uses_weight_table` (seed `usage_aggregates` +
  `metric_weights` + a plan, assert `total_cents`); `evaluate_all` still
  transitions correctly under CU pricing (re-run the shipped spend-engine
  integration test against the new charge — it must stay green: proves the
  state machine is untouched); `reconcile_creates_one_invoice_item_per_app`
  re-run with CU pricing + mock-Stripe (proves the rail is untouched).
- **A — primitives (unit per plugin):** `kv_op_emits_kv_write_on_success`,
  `db_query_emits_db_read`, `storage_put_emits_storage_bytes_eq_size`, and the
  RED regression `failed_op_emits_nothing` (emit must be in the `Ok` arm only).
  Use a real `Meter` + assert `drain()` — not a mock.
- **A — worker (faithful):** `create_plugins` no longer registers `meter`;
  `env.meter` is `undefined` in an isolate (the deletion is observable);
  `create_plugins_threads_meter_into_db_kv_storage` (the three data plugins
  receive the shared `Arc<Meter>`).
- **A — e2e (`tests/e2e_metering_billing.sh`, real multi-node):** re-pointed
  probe drives real kv/db/storage ops → worker flush → control aggregate →
  assert `kv_writes`/`db_writes`/`storage_ops` in `usage_aggregates` →
  price via CU → cross a spend cap → assert gateway 402/degrade → reconcile →
  assert mock-Stripe invoice item. This is the single end-to-end proof that
  both refactors compose.

### The 3 riskiest design points

1. **`MeterHandle` reaches plugin-db's exec layer cleanly.** kv/storage stamp
   it onto a per-instance struct / thread-local trivially; db's emit lives at
   the shared `exec.rs` boundary which is reached by BOTH the v8_class and 27
   flat callbacks via `app_id`-keyed `context`. Putting the handle in
   `context` (not a new exec parameter) is the low-churn choice but must be
   verified to not collide with the tx-client/schema caches already there.
2. **`fx` sub-cent precision + single round.** Choosing the integer fx scale
   (pico-cents/CU) so the cheapest unit price is representable AND the
   `u128` CU×fx product can't overflow before the one `div_round_half_up`.
   Get this wrong and either cheap metrics round to free or huge usage saturates.
3. **The `0038` in-place reshape + dev-DB re-migrate.** Editing a content-hashed
   changeset forces every dev/test DB to drop-and-re-migrate; if any harness
   assumes an incremental `update`, it breaks. Mitigated by pre-launch (no prod
   data) but the compose `migrate` flow + every integration test's DB setup
   must tolerate the changed `0038` hash (fresh DB, not incremental).

### Metering hardening backlog (post-merge, not blockers)

Known gaps deliberately deferred — documented, not fixed in this epic. None
block merge; each is a clean future plug-in, not a rewrite.

- **(a) Crash-loss window.** Undrained in-memory deltas are lost if a worker
  dies between ~10s flushes (bounded by the flush interval + customer-favorable
  — we under-bill, never over-bill). Close with a worker-side durable checkpoint
  once a durability SLA actually exists.
- **(b) Hot-row UPSERT contention.** All workers UPSERT the same
  `(app_id, period_start, metric)` row in `usage_aggregates`. Move to
  per-worker-sharded append-only rows + `SUM`-on-read + period partitioning if
  contention shows up at fleet scale.
- **(c) Ledger retention/pruning.** `usage_reports_seen` and
  `spend_state_history` grow unbounded — and `usage_reports_seen` now also grows
  one identity per worker boot-epoch (after the FIX-1 boot-nonce identity). Needs
  a periodic prune past the dedup-relevant window.
- **(d) `get_stream` egress timing.** Storage egress is billed at stream-open
  (`storage_egress_bytes` currently counts "objects opened for read," not bytes
  actually delivered). Confirm that's intended, or bill incrementally in
  `read_chunk` against bytes streamed.
- **(e) db/kv byte metrics deferred.** db/kv byte-level metrics are weightless
  (0-weight in `metric_weights`). Confirm they can't accidentally bill (a
  non-zero weight slipping in would start charging for them silently).
- **(f) Adapter boundary at the flush drain.** Keep the flush drain a clean
  adapter boundary so external interop (CloudEvents→OpenMeter, OTLP→telemetry,
  Stripe `meter_events`) plugs in later without a rewrite.
- **(g) `MeterHandle` `String`→`Arc<str>` micro-opt.** Drop the per-op `app_id`
  heap clone by holding the id as `Arc<str>`.

---

## Holistic re-review + hardening (2026-06-13)

After billing-v2, the whole pipeline was re-reviewed **end-to-end** with four
parallel adversarial passes (a holistic re-review catches cross-cutting issues the
per-PR reviews structurally cannot — each per-PR review saw only one side of an
interaction):

| Lens | Score | Outcome |
| --- | --- | --- |
| Consistency / integration | 92 | metric names round-trip emit↔seed↔consume; no $0 leak |
| Money-flow numerics | 84 | two spend↔invoice **divergences** + a global-FX gap |
| Concurrency / failure | 84 | **two CRITICAL crash-window money bugs** |
| Security / trust boundaries | 91 | trust model sound; one underpay vector |

**Security posture confirmed sound:** `app_id` is server-injected at every emit
(no JS path); FX / weights / plan writes are provably operator-only (creators
can't reach `Resource::Any` via Cedar); the Stripe webhook is signature-verified
before any mutation; the six billing tables are RLS-fail-closed with
deny-by-absence grants.

### CRITICAL — fixed in `d7ea29b2`
- **C1 double-bill.** The per-app `billing_run_items` ledger row was written
  *after* `create_invoice_item`; a crash between the Stripe POST and the ledger
  commit, re-driven **>24h later** (Stripe Idempotency-Key expired), re-POSTed.
  **Fix:** claim-then-call — a durable intent row (`stripe_item_id = NULL`) is
  written *before* the Stripe POST; on re-drive a NULL-intent row triggers a Stripe
  metadata lookup (`find_invoice_item_by_key` via `metadata.zs_item_key`) to adopt
  the already-posted item rather than blindly re-POST. The >24h window is *closed*,
  not documented-around.
- **C2 under-bill.** `create_and_finalize_invoice` was non-atomic and the draft id
  was never persisted; a crash after create-draft (which sweeps the items) but
  before finalize, re-driven >24h later, created a new empty draft and finalized a
  $0 invoice. **Fix:** split into `create_invoice` + `finalize_invoice`; persist
  `billing_runs.draft_invoice_id` *before* finalize; on re-drive finalize the
  *existing* draft. Schema: `billing_runs.draft_invoice_id`,
  `billing_run_items.stripe_item_id` nullable.

### MAJOR — fixed in the fixer-B pass
- **Overflow-posture divergence.** `spend.rs` silently clamped `i64::MAX` while
  `billing_reconcile` hard-errors → an overflowing app was Blocked-via-clamp by
  enforcement but skipped (unbilled) by reconcile. Fix: spend skips+warns to match.
- **Catalog poison divergence.** Spend used `catalog.list()` (tolerant → app runs
  *uncapped*) while reconcile used `catalog.get()` (strict → that creator's bill
  errors). A corrupt `runtime_limits_json` left an app both uncapped *and* unbilled.
  Fix: both paths price off the scalar columns; poison in `runtime_limits_json`
  never blocks pricing.
- **Global FX had no near-zero floor** (per-plan `fx` did) → a `0`/near-zero global
  `pricing_config.fx` silently priced all overage to $0 platform-wide. Fix:
  `CHECK (fx >= MIN_FX)` + fail-closed in `pricing_store`.
- **`set_plan` underpay.** A creator (app_owner) could self-assign a cheaper
  operator plan. Fix: an `assignable_by_creator` flag on `plans` — creators
  self-select only among public tiers (free/pro); operator plans aren't
  creator-assignable. (Self-service preserved, with a guardrail; analogous to the
  reduction-only spend-limit override.)

### MINOR — fixer-B pass
Dead `db_rows_read` seed dropped; `u64→i64` ingest cast hardened to `try_from`+warn;
`period_start` bind converged to integer `TIMESTAMPTZ` across metering/spend;
`plan_catalog::upsert` now calls `PlanPrice::validate()` (no silent write-path clamp).

---

## Pluggable metering providers — design direction (confirmed 2026-06-13)

**Goal:** make the *billing-aggregation backend* swappable — **Native** (default),
**Stripe** (Billing Meters), **OpenMeter** — selected per deployment.

### The non-negotiable constraint
The **local meter stays the enforcement source of truth.** The gateway's
Warn→Degrade→Block loop needs the current-period CU aggregate *locally and within
~a minute*; no external provider can own that (rate-limited APIs, network hop). So
"pluggable metering" does **not** outsource metering — the local CU aggregate is
*always* maintained; the provider decides where usage *also* goes for billing.
OpenMeter/Stripe-Meters are **export sinks**, not replacements.

### The seam (CU is the contract)
Because the model already produces **compute units** as the single neutral
quantity, every provider just receives a CU number — weights + FX + enforcement
stay ours. Two traits draw the layers:

- **`UsageLedger`** (below) — the read contract over `usage_aggregates`
  (`period_totals(app, period) -> map<metric,u64>`). Metering = *fact*. An external
  meter could even back this.
- **`MeteringProvider` / `BillingProvider`** (above) — where CU usage goes for
  billing:
  ```rust
  trait MeteringProvider {
      async fn ensure_customer(creator) -> CustomerRef;
      async fn report_usage(customer, period, compute_units, idempotency_key);
      async fn invoice(customer, period) -> InvoiceRef;   // no-op if provider self-invoices
      async fn handle_webhook(payload, sig);
  }
  ```

### The providers
| Provider | CU handoff | Aggregates? | Invoices? |
| --- | --- | --- | --- |
| **Native** (default) | control aggregation → CU×FX → reconciler | ours | ours (Stripe invoice items) |
| **Stripe** (Billing Meters) | CU → `meter_events`; FX = a metered Price | Stripe | Stripe |
| **OpenMeter** | CU → CloudEvents | OpenMeter | stays Native / other rail |

### Decisions
- **Metering & billing are separated** at `usage_aggregates`: metering = fact
  (produce the ledger); billing = policy + money (price/enforce/invoice on top).
  Logical (crate/module) separation + the contracts; **physical/service** separation
  deferred (spend enforcement wants the ledger co-located).
- **Config = per-deployment** (`--metering-provider native|stripe|openmeter`).
- External clients are **zero-tokio cyper adapters** (no SDK that pulls tokio), like
  the Stripe client.
- **Second provider = OpenMeter** (confirmed) — self-hostable, no vendor lock,
  architecturally most different from Stripe (pure aggregation), so it exercises the
  abstraction rather than cloning Stripe.

### Build order (after the review fixes above land)
The abstraction **wraps the reconciler** that the C1/C2 + MAJOR fixes restructure,
so the implementation blueprint is written against the *corrected* code:
1. blueprint the `MeteringProvider`/`UsageLedger` seams (exact placement vs the
   reconciler; how `Native` retains enforcement; per-deployment wiring) — dual-reviewed;
2. implement `Native` (refactor current behind the trait) + `Stripe` (Billing
   Meters) + `OpenMeter`, TDD with a faithful mock per provider.

---

## Pluggable metering — implementation blueprint

> **Status:** BLUEPRINT (design only — no implementation code). Implements the
> "Pluggable metering providers — design direction (confirmed 2026-06-13)"
> section above, resolved against the CURRENT, just-hardened code
> (`d7ea29b2` C1/C2 + `d3a7d1ea` MAJOR/MINOR). Worktree `appbase-billing` @
> `feat/billing-metering`. Latest changeset `0042` (`0038`–`0042` already
> landed for plans/spend/billing/metric-weights/pricing-config). New changesets
> here start at `0043`.
>
> **The single load-bearing finding** (everything below follows from it): the
> reconciler ALREADY parameterises its Stripe surface behind the `StripeApi`
> trait and is ALREADY driven by a `tick_with<S: StripeApi>(state, &stripe,
> now)` seam (`billing_reconcile.rs:167`). The pluggable layer does NOT rewrite
> the reconciler — it lifts the *whole* `tick_with` body (group-by-owner →
> price → invoice) to become the **Native** provider's `invoice()`, and adds two
> sibling providers that differ only in *where CU goes*. The hardened C1/C2 +
> MAJOR crash-window logic moves verbatim inside `NativeProvider::invoice` (it is
> not re-derived).

### M0 — The two layers, and why CU is the only thing that crosses

`usage_aggregates` stays **raw-metric-keyed** (`0037`: `PK (app_id, period_start,
metric)`) — **no schema change, no provider touches it**. The neutral quantity
that crosses to a provider is **compute units (CU)**, and CU is *derived* from
the raw totals via the global weight table:

```text
raw usage_aggregates (per metric)  ──pricing::total_units(weights, usage)──►  CU
                                   ──pricing::charge_cents(price,usage,wt)──►  cents
```

Both `total_units` and `charge_cents` already exist and are pure
(`pricing.rs:253`, `:321`). So a provider that wants "CU" calls `total_units`;
a provider that wants "cents" calls `charge_cents`. **No CU column is added** —
re-weightability (the whole point of Refactor B) is preserved.

Two contracts split the layers (both new, both in control):

- **`UsageLedger`** — the READ contract over the local ledger (metering = fact):
  ```rust
  // crates/control/src/metering/ledger.rs (or a method-trait on Metering)
  trait UsageLedger {
      async fn period_totals(&self, app: Uuid, period_start: i64)
          -> Result<HashMap<String,i64>, RegistryError>;
  }
  ```
  This is `Metering::period_totals` (`metering.rs:284`) promoted to a trait so
  enforcement + every provider read the SAME local fact. **No behaviour change**
  — `Metering` impls it; the existing inherent method stays (or becomes the impl
  body). This is the seam an external meter could one day back; today only the
  PG impl exists.
- **`MeteringProvider`** — the WRITE/EXPORT + INVOICE contract (billing = policy):
  the four verbs below. The provider decides where CU *also* goes; it never owns
  the local ledger.

### M1 — The `MeteringProvider` trait and its value types

Home: **NEW `crates/control/src/metering/provider/mod.rs`** (a submodule tree
under the existing `metering` module — `mod.rs`, `native.rs`, `stripe_meters.rs`,
`openmeter.rs`, `types.rs`). Lives in `control` (not `core`): it is control-plane
policy that pulls `cyper` + the catalog + `stripe_client`, none of which belong in
the foundational `core` crate (same reasoning that kept `crates/metering` out of
`core`).

```rust
// crates/control/src/metering/provider/types.rs (design)
pub struct CustomerRef(pub String);          // Native/Stripe: "cus_…"; OpenMeter: the subject id
pub struct InvoiceRef(pub Option<String>);   // "in_…" for Native; None when the provider self-invoices
pub struct BillingPeriod { pub start: i64, pub end: i64 } // unix secs; == stripe_client::Period
pub struct CreatorBilling {                  // what ensure_customer needs/returns
    pub creator_id: Uuid,
    pub email: String,
    pub customer: Option<CustomerRef>,        // already-saved cus_… if any
}

// crates/control/src/metering/provider/mod.rs (design)
#[allow(async_fn_in_trait)]
pub trait MeteringProvider {
    /// Ensure the provider knows this creator (Native/Stripe: a cus_…;
    /// OpenMeter: a no-op, the subject is the app/creator id). Idempotent.
    async fn ensure_customer(&self, creator: &CreatorBilling)
        -> Result<CustomerRef, ProviderError>;

    /// Forward this period's CU for one app. `compute_units` is the integer CU
    /// from `pricing::total_units`. `idempotency_key` is the deterministic
    /// per-(app,period) key. Native: NO-OP (usage is already local). Stripe:
    /// POST /v1/billing/meter_events. OpenMeter: POST a CloudEvent.
    async fn report_usage(
        &self, customer: &CustomerRef, app_id: Uuid, period: BillingPeriod,
        compute_units: u64, idempotency_key: &str,
    ) -> Result<(), ProviderError>;

    /// Close + bill the period for one creator. Native: the WHOLE current
    /// reconciler body (price → invoice items → create+finalize, with C1/C2).
    /// Stripe: NO-OP (Stripe self-invoices from meter_events). OpenMeter: NO-OP
    /// (OpenMeter aggregates only — invoicing stays on the Native rail or
    /// another billing backend).
    async fn invoice(&self, creator: &CreatorBilling, period: BillingPeriod)
        -> Result<InvoiceRef, ProviderError>;

    /// Inbound webhook (signature-verify + mutate). Native/Stripe: the existing
    /// Stripe webhook path. OpenMeter: a no-op (no inbound billing events).
    async fn handle_webhook(&self, payload: &[u8], sig: &str)
        -> Result<(), ProviderError>;
}
```

**Relationship to `StripeApi` (explicit).** `StripeApi` is UNCHANGED — it stays
the low-level Stripe REST surface (`create_customer`, `create_invoice_item`,
`find_invoice_item_by_key`, `create_invoice`, `finalize_invoice`,
`create_checkout_setup_session`). `MeteringProvider` sits ABOVE it:
`NativeProvider` and `StripeMetersProvider` both *hold* a `StripeApi` and call
into it; `OpenMeterProvider` does not. `StripeApi` is "how to talk to Stripe";
`MeteringProvider` is "what the billing backend is". The new Stripe-Meters verb
(`meter_events`) is added as a NEW method on `StripeApi` (M3), not a new trait —
keeping one Stripe REST surface.

`ProviderError` wraps `StripeError` + a transport variant + a `Config` variant;
it maps into `RegistryError` at the cron boundary exactly as `StripeError` does
today.

### M2 — Native provider (default): wrap the current pipeline, zero behaviour change

`NativeProvider { stripe: StripeClient, store: StripeStore, registry: Registry }`
in `provider/native.rs`. It is the current code behind the trait:

- `ensure_customer` = today's `billing_setup` customer path
  (`stripe_handlers.rs:122` → `StripeApi::create_customer` + `StripeStore`
  upsert). Returns the `cus_…`.
- `report_usage` = **NO-OP** (`Ok(())`). Native enforcement + invoicing both read
  the local ledger; nothing to forward. (This is *the* concrete statement of
  "metering is never outsourced".)
- `invoice` = **the entire `bill_creator` body** (`billing_reconcile.rs:290`)
  moved verbatim — claim-then-call ledger (C1), create/finalize split (C2),
  deterministic idempotency keys, the `find_invoice_item_by_key` adoption path,
  the per-app pricing via `charge_cents`. **The hardened logic is not touched; it
  is relocated.** `sweep` (group-by-owner) stays in the cron and calls
  `provider.invoice(creator, period)` once per creator.
- `handle_webhook` = the existing `webhook` handler
  (`stripe_handlers.rs:497` — `verify_stripe_signature` + `setup_intent.succeeded`
  + `invoice.payment_failed`), unchanged.

**Refactor mechanics (low-risk):** `billing_reconcile.rs` keeps
`run`/`tick`/`tick_with`/`sweep`; `bill_creator` is moved into
`NativeProvider::invoice` and `sweep` calls `provider.invoice(...)` instead of
`bill_creator(...)`. Under `--metering-provider native` the call graph is
byte-identical to today; the integration tests (`reconcile_is_idempotent_per_period`,
`crashed_run_with_null_invoice_id_is_redriven`, the owner-grouping + mock-Stripe
suite) run UNCHANGED and are the regression gate proving no behaviour drift.

### M3 — Stripe (Billing Meters) provider

`StripeMetersProvider { stripe: StripeClient, store, registry }` in
`provider/stripe_meters.rs`. Stripe owns aggregation + invoicing; we only push CU.

- **NEW `StripeApi::create_meter_event`** (added to the existing trait + impl):
  ```rust
  async fn create_meter_event(
      &self, event_name: &str, stripe_customer_id: &str,
      value: u64, identifier: &str, timestamp: i64,
  ) -> Result<(), StripeError>;
  ```
  POSTs `POST /v1/billing/meter_events` (form-encoded, same `post_form` idiom):
  `event_name=<the Meter's event_name>`,
  `payload[stripe_customer_id]=cus_…`, `payload[value]=<CU>`,
  `identifier=<idempotency_key>` (Stripe dedupes on `identifier` within its
  window), `timestamp=<period end or now>`. This is a DISTINCT surface from
  `/v1/invoiceitems` — meter events feed a Stripe Meter, not an invoice item.
- **Mapping (CU/FX/customer → Stripe Meters concepts):**
  - **Customer** → the same platform `cus_…` (`ensure_customer` reuses the Native
    customer path).
  - **CU** → the meter-event `value`. One event per `(app, period)` carrying the
    period's total CU (or incrementally per export sweep — M4).
  - **FX** → a Stripe **metered Price** (unit_amount = FX per CU) on a Stripe
    **Meter** + a **Subscription** that ties the customer to that price. The
    Price/Meter/Subscription are OPERATOR-PROVISIONED once in the Stripe
    dashboard (or a one-shot setup) and their ids are config
    (`--stripe-meter-event-name`, `--stripe-meter-price-id`). We do NOT sync FX
    per-plan into Stripe in v1 — a single fleet meter+price is the v1 shape
    (per-plan Stripe prices is a documented follow-up). The plan's `fx` is
    therefore IGNORED on this rail (Stripe's price is the price); the local
    spend cap still uses our `fx` for enforcement (see M5).
- `report_usage` → `create_meter_event(event_name, cus, compute_units, idem_key, ts)`.
- `invoice` → **NO-OP** (`Ok(InvoiceRef(None))`). Stripe self-invoices from the
  subscription + pushed meter events on its own billing cycle.
- `handle_webhook` → reuse the existing verified webhook ingest; Stripe-Meters
  adds no new mutation in v1 (invoice.payment_failed still audited).

### M4 — OpenMeter provider

`OpenMeterProvider { client: OpenMeterClient, base_url, token }` in
`provider/openmeter.rs`. OpenMeter aggregates; it does NOT invoice.

- **NEW `OpenMeterClient`** — a `cyper`-based zero-tokio adapter, SAME shape as
  `StripeClient` (Bearer token, `compio::time::timeout`, base-url overridable for
  the mock). Behind a `trait OpenMeterApi { async fn ingest_event(&self, ev:
  &CloudEvent) -> Result<(), ProviderError>; }` for test injection.
- **CloudEvents shape** (OpenMeter's ingest is CloudEvents/JSON to
  `POST /api/v1/events`, `content-type: application/cloudevents+json`):
  ```json
  {
    "specversion": "1.0",
    "id": "<idempotency_key>",          // OpenMeter dedupes on id
    "source": "zeroship-control",
    "type": "compute_units",            // the OpenMeter meter's eventType
    "time": "<RFC3339 period end>",
    "subject": "<app_id or creator_id>",// the OpenMeter meter's subject
    "data": { "value": <CU>, "app_id": "...", "period_start": <unix> }
  }
  ```
- `ensure_customer` → no-op (OpenMeter has no customer object; the subject IS the
  identity). Returns `CustomerRef(app_id|creator_id)`.
- `report_usage` → `ingest_event(CloudEvent{ id: idem_key, subject, value: CU })`.
- `invoice` → **NO-OP** — invoicing stays on the Native/Stripe rail. v1 ships
  OpenMeter as an *export-only* sink (aggregate-elsewhere); if an operator runs
  OpenMeter they pair it with Native invoicing OR consume OpenMeter's aggregates
  out-of-band. (A future "OpenMeter→Stripe invoice" bridge is a follow-up, not v1.)
- `handle_webhook` → no-op.

### M5 — THE CRUX: where `report_usage` fires, and how enforcement stays Native

**Decision: a dedicated periodic EXPORT sweep, NOT the ingest tick and NOT the
flush boundary.** Rationale, resolved against the live code:

- The **flush boundary** is in the WORKER (`crates/metering/flush.rs`) — wrong
  layer: it has no provider config, no customer mapping, no catalog, and the
  worker must stay billing-agnostic. Rejected.
- The **ingest/aggregation tick** is `internal.rs::report_usage` → `Metering::ingest`
  — it runs per worker POST (~10s × N workers), is per-RAW-report (no
  period-total CU yet), and is on the hot ingest path. Forwarding here would push
  partial deltas at high frequency and couple ingest latency to an external API.
  Rejected.
- **A new `cron/metering_export.rs` sweep (~hourly, matching billing)** is the
  right boundary. It mirrors `spend_reconcile`/`billing_reconcile`: advisory-lock
  → for each `(creator, owned app)` → `total_units(weights, period_totals)` → CU
  → `provider.report_usage(cus, app, period, cu, idem_key)`. **For
  `--metering-provider native` this sweep is not even spawned** (report_usage is a
  no-op; spawning it would be pure waste). It is spawned ONLY for stripe/openmeter.

**Enforcement is PROVABLY unchanged across all providers.** `spend.rs::evaluate_all`
reads `usage_aggregates` (the batched fleet query at `:270`), prices via
`charge_cents`, derives `SpendState`, persists, and the gateway pulls it — and
**none of that calls a provider**. The provider abstraction is wired ONLY into
the two export/invoice cron tasks (`metering_export`, `billing_reconcile`), never
into `spend.rs` or `enforce.rs`. So Warn→Degrade→Block uses the local ledger
identically whether the provider is native, stripe, or openmeter. This is the
metering↔billing separation made concrete: **enforcement = local fact; provider =
export/invoice only.**

**Native vs Stripe-Meters cron wiring (the one subtlety):**

| Provider | `metering_export` cron | `billing_reconcile` cron | `spend_reconcile` cron |
| --- | --- | --- | --- |
| native | NOT spawned (no-op) | **spawned** → `NativeProvider::invoice` | spawned (unchanged) |
| stripe | **spawned** → meter_events | spawned but `invoice` = no-op (so it does nothing) | spawned (unchanged) |
| openmeter | **spawned** → CloudEvents | spawned, `invoice` = no-op | spawned (unchanged) |

`spawn_all` constructs the configured provider once (`Arc<dyn MeteringProvider>`),
threads it into the cron tasks, and skips spawning a cron whose provider verb is a
no-op (cheap match on the provider kind). `spend_reconcile` is provider-agnostic
and always spawned.

### M6 — Per-deployment wiring

- **CLI/env (`main.rs`):** add `--metering-provider <native|stripe|openmeter>`
  (env `METERING_PROVIDER`, default `native`). Provider creds:
  `--stripe-meter-event-name` + `--stripe-meter-price-id` (env
  `STRIPE_METER_EVENT_NAME`/`STRIPE_METER_PRICE_ID`) for stripe;
  `--openmeter-url` + `--openmeter-token` (env `OPENMETER_URL`/`OPENMETER_TOKEN`,
  `SecretString`) for openmeter. Parse into a `MeteringProviderConfig` enum.
- **`AppState`:** add `pub metering_provider: Arc<dyn MeteringProvider>` (built
  once at boot). The existing `stripe_secret_key`/`stripe_base_url` feed the
  Native + Stripe-Meters providers; new fields back the OpenMeter one.
- **Construction:** a `fn build_provider(cfg, state-deps) -> Arc<dyn
  MeteringProvider>` in `provider/mod.rs`. `spawn_all` reads
  `state.metering_provider` + the provider KIND to decide which crons to spawn
  (M5 table).
- **Prod-required validation (`main.rs` startup guard, beside the existing
  `stripe_secret_key` guard at `:633`):**
  - `native` / `stripe`: `stripe_secret_key` required (already enforced).
  - `stripe`: ALSO require `stripe-meter-event-name` + `stripe-meter-price-id`
    non-empty (else fail to boot — a Stripe-Meters deployment with no meter is a
    silent revenue black hole).
  - `openmeter`: require `openmeter-url` + `openmeter-token` non-empty.
  - All guards bypassable ONLY under `insecure_dev`, matching the existing
    pattern.

### M7 — Zero-tokio adapters

Both new clients mirror `StripeClient` exactly: `cyper::Client` + `compio::time::
timeout`, hand-rolled body encoding (form for Stripe meter_events,
`application/cloudevents+json` JSON for OpenMeter), Bearer auth, behind an
injectable trait (`StripeApi::create_meter_event` reuses the existing trait;
`OpenMeterApi` is the new sibling). NO SDK (every Stripe/OpenMeter Rust SDK pulls
tokio + reqwest — banned). Base URLs overridable for the localhost mocks.

### M8 — Test strategy (faithful, no shims — `feedback_faithful_e2e_tests`)

- **Per-provider localhost mock** (mirror the existing mock-Stripe HTTP server the
  reconciler integration tests already use): a mock that records inbound calls and
  speaks the provider's JSON. Stripe-Meters mock asserts `POST /v1/billing/
  meter_events` with the right `payload[value]`=CU + a stable `identifier`.
  OpenMeter mock asserts `POST /api/v1/events` with a CloudEvent whose `id`=idem
  key + `data.value`=CU. The REAL `cyper` client hits the mock (not a stubbed
  trait) for the integration leg.
- **RED→GREEN per provider:**
  - **Native unchanged (regression gate):** the EXISTING reconciler integration
    suite (`reconcile_is_idempotent_per_period`,
    `crashed_run_with_null_invoice_id_is_redriven`,
    `reconcile_groups_apps_by_owner_via_app_members`, the mock-Stripe invoice-item
    assertions) re-run against `NativeProvider::invoice` and must pass byte-for-byte
    — proving the lift introduced zero drift.
  - **Stripe:** `stripe_report_usage_pushes_cu_as_meter_event` (CU forwarded with
    correct value); `meter_event_identifier_is_idempotent` (re-run ⇒ same
    `identifier`, mock sees one logical event); `stripe_invoice_is_noop` (the
    billing cron does nothing under stripe).
  - **OpenMeter:** `openmeter_report_usage_emits_cloudevent_with_cu`;
    `cloudevent_id_is_idempotent`; `openmeter_invoice_is_noop`.
  - **Enforcement-invariant (the crux, per provider):**
    `spend_enforcement_reads_local_ledger_under_<provider>` — set provider to
    stripe/openmeter, seed `usage_aggregates`, run `evaluate_all`, assert the SAME
    `SpendState` transition as native (proves the provider never touched
    enforcement). This is the single test that nails "providers are export/invoice
    backends only".
- **e2e:** extend `tests/e2e_metering_billing.sh` with a `METERING_PROVIDER`
  matrix leg (native asserts a mock-Stripe invoice item as today; stripe asserts a
  mock meter_event; openmeter asserts a mock CloudEvent) — the same probe traffic,
  three export backends, one local ledger.

### M9 — Build sequence + new/modified/refactored files + risks

**Build order** (each step green before the next):

1. **Seams (no behaviour change):** add `UsageLedger` trait (impl by `Metering`);
   add `MeteringProvider` + `types.rs` + `ProviderError`. NEW files only;
   nothing wired yet.
2. **Native lift:** move `bill_creator` → `NativeProvider::invoice`; `sweep` calls
   `provider.invoice`. Re-run the full reconciler suite (regression gate). This is
   the riskiest step — gated entirely by the existing hardened tests.
3. **Config + wiring:** `MeteringProviderConfig`, CLI/env, `AppState.metering_provider`,
   `build_provider`, `spawn_all` provider-aware cron spawning, prod guards.
4. **Stripe-Meters:** `StripeApi::create_meter_event` + impl + mock; `StripeMetersProvider`;
   `cron/metering_export.rs`. TDD.
5. **OpenMeter:** `OpenMeterApi` + `OpenMeterClient` + mock; `OpenMeterProvider`. TDD.
6. **e2e matrix** + docs (`billing-metering.md` gains a "metering providers" section).

**New files:** `metering/provider/{mod,types,native,stripe_meters,openmeter}.rs`,
`metering/ledger.rs` (or trait in `metering.rs`), `openmeter_client.rs`,
`cron/metering_export.rs`, per-provider mock test modules.
**Modified:** `stripe_client.rs` (+`create_meter_event`), `lib.rs` (`AppState`),
`main.rs` (CLI/env/guards), `cron/mod.rs` (`spawn_all`), `cron/billing_reconcile.rs`
(`bill_creator`→`NativeProvider::invoice`).
**Refactored (no behaviour change):** `billing_reconcile.rs` body relocation;
`Metering::period_totals` behind `UsageLedger`.
**No changeset needed** in v1 (`usage_aggregates` unchanged; provider config is
CLI/env, not DB) — a `0043` only appears if an operator later wants per-deployment
provider config persisted, which v1 does NOT.

**The 3 riskiest points:**

1. **The Native lift must be zero-drift.** Moving the C1/C2 + MAJOR crash-window
   logic out of `bill_creator` into `NativeProvider::invoice` is a pure relocation
   — but a subtle change to the claim-then-call ordering or the
   `find_invoice_item_by_key` adoption path would silently reintroduce a
   double/under-bill. Mitigation: relocate verbatim, change ONLY the function
   boundary, and gate on the UNCHANGED hardened integration suite (the same tests
   that caught C1/C2). Do not "tidy" the body during the move.
2. **CU derivation parity between export and invoice.** Stripe-Meters' `report_usage`
   pushes `total_units(weights, totals)` while the LOCAL spend cap prices
   `charge_cents(...)` — both must read the SAME `weights` + the SAME period
   totals or a creator is enforced on one number and billed (by Stripe) on
   another. Mitigation: both load the weight table the same way per tick; the
   enforcement-invariant test asserts the local figure is provider-independent;
   document that on the Stripe rail the *invoice* number is Stripe's (FX = Stripe
   price), so operators must keep the Stripe price ≈ the plan FX or the two
   diverge by design.
3. **Provider-aware cron spawning correctness.** A misconfigured `spawn_all` that
   spawns `billing_reconcile` under `stripe` (where `invoice` is a no-op, fine) but
   FORGETS to spawn `metering_export` (where CU is pushed) yields a Stripe
   deployment that enforces locally but bills Stripe $0 — a silent revenue black
   hole. Mitigation: the M6 prod guard refuses to boot a stripe/openmeter
   deployment without its creds, and an integration test asserts the spawned-cron
   set per provider kind matches the M5 table.
