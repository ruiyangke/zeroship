# Billing And Metering

Zeroship bills you for the infrastructure your apps consume. This page is the
contract: what is measured, how it is priced, what you can read back, and what
happens at every limit. It is written for the person building an app.

The platform invariant that shapes everything below: **metering is
infrastructure, not an app API.** There is no `env.meter`. Usage is measured
server-side at the platform and primitive boundaries, so your app code can
neither forge nor suppress what it costs. What this page documents is the part
you can observe and rely on — the metric names, the pricing model, the read
endpoints, and the enforcement states.

## What is measured

The platform records named counters, one set per app, at the boundaries where
your app does work. The names you will see (in the order you would first
encounter them):

- `requests` — one per dispatched request.
- `cpu_us` — CPU time, in microseconds.
- `wall_us` — wall-clock time, in microseconds.
- `egress_bytes` / `ingress_bytes` — bytes out and in.
- `gateway_egress_bytes` — bytes the platform itself serves against your route
  (static assets, redirects, gateway-owned streams) rather than your worker.
- `db_reads` — one database read operation: a query, count, or search. Counts
  operations, not rows.
- `db_writes` — one database mutation: an insert, update, or delete.
- `db_rows_written` — rows a mutation affected or returned.
- `kv_reads` / `kv_writes` — key-value operations.
- `storage_ops` / `storage_bytes` — object-storage operations and bytes.
- `net_egress_bytes` / `net_ingress_bytes` — bytes on raw outbound sockets
  (`node:net`, `node:tls`, outbound WebSocket), recorded in addition to
  `egress_bytes` / `ingress_bytes`.

Every successful operation is measured; failures on a primitive are not. You
cannot emit a metric yourself, and you cannot zero one out: the only inputs are
the platform counters and the trusted primitives (`env.db`, `env.kv`,
`env.storage`, and the network/runtime boundaries).

This list is not closed, and you should not assume it is the whole set for your
deployment. The authoritative view of what your app accrued is the usage
endpoint below; a metric it reports is one the platform measured.

## How usage is priced

Usage is priced in **compute units (CU)** — an integer unit, never a float. Each
metric the platform measures has a cost weight, written as an exact ratio: so
many CU per so many operations (for example "1 CU per 1,000 bytes"). A metric's
raw total is multiplied by that ratio, the result floored per metric, and the
floors summed. For a billing period (a calendar month in UTC, from 00:00 UTC on
the 1st):

```text
total_units    = Σ  floor( max(0, usage[m]) × units_per_op[m] / per_units[m] )
billable_units = max(0, total_units − included_units)
charge         = base_fee + round_half_up(billable_units × fx)
```

- A metric's weight is the pair `units_per_op` over `per_units` (the number of
  CU credited per `per_units` operations), so a sub-unit weight stays exact
  rather than rounding to a fraction.
- A metric with no weight contributes zero CU. Metering something and then not
  weighting it is free, not an error.
- The floor is applied per metric before summing, and the currency conversion
  rounds exactly once, at the end — so rounding can only under-count, never
  over-bill.
- The per-CU price `fx` is held as an integer number of pico-cents (10⁻¹² cent)
  per CU, so a sub-cent unit price is exact. `base_fee` and the amounts on your
  invoices are integer cents.

The weight table and the `fx` value are operator configuration, set per
deployment and changeable between billing periods; they are not published as
fixed numbers. You never need them in order to see what you owe — your own
charge is readable without them. The projected-charge endpoint returns a figure
already priced through this formula, and every finalized invoice freezes the
exact `fx`, `base_fee`, `included_units`, and weight snapshot that were applied
(see "Reading your bill").

## Plans

Every app belongs to a plan, and a plan is what carries the numbers above plus
your runtime and network limits. A plan is an operator-managed catalog entry you
assign by id, not a value you author. The three built-in tiers, with their
catalog ids (`pln_…`) and pricing:

- `free` — `pln_0pmepeesn0v30md0sick7lo65`. Base fee `$0.00`, `100,000` CU
  included, and a `$0.00` spend cap, so no card is required. Its included quota
  is the only usage you get before the app blocks: there is no paid headroom.
- `pro` — `pln_7einr1yv1u9nabqjohrit3f7y`. Base fee `$5.00`, `1,000,000` CU
  included, and a `$50.00` default spend cap. Usage past the included quota is
  billed at the per-CU `fx`, up to the cap.
- `unlimited` — `pln_4cklt6kbysmsugjft40bdetjx`. No base fee, no included quota,
  and no runtime caps. This tier is **operator-only**: a creator app cannot
  self-assign it.

A plan carries a flag telling you whether you may assign it yourself. The only
built-in tiers you can assign are `free` and `pro`; an operator may assign any
plan. Assigning one you may not self-select is refused with
`403 plan not assignable by creator`, and naming a plan id that does not exist
(or has been archived) is refused with `400`. There is **no plan-list endpoint**:
you set a plan by id and read your current `plan_id` back through the
billing-status endpoint, not through a catalog you browse.

