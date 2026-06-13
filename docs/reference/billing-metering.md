# Billing And Metering

**Status: shipped (Stream 1 — infra usage billing).** The metering → aggregation
→ pricing → spend-enforcement → Stripe-invoicing pipeline is live across the
`plugin-meter`, `control`, and `gateway` crates. The old monolithic
`crates/platform` engine has been deleted; its salvageable domain logic was
ported into the live compio stack (billing PR1–7). Stream 2 (application fee on
creator revenue via Stripe Connect) is a separate upcoming epic — see "Revenue
model" in `AGENTS.md`.

## Producer — `env.meter` + the per-worker meter

The `env.meter` namespace is registered by `MeterPlugin`
([crates/plugin-meter/src/lib.rs](../../crates/plugin-meter/src/lib.rs)),
mirroring `plugin-kv`. App code calls `env.meter.increment(metric, n?)`, a
synchronous atomic bump (not a backend round trip). The five fixed platform
counters (`requests`, `cpu_us`, `wall_us`, `egress_bytes`, `ingress_bytes`) and
SDK-defined `custom` metrics share one per-`(app_id, metric)` atomic counter
([crates/plugin-meter/src/meter.rs](../../crates/plugin-meter/src/meter.rs)).

A compio flush task
([crates/plugin-meter/src/flush.rs](../../crates/plugin-meter/src/flush.rs))
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
