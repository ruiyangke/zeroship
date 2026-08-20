# Billing And Metering

Zeroship infra billing is a stream-to-provider pipeline. Trusted platform
producers emit immutable `UsageEvent` records, the durable stream buffers them,
the control-plane event forwarder ships them to the selected billing provider,
and the provider is the canonical source for metered usage, rating, and
invoicing.

Spend enforcement is local and per app. A periodic control-plane recompute reads
the retained stream, writes the current-period usage snapshot into
`app_spend_state` through the existing spend engine, and the gateway enforces
that state at the edge.

```
worker + primitives
  -> UsageEvent
  -> durable stream (crates/stream, Redpanda in production)
  -> event_forwarder
  -> billing provider (meter/rater/invoicer/webhooks)

retained stream
  -> spend_recompute
  -> app_spend_state
  -> gateway edge enforcement

retained stream + provider aggregates + invoice records
  -> billing_reconcile safety net
  -> provider correction capability
```

## Producer Model

There is no creator-facing `env.meter` API. Metering is infrastructure so app
code can neither forge nor suppress billable usage.

Trusted producers feed the process-wide `Meter` in `crates/metering`:

- The worker records platform counters once per dispatch: `requests`, `cpu_us`,
  `wall_us`, `egress_bytes`, and `ingress_bytes`.
- Native outbound TCP records accepted network bytes into the same ingress and
  egress counters plus network-specific counters.
- `env.db`, `env.kv`, and `env.storage` record raw usage metrics in the success
  arm of each native operation, such as `db_reads`, `db_writes`, `kv_reads`,
  `kv_writes`, `storage_ops`, and storage byte counters.

`MeterHandle` binds the process meter to the server-injected app id. Native
primitives receive a handle for their app and cannot meter another app.

`Meter::drain` emits one `UsageEvent` per drained `(app_id, metric)` window. The
stable JSON contract lives in `crates/core/src/usage_event.rs`:

- `event_id`: provider idempotency key.
- `source`: producer namespace.
- `subject`: app and creator attribution.
- `meter`: metric name.
- `value`: unsigned quantity.
- `event_time`: Unix seconds.
- `dims`: optional string dimensions.

The worker outbox in `crates/metering/src/outbox.rs` publishes each event as one
stream record. The partition key is the app id, so one app's usage remains
ordered within a stream partition.

## Durable Stream

`crates/stream` provides the `StreamTransport` trait, registry, Redpanda adapter,
and memory adapter. The contract is Kafka-family scoped: partition offsets,
consumer groups, and partition-key ordering are required.

The production transport is Redpanda through `rust-rdkafka` and librdkafka. It
uses an idempotent producer with `acks=all`, manual consumer commits, and
transport-owned offsets. The in-process memory transport is for tests and local
fixtures.

Control builds the stream from:

- `--stream-transport` / `ZEROSHIP_CONTROL_STREAM_TRANSPORT`
- `--stream-config` / `ZEROSHIP_CONTROL_STREAM_CONFIG`
- `--spend-recompute-interval` / `ZEROSHIP_CONTROL_SPEND_RECOMPUTE_INTERVAL`

There is no Postgres raw-event table. Postgres stores config, spend limits,
spend state, invoice bookkeeping, provider refs, dead letters, and
reconciliation findings.

## Provider Stack

Billing providers live under `crates/control/src/metering/provider/`. The
registry maps a string id to a provider factory. Boot builds a role-addressed
`BillingStack`:

- `--meter-provider` / `ZEROSHIP_CONTROL_METER_PROVIDER`
- `--invoicer-provider` / `ZEROSHIP_CONTROL_INVOICER_PROVIDER`
- `--provider-config` / `ZEROSHIP_CONTROL_PROVIDER_CONFIG`
- `--allow-unsupported-billing` / `ALLOW_UNSUPPORTED_BILLING`

Each provider declares its capabilities:

- `Meter`: ingest usage events, read provider aggregates, ensure subjects.
- `Invoicer`: close periods and record adjustment notes.
- `Backfiller`: submit corrected totals when the provider supports backfill.

The built-in adapters are:

- `openmeter`: meter provider.
- `stripe_meters`: Stripe Billing Meters provider.
- `stripe_invoice`: Stripe invoice provider backed by the platform invoice
  store.
- `lago`: full-stack provider with meter, invoicer, and backfill capability.
- `lite`: evaluation-grade self-hosted provider backed by platform storage.

Factories parse opaque JSON config and resolve secrets through `SecretResolver`.
Missing required URLs, meter ids, customer mapping fields, or secrets are boot
errors. A provider whose advertised capability flags do not match its `as_*`
methods also fails boot.

A secret inside `--provider-config` is written the way every other zeroship
secret is written: the material itself, or `urn:zeroship:file:<path>` naming the
file that holds it (owner-only permissions are enforced on the file). A value
starting with `urn:` or `arn:` that is not that file reference is refused, never
taken as literal material.