The change is made with `PUT /api/apps/{id}/plan`, body `{"plan_id": "<pln_…>"}`
(wrapped by `@zeroship/control` as `control.apps.setPlan(id, { plan_id })`). A
mid-period plan change is priced in segments — the period is split at the
change, and each segment is priced under the plan that was in force for it.

## Reading your usage

`GET /api/apps/{id}/usage` returns your app's usage for the current calendar
month (UTC) as a plain object mapping metric name to total:

```json
{ "requests": 12841, "cpu_us": 9400000, "egress_bytes": 510000000 }
```

In `@zeroship/control` this is `control.apps.usage(id)` (typed
`Record<string, number>`). This read requires the same billing authority as the
invoice reads in "Reading your bill".

Three things to rely on about this number:

- It is the **current calendar-month** total, in raw metric units — not CU and
  not money.
- It is a **periodic snapshot, not a live counter**. Within a billing month a
  total only grows, and the snapshot is recomputed on an operator-tunable
  cadence (one hour by default), so a value you read may lag the most recent
  requests. Only a finalized invoice is authoritative; the usage view is for
  orientation.
- The projected-charge endpoint below is held for a brief cache window (60
  seconds) and answers with an `as_of` timestamp, so you can tell how fresh its
  figure is.

## Spend limits and enforcement

Separate from pricing, each app has a **spend limit**: a cap on the money value
of the current period, enforced at the edge before your code runs. The
effective limit is your plan's default unless you set a per-app override.

The override is deliberately **reduction-only**: `PUT /api/apps/{id}/spend-limit`
with body `{"cents": <number|null>}` sets a cap, and `null` clears it back to the
plan default. You can only lower your cap — an override above the plan default
is refused with `403 spend limit exceeds plan maximum`. Raising your headroom
means upgrading the plan, which is a billing-gated action.
`GET /api/apps/{id}/spend-limit` returns `effective_limit_cents`, your
`override_cents`, the `plan_default_cents`, and the current `state`.

As your priced spend approaches the cap, your app moves through four states.
You can read the state as one of `allow`, `warn`, `degrade`, or `block`, entered
at fractions of the effective cap:

- `allow` — served normally. Below 80% of the cap.
- `warn` — at or above 80%. Still served; the gateway adds an
  `x-zs-spend-warn: 1` header to responses so your frontend or your callers can
  key on it.
- `degrade` — at or above 95%. Still served, but throttled: the app's
  concurrency ceiling is divided by 8 (to a minimum of one in-flight request)
  and each request spends eight times the normal rate-limit allowance — an
  eight-fold slowdown against the same limits, not a refusal.
- `block` — at 100% of the cap. Refused before dispatch with `402` and body
  `{"code": "SPEND_LIMIT"}`, for every action class: worker, redirect, rewrite,
  and static assets alike. Raising your spend limit or moving to a higher plan
  recovers it.

States tighten immediately at the threshold but relax only once spend falls five
points past it — an anti-flap deadband (`degrade` relaxes to `warn` only below
90%, `warn` to `allow` only below 75%). Raising the cap yourself recovers the
app immediately rather than waiting out the deadband.

There is a second, account-level gate in front of the spend gate. Your
organization's account state is one of `active`, `past_due`, or `suspended`:

- `active` — payment current.
- `past_due` — a payment has failed and is being retried. This is a grace
  window: your apps keep serving.
- `suspended` — the dunning window (an operator setting, seven days by default)
  has elapsed. Requests are refused with `402` and body
  `{"code": "ACCOUNT_SUSPENDED"}`. Suspension is reversible: a completed payment
  moves the account straight back to `active`. The seven-day figure is a
  default, not a guarantee — your deployment's operator may tune the window.

The two refusals carry distinct codes on purpose, so you can tell a usage cap
(`SPEND_LIMIT`) from a payment failure (`ACCOUNT_SUSPENDED`).

## Reading your bill

The organization is the billing subject: an app's invoices and money state are
owned by the organization that owns the app, not by the app or a single user.
App-scoped reads resolve that organization server-side, and every billing read
below requires a seat that holds billing authority at that organization.
Organizations rank money authority separately from app authority: the `billing`
seat outranks `admin` for money actions, while `admin` outranks it for app
administration — so a seat that can administer apps may still be unable to read
invoices, and vice versa.

The organization id you need below (`?organization_id=`) is the `id` of an
organization you hold a seat in, listed by `GET /api/organizations`.

- `GET /api/apps/{id}/invoices?limit=&offset=` — your organization's invoice
  history, newest first. The response is `{"invoices": […]}`; each entry carries
  `id`, `period`, `status`, `currency`, `subtotal_cents`, `credit_cents`,
  `tax_cents`, `total_cents`, and `finalized_at`. `limit` defaults to `50` and
  is clamped to `1..200`.
