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
platform-measured so app code can neither forge nor suppress it. Three producers
feed one process-wide `Meter`:

1. **The worker** emits the five fixed platform counters (`requests`, `cpu_us`,
   `wall_us`, `egress_bytes`, `ingress_bytes`) once per dispatched request via
   `cache::record_request` → `Meter::record_request`.
2. **Native raw TCP** (`node:net`/`node:tls`) emits accepted outbound socket
   bytes through the same server-stamped `MeterHandle` into fixed
   `egress_bytes` (the spend-enforced metric the gateway path already uses)
   plus `net_egress_bytes`, and emits inbound socket bytes into
   `ingress_bytes` plus `net_ingress_bytes`. The socket path also enforces hard
   per-socket/per-app egress ceilings before billing is involved.
3. **The trusted data primitives** (`env.db`, `env.kv`, `env.storage`) emit raw
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

### Real-API divergences from the mock (faithful e2e findings)

The in-test mock (`crates/control/tests/metering_export_test.rs`) is a fast
in-process HTTP server. The **faithful e2e** against REAL `api.stripe.com` (test
mode) — `tests/e2e_stripe_meters_export.sh` +
`crates/control/tests/metering_export_stripe_live_test.rs` — provisions a real
Billing Meter via `POST /v1/billing/meters` and drives the SAME export cron
through the real cyper `StripeClient` over the wire. Findings:

1. **The aggregate is EVENTUALLY consistent.** Real Stripe aggregates
   `meter_events` ASYNCHRONOUSLY: a `2xx`-accepted event is NOT immediately
   reflected in `GET .../event_summaries` (observed convergence lag tens of
   seconds to a couple of minutes; the full 4-test live run took ~200s, dominated
   by aggregate-convergence polling). The mock answers `event_summaries`
   synchronously. ⇒ The e2e POLLS the aggregate for convergence (up to 180s/leg);
   the export cron is safe because the local high-water — not the live aggregate —
   is the authoritative post-push state, and the C2 reconcile runs on the next
   tick.
2. **`event_summaries` filters by the `[start_time, end_time)` window AND requires
   day-aligned bounds; the mock ignores both.** Real Stripe only counts events
   whose `timestamp` falls inside `[start_time, end_time)`, and with
   `value_grouping_window=day` (which `meter_event_summary` sends) BOTH bounds must
   align to UTC day boundaries. `report_usage` stamps the event at the consumption
   instant (`now`) and `reported_total` queries `[period.start, period.end)`, so
   the reconcile is correct **only because `now ∈ [period.start, period.end)`** for
   the current billing month (period start/end are first-of-month 00:00 UTC, i.e.
   already day-aligned). The mock sums by customer regardless of window, so the
   mock test can use far-future synthetic buckets (`month_period(2031, …)`) while
   stamping at `now`; against real Stripe that combination returns an empty
   aggregate. The live e2e therefore uses the **current calendar month**.
