# Runbook — Stripe-DELIVERED webhook E2E (`tests/e2e_stripe_webhooks_live.sh`)

The [[feedback_faithful_e2e_tests]] capstone that closes the one gap the sibling
harness (`tests/e2e_stripe_billing.sh`, see `stripe-billing-e2e.md`) could not:
that harness **self-constructed** the webhook event envelopes and HMAC-signed
them itself (no Stripe CLI was on PATH), so it proved the signature-verify +
handler logic but never exercised Stripe's **real delivery** — the real event
`type` names, the real `data.object` shape, the real `api_version`, real
ordering, real cascades, and Stripe's **own** signature.

This harness uses the **Stripe CLI** (`stripe listen` + `stripe trigger`) so
**Stripe itself** delivers the real, signed event envelopes through the listener
to the control instance's `/internal/webhooks/stripe`. Nothing about the
delivery is self-driven.

```
stripe listen --print-secret              → capture the STABLE whsec_…
  ─► boot zeroship-control with STRIPE_WEBHOOK_SECRET = that secret
  ─► stripe listen --forward-to $CONTROL/internal/webhooks/stripe   (bg forwarder)
      (Stripe streams every test-mode event to control, REAL-signed)
  ─► create real objects (customer + invoice + pay) → Stripe DELIVERS
      invoice.paid / invoice.payment_succeeded / payment_intent.succeeded /
      charge.succeeded … through the listener to our handler
  ─► stripe trigger <event>                 → Stripe fires + DELIVERS a real
      test event of that type
```

## Prerequisites

- `zeroship-control` release binary: `cargo build --release -p zeroship-control`.
- The **Stripe CLI**. Obtain it via `nix-shell -p stripe-cli` (the harness
  resolves the realised `/nix/store/*-stripe-cli-*/bin/stripe` automatically, or
  honour `ZEROSHIP_STRIPE_BIN=/path/to/stripe`, or a `stripe` already on PATH).
- Postgres on `localhost:5440` (user `postgres`, pw `zeroship`). The harness
  creates a **dedicated** DB `zeroship_stripe_e2e` and **never** touches the real
  `zeroship` DB nor the concurrent `zeroship_billing_test` DB.
- `docker` (Liquibase migrate via `ops/db-migrate.sh`), `node`, `openssl`, `curl`,
  `psql` (default Nix store path; override with `ZEROSHIP_PSQL`).
- The operator's Stripe **TEST** secret key, sourced from the env file.

## Run

```bash
cargo build --release -p zeroship-control
source /home/ruiyang/.config/zeroship-stripe-test.env   # REQUIRED — skips cleanly if unset
./tests/e2e_stripe_webhooks_live.sh
STRICT=1 ./tests/e2e_stripe_webhooks_live.sh            # documented divergences = hard fail
```

Skips cleanly (exit 0) when prereqs are absent (no stripe CLI / keys / PG :5440 /
docker / tools). Refuses to run if `STRIPE_TEST_SECRET_KEY` is not an `sk_test_`
key. Self-managed up/down: tears down `stripe listen` + control on exit.

## Secrets handling

- Reads `STRIPE_TEST_SECRET_KEY` (`sk_test_…`) from the **environment only**;
  never prints/writes/commits it; passes it to control via the `STRIPE_SECRET_KEY`
  env var (not argv, so it never lands in `/proc/<pid>/cmdline`).
- The webhook signing secret is the **stable** value from
  `stripe listen --print-secret` (so control's `STRIPE_WEBHOOK_SECRET` matches the
  secret Stripe signs its deliveries with — the real verify path). It is captured
  into a shell var, redacted in all log echoes, and never persisted.

## What each stage proves (with real ids in the output)

1. **DB + control + forwarder** — dedicated `zeroship_stripe_e2e` migrated with
   the full changelog (incl. `0054` webhook follow-ups); control at
   `https://api.stripe.com`; `stripe listen` forwarder READY.
2. **Core loop, Stripe-DELIVERED** — a real customer + a real paid invoice; Stripe
   DELIVERS the natural money cascade (`invoice.created → finalized → invoiceitem.created
   → invoice.paid → invoice_payment.paid → invoice.payment_succeeded →
   payment_intent.succeeded → charge.succeeded`); control appends the `charge`
   `invoice_payments` row and **recovers** the real `pi_`/`ch_` linkage via its
   out-of-band `expand[]=payments.data.payment` fetch.
3. **The 3 newly-handled deferred events, Stripe-DELIVERED** —
   `charge.refund.updated` and `payment_intent.payment_failed` are fired by
   `stripe trigger` and DELIVERED + ACK'd; `payout.failed` is reported as
   un-deliverable (no CLI fixture + Connect-disabled account — see below).
