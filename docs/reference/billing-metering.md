# Billing And Metering

**Status: shipped (Stream 1 — infra usage billing).** The metering → aggregation
→ pricing → spend-enforcement → Stripe-invoicing pipeline is live across the
`metering`, `control`, and `gateway` crates. The old monolithic
`crates/platform` engine has been deleted; its salvageable domain logic was
ported into the live compio stack (billing PR1–7). Stream 2 (application fee on
creator revenue via Stripe Connect) is a separate upcoming epic — see "Revenue
model" in `AGENTS.md`.

## Producer — metering is infrastructure (no `env.meter`)

**There is no creator-facing `env.meter` API.** The billing signal is
platform-measured so app code can neither forge nor suppress it. Two producers
feed one process-wide `Meter`:

1. **The worker** emits the five fixed platform counters (`requests`, `cpu_us`,
   `wall_us`, `egress_bytes`, `ingress_bytes`) once per dispatched request via
   `cache::record_request` → `Meter::record_request`.
2. **The trusted data primitives** (`env.db`, `env.kv`, `env.storage`) emit raw
   usage metrics at their op boundary, **in the success arm only** (a failed op
   is not billable), through a `MeterHandle` bound to the isolate's
   server-injected `app_id`:
   - `plugin-db` ([exec.rs](../../crates/plugin-db/src/exec.rs)): `db_reads`
     (query/count), `db_writes` + `db_rows_written` (mutations).
   - `plugin-kv` ([dispatch.rs](../../crates/plugin-kv/src/dispatch.rs)):
     `kv_reads` (get/list/ttl), `kv_writes` (set/delete/incr/setIfAbsent/
     expire/persist).
   - `plugin-storage` ([callbacks.rs](../../crates/plugin-storage/src/callbacks.rs)):
     `storage_ops`, `storage_bytes` (put), `storage_egress_bytes` (get).

The `Meter` (per-`(app_id, metric)` atomics; the five fixed counters as
dedicated atomics, everything else in an open `custom` map) and the
`MeterHandle` injection vehicle live in `crates/metering`
([meter.rs](../../crates/metering/src/meter.rs),
[lib.rs](../../crates/metering/src/lib.rs)) — a V8-free crate the worker owns
and the three data plugins depend on.

A compio flush task
([crates/metering/src/flush.rs](../../crates/metering/src/flush.rs))
drains the meter every ~10s and POSTs a `UsageReport` to control's
`/internal/usage`. `Meter::drain` snapshots AND zeroes; on POST failure the
snapshot is merged back, so no counts are lost (zero tokio — `compio` interval +
spawn).

## Usage reporting wire type

`UsageReport` ([crates/core/src/types.rs](../../crates/core/src/types.rs))
carries `{ worker_id, report_id, sequence, counters }` where `counters` is
per-app `AppUsage` plus the open `custom` metric map. The dedup key is
`(worker_id, sequence)` — a monotonic per-worker counter — so the at-least-once
flush retries cannot double-count. `report_id` is a per-report uuidv7 for logs.

## Ingest + aggregation (control)

[crates/control/src/internal.rs](../../crates/control/src/internal.rs) receives
the report; [crates/control/src/metering.rs](../../crates/control/src/metering.rs)
ingests it **idempotently** (dedup on `(worker_id, sequence)` via
`zeroship.usage_reports_seen`) and aggregates per `(app_id, period_start,
metric)` into `zeroship.usage_aggregates`. `period_start` is the calendar-month
boundary (00:00:00 UTC on the 1st), so month rollover lands in a new row
automatically. Dedup + apply run in one transaction per report (Postgres via
`compio-postgres`).

## Pricing + plan catalog

The plan catalog is data-driven and operator-editable
([crates/control/src/plan_catalog.rs](../../crates/control/src/plan_catalog.rs)),
not hardcoded `plan_id` free-text. Per tier: `base_fee_cents`,
`included_quota[metric]`, `overage_rate[metric]`, `spend_limit_default_cents`.
Pure tier math (`PlanPrice`, `rule_cost`, `ChargeBreakdown`) lives in
[crates/control/src/pricing.rs](../../crates/control/src/pricing.rs); all money
is integer cents with `u128` intermediates and half-up rounding at the line-item
boundary. Charge for a period:
`base_fee + Σ max(0, usage[m] − included[m]) × overage_rate[m]`.

