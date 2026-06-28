# Runbook — REAL-Stripe billing E2E (`tests/e2e_stripe_billing.sh`)

The capstone, [[feedback_faithful_e2e_tests]]-grade validation of the
metering→billing→Stripe rail (gap #26 / #29) against **REAL Stripe TEST mode**
(`api.stripe.com`) — NOT the in-repo `zeroship-mock-stripe`. It independently
validates the PR-8 dispute `ch_`/`pi_` resolution against REAL Stripe object
shapes, the exact dimension the mock masked.

It boots the real `zeroship-control` binary pointed at real Stripe with the
operator's TEST secret key, drives the real `bill_creator` reconciler, pays the
finalized invoice on real Stripe, replays the real `invoice.paid` /
`charge.dispute.created` webhooks **with valid HMAC signatures the control
instance verifies**, exercises the real `POST /api/invoices/{id}/refunds`
endpoint, and creates a **real Stripe dispute** (`du_…`) via the dispute test
token. Nothing under test is stubbed; only the webhook *delivery* is self-driven
(there is no Stripe CLI on PATH) — the object **shapes** and the **signature
verification** are real.

## Prerequisites

- `zeroship-control` release binary: `cargo build --release -p zeroship-control`.
- Postgres reachable on `localhost:5440` (user `postgres`, pw `zeroship`) — the
  same dev server the cargo integration tests use. The harness creates a
  **dedicated** DB `zeroship_stripe_e2e` and **never** touches the real
  `zeroship` DB nor the concurrent `zeroship_billing_test` DB.
- `docker`, the `zeroship-migrate` bin (built; the migrate step via `ops/db-migrate.sh`), `node`,
  `openssl`, `curl`, and `psql` (default path is the Nix store path; override
  with `ZEROSHIP_PSQL`).
- The operator's Stripe **TEST** keys, sourced from the env file (see Secrets).

## Run

```bash
# 1. Build control if needed
cargo build --release -p zeroship-control

# 2. Source the Stripe TEST keys (REQUIRED — the harness skips cleanly if unset)
source /home/ruiyang/.config/zeroship-stripe-test.env

# 3. Run
./tests/e2e_stripe_billing.sh

# Treat documented real-API divergences as hard failures:
STRICT=1 ./tests/e2e_stripe_billing.sh
```

The harness **skips cleanly (exit 0)** when prereqs are absent (keys not
sourced, no PG :5440, no docker, missing tools) so it is CI-safe. It
**refuses to run** if `STRIPE_TEST_SECRET_KEY` is not an `sk_test_` key.

## Secrets handling

- The env file exports `STRIPE_TEST_SECRET_KEY` (`sk_test_…`) and
  `STRIPE_TEST_PUBLISHABLE_KEY` (`pk_test_…`). The harness reads them from the
  **environment only** — it never prints them, never writes them to disk, never
  bakes them into any artifact, and passes the secret key to control via the
  `STRIPE_SECRET_KEY` env var (NOT a command-line flag, so it never lands in
  `/proc/<pid>/cmdline`).
- The webhook signing secret is a throwaway `whsec_e2e_<random>` generated per
  run, used to produce valid signatures for the **real** verification path.

## What each stage proves (with real object ids in the output)

1. **DB + control** — dedicated `zeroship_stripe_e2e` migrated with the full
   zeroship-migrate platform set; control booted at `https://api.stripe.com`.
2. **Customer + PM** — real `cus_…` + a saved test PaymentMethod (`pm_card_visa`).
3. **Reconcile** — `POST /internal/billing/reconcile` drives the real
   `bill_creator`: real invoice item + invoice + finalize on Stripe; the
   finalized `in_…` lands in `billing_provider_refs(ref_kind='invoice')`.
4. **Pay** — the harness pays the finalized invoice on real Stripe → real
   `pi_…`/`ch_…`; it inspects Stripe's OWN delivered `invoice.paid` event payload
   to assert what the webhook actually carries.
5. **invoice.paid webhook** — a signed event (real ids) → control appends the
   `charge` `invoice_payments` row + the `pi_`/`ch_` linkage rows.
6. **Refund** — `POST /api/invoices/{id}/refunds {destination:cash}` → a real
   Stripe `re_…`; asserts the over-refund cap.
7. **Dispute** — a real dispute (`du_…`) via `tok_createDispute`; a signed
   `charge.dispute.created` → the PR-8 resolution maps `du_`'s `pi_/ch_` to our
   invoice → a `dispute_debit` row → the over-refund cap **tightens**.

## Known REAL-API divergences this harness surfaces

On the test account's default API version (`2025-09-30.clover`, Basil line):

- **D1 — invoice items not swept:** `POST /v1/invoices` does NOT include pending
  invoice items unless `pending_invoice_items_behavior=include` is passed; our
  `create_invoice` (`crates/control/src/stripe_client.rs`) omits it, so the
  finalized invoice can be **$0** and the creator is not billed.
- **D2 — settlement ids not inline:** the Invoice object (and Stripe's OWN
  delivered `invoice.paid` event) carries **no** top-level `payment_intent` /
  `charge` and **no** `payments` key by default; they require
  `expand[]=payments.data.payment`. Our `record_infra_payment` /
  `invoice_payment_object_ids` and `create_refund` read exactly those absent
  fields, so against real Stripe the `pi_`/`ch_` linkage is never recorded (the
  dispute resolution cannot resolve a real dispute) and the cash refund fails
  ("invoice has no payment_intent — cannot refund cash").

These are bugs in `crates/control` (the Stripe client + webhook handler) to be
routed for a fix — they are reported, not patched, by the harness. The mock-Stripe
masked both because it inlined the fields.