```json
{"lago": {"api_url": "https://lago.example", "api_key": "urn:zeroship:file:/etc/zeroship/secrets/lago_api_key"}}
```

## Event Forwarder

`crates/control/src/cron/event_forwarder.rs` consumes the stream and forwards
usage to the meter-capable provider.

For each batch:

1. Poll stream records.
2. Decode each payload as `UsageEvent`.
3. Call `Meter::ingest`.
4. Commit stream offsets only after the provider accepts the batch or the batch
   is quarantined as a permanent provider reject.

Transient provider or stream errors do not commit offsets; the batch is retried.
Permanent provider rejects are written to `provider_dead_letter` and recorded as
billing reconciliation findings before offsets are committed. Provider-side
deduplication on `event_id`, transaction id, or the provider's declared key keeps
retries from double-counting.

## Pricing Catalog

The platform plan catalog is data-driven, held in `zeroship.plans` and read
through `crates/control/src/plan_catalog.rs`. It is edited in the DATABASE:
the operator HTTP surface that used to front it (`/api/plans`,
`/api/pricing-config`) was gated on a fleet-wide grant that no principal holds
since the platform staff roles were deleted, and it went with them. Per tier:

- `base_fee_cents`
- included quota by metric
- overage rate by metric
- `spend_limit_default_cents`
- runtime and network limits

Pricing math lives in `crates/control/src/pricing.rs`. Money is integer cents
with widened intermediates and line-boundary rounding. For a period:

```text
charge = base_fee + sum(max(0, usage[metric] - included[metric]) * overage_rate[metric])
```

Providers that own rating and invoicing use their provider-side configuration as
the billing source. Providers that rely on the platform invoice store use the
same catalog and frozen invoice records that the creator billing APIs expose.

## Spend Enforcement

Spend limits are per app and are distinct from included quota. The effective
limit is either the app override in `app_spend_limit` or the plan default.

`crates/control/src/cron/spend_recompute.rs` periodically reads the retained
stream for the current billing period, computes a full sum per `(app_id, metric)`,
overwrites the `usage_aggregates` snapshot for that period, and runs the spend
evaluator. The default recompute cadence is hourly and is configurable with
`ZEROSHIP_CONTROL_SPEND_RECOMPUTE_INTERVAL`.

`crates/control/src/spend.rs` derives `SpendState`:

- `Allow`: below warning threshold.
- `Warn`: near the limit; the gateway can surface a warning header.
- `Degrade`: soft cap; the gateway applies tighter rate and concurrency limits.
- `Block`: hard cap; the gateway returns 402 before dispatch.

The gateway pulls route and spend state from control on its normal registry poll
and enforces locally in `crates/gateway/src/enforce.rs`. No provider call is made
on the request path.

The gateway also applies a coarse per-app throughput backstop with rate and
concurrency limits. That caps worst-case overshoot between recompute ticks. Free
tier apps use tighter limits because they have little or no paid headroom.

## Reconciliation Safety Net

`crates/control/src/cron/billing_reconcile.rs` is the safety net for provider
drift and late period adjustments. It compares:

- the local stream witness for the period,
- provider aggregate read-back when the meter supports it,
- invoice records and provider refs for the period,
- prior correction history.

The reconciler records findings for provider drift, late adjustments, and
provider rejects. When correction is possible, it uses the provider's declared
`CorrectionCapability`:

- `Backfill`: call `Backfiller::backfill` with the corrected total inside the
  declared window.
- `InvoiceCredit`: issue an idempotent signed adjustment note through the
  invoicer.
- `None`: record the finding for operator action.

Correction actions carry deterministic idempotency keys and correction sequence
numbers so a re-run with the same corrected quantity is a no-op.

## Operator Checklist

For production billing:

1. Run a durable stream transport, normally Redpanda, with retention long enough
   for spend recompute and reconciliation.
2. Configure `ZEROSHIP_CONTROL_STREAM_TRANSPORT=redpanda` and a `ZEROSHIP_CONTROL_STREAM_CONFIG` JSON object with
   broker, topic, and consumer group fields.
3. Select provider roles with `ZEROSHIP_CONTROL_METER_PROVIDER` and `ZEROSHIP_CONTROL_INVOICER_PROVIDER`.
4. Put provider config in `ZEROSHIP_CONTROL_PROVIDER_CONFIG` using secret handles, not plaintext
   secrets.
5. Keep plan catalog pricing, included quotas, spend-limit defaults, and network
   backstops aligned with the tier you sell. These are database rows, not an
   API: there is no operator endpoint to change them.
6. Monitor event-forwarder retries, provider dead letters, reconciliation
   findings, spend-state transitions, and gateway 402/degrade rates.