4. **Dispute, Stripe-DELIVERED** — a real dispute (`du_…`) via the dispute test
   card `tok_createDispute`; Stripe DELIVERS `charge.dispute.created` (+
   `charge.dispute.funds_withdrawn`); the handler resolves it to our invoice via
   the pre-linked `pi_`/`ch_` → a `billing_disputes` row + a `dispute_debit`
   (cap tightens). Subject to a real-delivery timing race (see below).
5. **Connect / payout** — reported honestly: blocked on this test account (see
   below). The one driveable leg, `account.updated`, IS DELIVERED + ACK'd.

## Known REAL-DELIVERY findings & genuine automation limits

On the test account's default API version (`2025-09-30.clover`, Basil line):

- **Delivered `invoice.paid` carries NO settlement ids.** The real delivered
  event's `data.object` is `{top_charge:null, top_pi:null, has_payments:false}`.
  This is the D2 shape the self-constructed harness only *assumed*. Control's
  `record_infra_payment` handles it by an out-of-band
  `expand[]=payments.data.payment` fetch (`settlement_ids_for` →
  `invoice_settlement_ids`), which **recovers** the real `pi_`/`ch_` — the harness
  asserts the recovered linkage rows landed.
- **New delivered event type not modelled by the self-constructed harness:**
  `invoice_payment.paid` (a separate Basil object) appears in the real cascade.
- **`payout.failed` cannot be Stripe-DELIVERED here.** `stripe trigger` has no
  `payout.failed` fixture (only `payout.created`/`payout.updated`), and a real
  bouncing payout needs a real connected account — blocked because the test
  account is **not signed up for Stripe Connect** (`POST /v1/accounts` →
  "You can only create new accounts if you've signed up for Connect"). The
  `handle_payout_failed` logic is unit/integration-tested; its real-delivery leg
  is genuinely unautomatable on this account.
- **The failed-refund REVERSAL cannot be driven by real delivery in TEST mode.**
  Stripe does not fail refunds on test cards, and `stripe trigger
  charge.refund.updated --override status=failed` delivers a brand-NEW refund
  fixture (not an UPDATE keyed on a `re_` our DB recorded as `issued`). The real
  `charge.refund.updated` IS delivered + ACK'd (the handler correctly no-ops a
  non-failed refund); the terminal-failure reversal + `refund_clawback`
  (`reconcile_failed_refund`) is unit/integration-tested.
- **Connect / payout leg is Connect-gated.** Real Express/Custom account creation,
  account-link onboarding, destination charges, the application-fee/FeePolicy
  split, real payouts, and the M4 settling-account ownership check all require
  real connected accounts — blocked as above. The hosted Express onboarding
  browser flow is never fully automatable regardless. The `account.updated`
  gate-refresh IS delivered + ACK'd (an unlinked-account no-op, the correct
  fail-safe); the gate-FLIP write path needs a real linked `acct_`. The full
  Connect money flow has its own dedicated harness —
  `tests/e2e_stripe_connect_live.sh` (see `stripe-connect-live-e2e.md`) — which
  probes for Connect and SKIPs cleanly until it is enabled at
  `dashboard.stripe.com/connect`.
- **Dispute resolution rides a NON-RECOVERABLE real-delivery timing race.** Stripe
  creates and DELIVERS `charge.dispute.created` within ~1s of the
  `tok_createDispute` confirm — frequently BEFORE the harness's `pi_`/`ch_`→invoice
  linkage commits. The first delivery then (correctly) 200-acks `no_internal_invoice`,
  and control's **M3 replay-dedup ledger** marks that `event_id` PROCESSED — so a
  `stripe events resend` of the SAME event is acked `duplicate` and **never
  re-dispatches** (verified: resend DOES re-deliver through the listener, but the
  dedup ledger no-ops it). The race is therefore permanently lost for that dispute.
  The harness pre-links as tightly as possible (reading `latest_charge` from the
  confirm response and inserting in the next statement); when it still loses, it
  emits a divergence (never a fake pass) that **verifies the delivered dispute
  object's `charge` matches the linked `ch_`** — proving the `ch_`/`pi_` resolution
  logic is correct and only the live delivery ORDER beat it. This is correct dedup
  behaviour, not a handler bug; the resolution itself is proven deterministically by
  the sibling harness (`stripe-billing-e2e.md`) and the crate tests.

These are **real-delivery characteristics and automation limits**, reported (not
patched) by the harness. Any genuine handler bug surfaced by real delivery is
flagged for routing to `crates/control`, never fixed in the test.