## Spend-limit enforcement

A per-app configurable spend limit (not the quota) drives enforcement.
[crates/control/src/spend.rs](../../crates/control/src/spend.rs) derives a
`SpendState` ([crates/core/src/types.rs](../../crates/core/src/types.rs)) from
period spend vs the effective limit: **Allow** → **Warn** (~80%) → **Degrade**
(soft cap) → **Block** (hard cap). The `cron/spend_reconcile.rs` task recomputes
state each tick and emits `ControlEvent::SpendState`. The gateway enforces at the
edge ([crates/gateway/src/enforce.rs](../../crates/gateway/src/enforce.rs)):
`Block` → 402 `SPEND_LIMIT` before dispatch; `Degrade` → tighter concurrency +
rate-limit throttle (the app stays up, accrual slows); `Warn` stamps a header.

## Stripe invoicing (Stream 1)

Stripe is the billing rail, not the pricing engine: WE compute line items, then
push them as Stripe **invoice items** on the platform's own Customer. The
creator's billing identity is a Stripe **Customer** (`cus_…`) in the platform's
Stripe account plus a saved PaymentMethod (Checkout setup-mode / SetupIntent) —
**not** a Stripe account, **no** Connect, **no** `application_fee` (that is
Stream 2).

- Thin `cyper`-based REST client (zero tokio):
  [crates/control/src/stripe_client.rs](../../crates/control/src/stripe_client.rs)
  — outbound-only, every mutating call carries an `Idempotency-Key`.
- Month-close reconciler:
  [crates/control/src/cron/billing_reconcile.rs](../../crates/control/src/cron/billing_reconcile.rs)
  sums each creator's owned-app usage for the closed calendar month, prices it
  via the plan catalog into invoice-item lines, and finalizes an invoice.
- Webhook signature verification (inbound) is in
  [crates/control/src/stripe_handlers.rs](../../crates/control/src/stripe_handlers.rs),
  with persistence in
  [crates/control/src/stripe_store.rs](../../crates/control/src/stripe_store.rs),
  mirrored by [sdks/payments/src/webhook.ts](../../sdks/payments/src/webhook.ts).

## Stripe Billing Meters export (M-Stripe)

An alternative rail (`--metering-provider stripe`) where Stripe owns aggregation
+ invoicing and the platform only PUSHES compute units. The export cron
([crates/control/src/cron/metering_export.rs](../../crates/control/src/cron/metering_export.rs))
sweeps each owned app's current-period CU and forwards a delta as Stripe
`meter_events` against an operator-provisioned Meter (`--stripe-meter-event-name`
+ `--stripe-meter-id`). Stripe's metered Price + Subscription self-invoice from
those events; the platform never invoices on this rail.

Exactly-once revenue under crash × >24h re-drive × multi-instance:

- **Billable CU parity.** The export pushes BILLABLE CU
  (`total_units − plan.included_units`) — the SAME quantity the spend cap /
  `charge_cents` treat as billable — so Stripe billing == local enforcement. The
  plan's `base_fee_cents` is a SEPARATE Stripe subscription line, not part of the
  metered usage.
- **Consumption-instant timestamp.** Each `meter_event` is stamped at the sweep's
  wall-clock instant (Stripe accepts only `[now−35d, now+5min]`), never at the
  period end (a future timestamp Stripe would reject — a $0-revenue black hole).
- **Aggregate reconcile (no 24h-window trust).** Before pushing, the cron reads
  the meter's *aggregated* value back
  (`GET /v1/billing/meters/{id}/event_summaries`) and pushes
  `current − max(local_high_water, stripe_aggregate)`. A crash-then-re-drive past
  Stripe's ~24h `identifier` dedup window therefore re-pushes only the missing
  remainder — the guarantee never depends on the dedup window.
- **Durable failure surface.** A failing export bumps `consecutive_failures` +
  records `last_error`/`last_attempt_at` on `metering_exports` (reset on success),
  so a permanently mis-provisioned app is observable, not silently under-billing.
- **Append-only — no clawback.** Stripe meters are additive: once CU is exported
  it is never refunded. A DOWNWARD re-weight mid-period does NOT claw back already
  exported CU; treat metric weights as append-only within a billing period.
