# Runbook - Stripe Connect payment-flow E2E (`tests/e2e_stripe_connect_live.sh`)

The Connect peer of `tests/e2e_stripe_billing.sh` (the infrastructure billing path, see
`stripe-billing-e2e.md`) and `tests/e2e_stripe_webhooks_live.sh` (real-delivery
webhooks). It drives zeroship's **Stripe Connect payment** path against
**REAL Stripe TEST mode** (`api.stripe.com`), not the in-repo mock: the cyper
`StripeClient` Connect calls (`create_connect_account`, `create_account_link`,
`retrieve_account`, `create_connect_payment_intent`), the server-held `FeePolicy` (seeded directly in the database now that the operator PUT is deleted)
(`crates/zeroship-control/src/fee_policy.rs`), the `connect_checkout` / `callback`
handlers, and the signature-verified `/internal/webhooks/stripe`
ingest of `account.updated` / `invoice.paid` (Connect payment) / `payout.failed`.

## Connect must be ENABLED on the test account — the harness refuses otherwise

`POST /v1/accounts` returns HTTP 400 `invalid_request_error`: "You can only
create new accounts if you've signed up for Connect…" on an account that has not
signed up. The harness probes this **first** and, when Connect is off, refuses
with exit 2 naming `dashboard.stripe.com/connect`. It runs the full payment flow
the moment Connect is enabled there.

It used to exit 0 on that probe, which made a run against a Connect-disabled
account print the same result as one that had moved money end to end.

## What still needs the user to do (one-time, to fully validate)

1. **Enable Connect** for the test account at `dashboard.stripe.com/connect`
   (Platform/marketplace; the API-creatable Express/Custom account path is what
   the harness needs). No code change is required afterward.
2. Re-run the harness (below). It will mint a real Express `acct_…`, drive the
   server-stamped configured application fee onto a real PaymentIntent, exercise the M2
   gate and M4 attribution, and assert against real fetched-back Stripe objects.

The **hosted Express browser onboarding flow is never fully automatable**; the
harness asserts the `account_links` URL is returned and uses Stripe **test-mode
capability activation** (prefilled business profile + TOS acceptance + a test
external account) — falling back to a real-shaped `account.updated` cache-write
when Stripe's test-mode activation lags — so the `charges_enabled` gate can open
and the charge path runs.

## Prerequisites

- `zeroship-control` release binary: `cargo build --release -p zeroship-control`.
- Postgres on `localhost:5440` (user `postgres`, pw `zeroship`). The harness
  creates a **dedicated** DB `zeroship_stripe_e2e` and **never** touches the real
  `zeroship` DB nor a concurrent `zeroship_billing_test` DB.
- `docker`, the built `zero-migrate` CLI
  (`packages/zero-migrate-cli/dist/cli-bin.js`, which the harness reaches
  through `zs_platform_migrate` in `tests/lib/runtime_secrets.sh`), `node`,
  `openssl`, `curl`, and `psql` (taken from $PATH, else the nix store;
  override with `PSQL`). Every one of these is a refusal when absent, not a
  skip.
- The operator's Stripe **TEST** secret key, sourced from the env file.

## Run

```bash
cargo build --release -p zeroship-control
pnpm install && pnpm build
source /home/ruiyang/.config/zeroship-stripe-test.env   # REQUIRED — refuses if unset
./tests/e2e_stripe_connect_live.sh
STRICT=1 ./tests/e2e_stripe_connect_live.sh             # documented divergences = hard fail
```

Refuses (exit 2) when a prerequisite is absent (no keys / PG :5440 / docker /
tools / jose) **or when Connect is not enabled**, naming the thing and its
remedy. It also refuses if `STRIPE_TEST_SECRET_KEY` is not an `sk_test_` key.
Self-managed up/down: tears down control and deletes the test-mode connected
accounts it minted on exit.

Nothing in `.github/workflows/` runs this script and neither does
`cargo xtask test billing`, so a refusal reaches the person who ran it by hand
and cannot leave a CI job permanently red.

## Secrets handling

- Reads `STRIPE_TEST_SECRET_KEY` (`sk_test_…`) from the **environment only**;
  never prints/writes/commits it; passes it to control via the `ZEROSHIP_CONTROL_STRIPE_SECRET_KEY`
  env var (not argv, so it never lands in `/proc/<pid>/cmdline`).
- The webhook signing secret is a per-run throwaway; the harness HMAC-SHA256-signs
  its own real-shaped events with it (the REAL `/internal/webhooks/stripe` verify
  path) and never persists it.

## What each stage proves (when Connect is enabled)

1. **DB + control** — dedicated `zeroship_stripe_e2e` migrated with the full
   platform corpus by `zeroship-platform-migrate` (incl. `V0044`
   `creator_fee_policy` + the `creator_accounts` Connect flags); control at
   `https://api.stripe.com`.
2. **Express account + onboarding link** — `onboard` mints a real `acct_…`
   (`type=express`, `metadata[creator_id]` ownership signal) and returns a real
   `account_links` onboarding URL; `callback` verifies ownership + persists
   Stripe's `charges_enabled` truth.
3. **Server-stamped fee** - `connect_checkout` resolves the server-held `FeePolicy`
   and stamps `application_fee_amount` + `transfer_data[destination]`
   on the real PaymentIntent; a malicious client `application_fee_amount=1` is
   **ignored** (no wire path). The harness fetches the PI back from Stripe and
   asserts `application_fee_amount` matches the configured policy, `amount ==
   gross`, and `destination == acct_`. An operator-set 25%-capped-$40 policy is
   also honored on the wire.
4. **M2 disabled-account gate** - a real-shaped `account.updated` flipping
   `charges_enabled→false` re-caches the flag; a subsequent `connect_checkout` is
   **blocked (400)** with no PaymentIntent created.
5. **M4 payout attribution** - a Connect `invoice.paid` whose settling
   account (`on_behalf_of` / `transfer_data.destination`) **mismatches** the
   creator's stored `stripe_account_id` is **rejected** (`attribution_mismatch`,
   no payout row — metadata-only trust refused); an **owned** settling account is
   **credited** (net = gross - configured fee); a payment event with **no** settling
   account is refused (`no_settling_account`).
6. **payout.failed** — a real-shaped `payout.failed` for the linked `acct_`
   resolves the creator (`get_creator_by_account`) and records a `payout_failures`
   row (idempotent on `po_…`); an unlinked-account `payout.failed` is a benign
   no-op.

Code map: handlers in `crates/zeroship-control/src/stripe_handlers.rs`
(`onboard` :128, `callback` :329, `connect_checkout` :459, `dispatch_event`
:1107, `handle_account_updated` :1440, `handle_payout_failed` :2121); fee math in
`crates/zeroship-control/src/fee_policy.rs` (`fee_cents`); store ownership in
`crates/zeroship-control/src/stripe_store.rs` (`account_belongs_to_creator` :295,
`get_creator_by_account` :316, `update_account_flags_by_account_id` :228); Connect
wire shapes in `crates/zeroship-control/src/stripe_client.rs`
(`create_connect_account` :963, `create_account_link` :983,
`create_connect_payment_intent` :1021); schema in
`db/migrations-ts/20260702000400_billing_metering_invoice_tables.ts`. Offline logic coverage:
`crates/zeroship-control/tests/connect_fee_test.rs`.

Any genuine handler bug surfaced by real Stripe is **flagged** (a divergence),
never patched in the test.