3. **The `[now−35d, now+5min]` timestamp window is REAL — confirmed against
   api.stripe.com.** A meter event stamped at `period.end` (the first of NEXT
   month — weeks in the future) is rejected with HTTP `400` and is NOT counted;
   the same event stamped at `now` is accepted. This is the C1 bug-class proven on
   the real API (the pre-fix code stamped at `period.end`): a `period.end`-stamped
   push is a literal $0-revenue black hole. (Verified per
   docs.stripe.com/api/billing/meter-event/create: "Must be within the past 35
   calendar days or up to 5 minutes in the future.")

**No provider bug was found.** The wire shape — form body
`event_name` + `payload[stripe_customer_id]` + `payload[value]` + `identifier` +
`timestamp`, the `identifier`-as-`Idempotency-Key`, the `Stripe-Version` pin, and
the `event_summaries` `data[].aggregated_value` readback — matches the real
`POST /v1/billing/meter_events` + `GET .../event_summaries` contracts exactly, and
a real meter provisioned with `customer_mapping.event_payload_key=stripe_customer_id`
+ `value_settings.event_payload_key=value` + `default_aggregation.formula=sum`
aggregated the pushed deltas to the exact billable-CU total (incl. the
`gross − included` M1 case). The divergences above are test-design constraints the
faithful e2e encodes (poll for convergence; current-month period), documented so
future edits don't reintroduce the far-future-period or read-after-write
assumptions the mock would let pass. Billing Meters are standard test-mode
resources — meter creation succeeded on the same test account where Connect is
NOT enabled (see `tests/e2e_stripe_webhooks_live.sh`).

## OpenMeter export (M-OpenMeter)

An **export-only** rail (`--metering-provider openmeter`): the platform PUSHES
compute units to OpenMeter as **CloudEvents** and READS the per-subject aggregate
back, but OpenMeter **never invoices** — billing stays on the Native (or Stripe)
rail. The same hardened export cron drives it through
[crates/control/src/metering/provider/openmeter.rs](../../crates/control/src/metering/provider/openmeter.rs)
+ [crates/control/src/openmeter_client.rs](../../crates/control/src/openmeter_client.rs):

- **Wire contract.** `report_usage` → `POST /api/v1/events` with
  `content-type: application/cloudevents+json` + `Authorization: Bearer` carrying
  one CloudEvent 1.0 (`source: zeroship-control`, `type` = the configured
  `--openmeter-event-type`, `subject` = the creator handle, `time` = the
  consumption instant, `data.value` = the CU delta, `data.app_id` for audit).
  `reported_total` → `GET /api/v1/meters/{slug}/query?subject=…&from=…&to=…`,
  summing `data[].value`. The operator provisions ONE meter (slug = eventType =
  e.g. `compute_units`, `aggregation: SUM`, `valueProperty: $.value`).
- **Same inherited guarantees as M-Stripe:** billable-CU parity,
  consumption-instant `time`, the aggregate reconcile
  (`current − max(local_high_water, openmeter_aggregate)`) so a >dedup-window
  re-drive never double-counts, and the durable per-app failure surface.

### Real-API divergences from the mock (faithful e2e findings)

The in-test mock (`crates/control/tests/metering_export_openmeter_test.rs`) is a
fast in-process HTTP server. The **faithful e2e** against a real OpenMeter
(`tests/e2e_openmeter_export.sh` + `crates/control/tests/metering_export_openmeter_live_test.rs`,
`docker-compose.openmeter.yml`) surfaced two behaviours the mock does NOT model —
each a latent bug class if a future change relied on the mock's simplification:

1. **The aggregate is EVENTUALLY consistent.** Real OpenMeter ingests CloudEvents
   into Kafka and a sink-worker drains them into ClickHouse asynchronously: a
   `204`-accepted event is NOT immediately visible to `/query` (observed lag a few
   seconds locally). The mock answers `/query` synchronously. ⇒ Any caller that
   reads `reported_total` *immediately* after `report_usage` and expects the new
   value will see a stale aggregate. The export cron is safe (it reconciles on the
   NEXT tick, and the local high-water — not the live aggregate — is the
   authoritative post-push state), and the e2e POLLS the aggregate for convergence.
2. **`/query` filters by the `time`/`[from,to)` window; the mock ignores it.** Real
   OpenMeter only counts events whose CloudEvent `time` falls inside the query's
   `[from, to)`. `report_usage` stamps `time` at the consumption instant (`now`)
   and `reported_total` queries `[period.start, period.end)`, so the reconcile is
   correct **only because `now ∈ [period.start, period.end)`** for the current
   billing month — which always holds in production. The mock sums by subject
   regardless of window, so the mock test can use far-future synthetic period
   buckets (`month_period(2032, …)`) while stamping `time` at `now`; against real
   OpenMeter that combination returns an empty aggregate. The live e2e therefore
   uses the **current calendar month** as the period. This mirrors the M-Stripe
   future-`period.end` $0-revenue class: a timestamp outside the query window
   silently aggregates to zero.

No provider bug was found — the wire shape (CloudEvents body, headers, 204-on-
accept, `{ "data": [ { "value": N } ] }` query response) matches the real
`/api/v1/events` + `/api/v1/meters/{slug}/query` contracts exactly. The
divergences are test-design constraints the faithful e2e encodes, documented here
so future edits don't reintroduce the far-future-period or read-after-write
assumptions the mock would let pass.