- `GET /api/invoices/{id}` — one invoice in full, including a frozen line per
  app/segment. Each line carries `app_id`, `segment_no`, `plan_id`,
  `included_units`, `fx_pico_cents_per_unit`, `base_fee_cents`, `amount_cents`,
  `usage_snapshot`, and `weights_snapshot`, plus the derived `compute_units` and
  `billable_units`. Together these reproduce the charge exactly.
- `GET /api/apps/{id}/projected-charge` — your current-period charge as
  projected over live usage. The response is
  `{ "projected_charge_cents": …, "authoritative": false, "period": "…", "as_of": … }`.
  It is explicitly **non-authoritative** (`authoritative` is always `false`):
  promises only an orientation figure, not a bill. Only a finalized invoice
  bills.
- `GET /api/apps/{id}/billing-status` — `plan_id`, the effective spend cap
  (`effective_limit_cents`, with `override_cents` and `plan_default_cents`
  beside it), `spend_state`, and `account_state`.
- `GET /api/billing/credit-balance?organization_id=` — your USD credit balance
  (`balance_cents`, `currency`) plus recent ledger entries. `currency` is always
  `usd`; each `recent` entry carries `id`, `kind`, `amount_cents`, `currency`,
  `applied_invoice_id`, `note`, `expires_at`, and `created_at`.
- `GET /api/billing/payment-method?organization_id=` — whether a default
  payment method is on file (`default_pm_set`) and whether a billing identity
  exists (`customer_ref_present`). These are presence flags only — no provider
  id is exposed.

Credit balance is readable, not writable: credit grants, refunds, and disputes
are operator actions, not something your app can issue. You read the balance
and see it applied at invoice finalization.

## Paying for infrastructure

The platform charges your organization's card for its infrastructure use. You
attach (or replace) that card with `POST /api/organizations/{id}/billing/setup`,
which returns a Stripe-hosted URL (`{ "url": …, "customer_id": … }`) to redirect
to. The card lives on the provider; the platform stores only whether one exists.

The address the platform contacts — and where billing notices are sent — is the
organization's `billing_email`, set when the organization is created and edited
with `PATCH /api/organizations/{organization_id}`. It is deliberately the
organization's address, not whichever member happened to mint it.

## Selling to your own end users

Separate from the infrastructure bill above, you can take payments from your
own customers through Stripe Connect. This is a different money direction: your
customers pay you, and the platform takes an application fee on each charge.
The fee is a platform-held policy, **15% of the charge by default**, and an
operator may set a different per-organization policy.

The `@zeroship/payments` SDK wraps it. `startOnboarding()` returns the Stripe
onboarding link, and `checkout({ amountCents, currency, cartId })` creates a
payment intent. `checkout` returns
`{ paymentIntentId, clientSecret, applicationFeeCents }`: hand `clientSecret` to
Stripe.js in the browser to confirm the payment, and read `applicationFeeCents`
only to display the fee the platform stamped. The fee is **server-stamped and
cannot be named, set, or overridden by your code** — there is no fee parameter
on any method, and the wire body carries only business fields. The server-side
routes are `POST /api/organizations/{id}/stripe/onboard` and
`POST /api/organizations/{id}/connect/checkout`, with your earnings readable at
`GET /api/organizations/{id}/earnings`.

The `@zeroship/payments` package also exports
`verifyWebhook(rawBody, signatureHeader, secret, opts?)` to verify Stripe
webhook signatures with WebCrypto HMAC-SHA256 before trusting a webhook. Pass the
exact bytes Stripe sent (never re-stringified JSON); it resolves to
`{ valid: true, timestamp }` or `{ valid: false, reason }`. The optional `opts`
accepts a `tolerance` in seconds (default 300) and a `now` clock override for
tests.

## Summary of errors

The two gateway refusals (spend and account blocks) return a `402` whose body is
a single `{"code": …}` key. Every other error below comes from the control plane
and returns its message under an `"error"` key.

| Situation | Result |
| --- | --- |
| Spend state reaches `block` | `402` `{"code":"SPEND_LIMIT"}` |
| Account is `suspended` | `402` `{"code":"ACCOUNT_SUSPENDED"}` |
| Spend override above the plan default | `403` `{"error":"spend limit exceeds plan maximum","plan_max_cents":N}` |
| Assigning a plan you may not self-select | `403` `{"error":"plan not assignable by creator","detail":"…"}` |
| Assigning an archived or unknown plan | `400` `{"error":"plan archived"}` / `{"error":"unknown plan"}` |
| Checkout with an unready connected account | `400` `{"error":"organization stripe account not ready (complete onboarding)"}` |
| Checkout with a missing cart id | `400` `{"error":"cart_id is required"}` |
| Checkout with a zero amount | `400` `{"error":"amount_cents must be positive"}` |
| Checkout with a non-ISO currency | `400` `{"error":"currency must be a 3-letter ISO code (lowercase)"}` |