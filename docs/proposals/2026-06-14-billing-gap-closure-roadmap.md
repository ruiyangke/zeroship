# Billing & metering — gap-closure design + sequenced roadmap

**Status:** DESIGN ONLY — no implementation. · **Date:** 2026-06-14 ·
**Worktree:** `appbase-billing` @ `feat/billing-metering` · **Latest changeset:** `0043`

This roadmap closes the *known* gaps left open after the core billing/metering
pipeline shipped + hardened (meter → price → enforce → invoice, pluggable
Native/Stripe-Meters/OpenMeter providers). It is the sequel to
`docs/proposals/2026-06-12-billing-metering-reimpl.md` (the FINALIZED DESIGN +
Stream-2 spec + the post-merge hardening backlog) and assumes everything in that
doc is *built* (PR1–7, billing-v2 A/B, provider abstraction, C1/C2 + MAJOR fixes).

It is written to compose with a **parallel schema-redesign proposal** landing at
`docs/proposals/2026-06-14-billing-schema-redesign.md` (cleans the existing 11
billing tables; may add a credits/invoice model). That redesign is the **schema
foundation** for several gaps here. This roadmap does **not** re-design the
existing infra-billing tables — it builds the gaps *on* the redesigned schema and
flags every dependency on it explicitly (see the **Schema-redesign dependency
matrix** below).

---

## 0. Where the built system stands (grounding)

Confirmed by reading the live code in this worktree:

- **Metering producer:** platform-measured (db/kv/storage primitives emit on the
  `Ok` arm; worker emits the 5 platform auto-counters via
  `cache::record_request`, finalised once per request at dispatch end —
  `worker/src/handler.rs:268-278`). `env.meter` is deleted (Refactor A).
- **Aggregation:** `control/metering.rs` — idempotent `(worker_id, sequence)`
  dedup into `usage_aggregates` (raw-metric-keyed, changeset `0037`).
- **Pricing:** compute-unit model (`pricing.rs::total_units`/`charge_cents`),
  global `metric_weights` (`0041`) + `pricing_config.fx` global FX (`0042`),
  per-plan catalog `zeroship.plans` (`0038`, `0042` assignable flag).
- **Spend enforcement:** `spend.rs::derive_state` (Warn 80 / Degrade 95 / Block
  100 + deadband + raised-limit recovery), `cron/spend_reconcile.rs` (~60s),
  pulled on `RouteEntry.spend_state`; gateway `enforce.rs` (`check_spend` 402 +
  immutable-bucket Degrade via `DEGRADE_FACTOR=8`). `app_spend_state` +
  `spend_state_history` (`0039`, RLS fail-closed).
- **Billing rail:** `stripe_client.rs` (cyper, `StripeApi` trait), `cron/
  billing_reconcile.rs` (claim-then-call C1 + create/finalize-split C2 + 2-layer
  idempotency), `creator_billing` + `billing_runs` (`0040`). Webhook
  signature-verified (`stripe_handlers.rs::verify_stripe_signature`), handles
  `setup_intent.succeeded`, `invoice.payment_failed` (**audit only**),
  `invoice.paid`.
- **Providers:** `metering/provider/{native,stripe_meters,openmeter}.rs` +
  `cron/metering_export.rs` (`0043`).

**What is conspicuously absent (the gaps):**

1. **Stream 2 — Connect + application fee.** The `onboard` handler
   (`stripe_handlers.rs:90`) returns a *placeholder* `connect.stripe.com/
   express_login?...` URL (no real `account_links`). The `callback` blindly trusts
   a POSTed `acct_…` (no ownership verification). The `@zeroship/payments` SDK
   stamps `subscription_data[application_fee_percent]` from a **client-side**
   `applicationFeePercent ?? 15` (`sdks/payments/src/checkout.ts:80,98`) — fully
   bypassable. There is **no server-side `FeePolicy`** table or stamping path.
2. **Payment-failure → dunning → suspension.** `invoice.payment_failed` is
   audit-only ("we do NOT mutate billing state here"). The spend engine caps
   **usage**, never **payment**. A creator with a dead card accrues infra cost
   indefinitely; nothing transitions an app to a non-payment suspended state.
3. **Billing-ops lifecycle.** No credits/refunds/adjustments, no mid-period
   plan-change proration, no tax/multi-currency, and spend transitions are
   **audit-log-only** — no creator email at Warn/Block
   (`cron/spend_reconcile.rs::emit_transition`).
4. **Metering coverage.** Long-lived connections (WS not reachable through HTTP
   dispatch — `handler.rs:296`; SSE counted once at stream finalize); gateway/
   static-asset egress is in the gateway proxy, **never metered** (the worker owns
   the meter); `cpu_us` is sync-only (`thread_cpu_time` at dispatch end misses
   async/streaming CPU); db/kv byte metrics deferred (0-weight); the ledgers
   (`usage_reports_seen`, `spend_state_history`, and the export/billing histories)
   grow unbounded with **no pruning job**.
5. **Operational/observability.** No Stripe-reconciliation (drift) job, no
   billing-health alerting (export lag, reconcile failures, $0-revenue anomaly,
   `consecutive_failures` consumer), no creator-facing usage/bill/spend UX, and the
   operator `PUT /api/pricing-config` + `PUT /api/metric-weights` runtime endpoints
   are **not registered** (only `/api/plans*` + `/api/apps/:id/spend-limit` are —
   `main.rs:1250-1267`). FX/weights are seed-only today.
6. **Verification.** Provider smoke tests are mock-only (a mock once hid a
   $0-revenue bug); no real-API smoke against Stripe Meters/OpenMeter; full-stack
   provider e2e covers Native only; no load/scale test (hot-row UPSERT contention
   on `usage_aggregates` unmeasured — hardening backlog (b)); the metering_export
   integration suite is slow (250–643s observed); webhook event-id replay dedup is
   per-payout (`payouts.event_id UNIQUE`) but not a general inbound-event ledger.

---

## 1. Recommended sequence (executive view)

The two gaps to **lead with** are **G2 (payment-failure enforcement)** — the only
gap that is a *live financial risk* (uncollectable infra cost on a dead card, an
unbounded liability) — and **G1 (Stream 2 Connect + fee)** — the biggest *value*
gap (it is the platform's second revenue stream, entirely unbuilt, and the current
client-stamped fee is a *bypass vulnerability*, not just a missing feature).

The driving constraints on ordering:

- **Shared serialization point:** every DB-schema-changing gap shares the
  `control` crate + the PG `:5440` test DB. **Schema-changing work serializes**
  (changesets are linearly numbered + content-hashed; two in flight collide). Pure
  code / SDK / observability gaps parallelize freely.
- **Schema-redesign dependency:** G3 (credits/refunds/adjustments) and parts of
  G2's state model should **build on the redesigned invoice/credit tables** rather
  than inventing parallel ones. G1 (Connect) is **schema-independent** of the
  redesign (it extends the *Connect* side — `creator_accounts`/`payouts` — not the
  *infra-billing* tables the redesign touches), so it can start immediately.

```
NOW (independent of schema redesign)         BLOCKED on schema redesign
────────────────────────────────────         ──────────────────────────
G2  payment-failure → dunning → suspend  ──┐  G3a credits/refunds/adjustments
     (account_state on a NEW table;         │       (build on redesign's credit/
      gateway suspend gate)                 │        invoice model)
G1  Stream 2: Connect + FeePolicy + SDK     │  G3b mid-period proration
     (own epic; Connect-side schema)        │       (needs the invoice model)
G4c retention/pruning crons (no new schema) │
G5a operator pricing-config/weights endpts  │
G5b billing-health alerting + Stripe-recon  │
G6  smoke/load/e2e verification             │
                                            │
G3c spend-notification email (independent) ─┘
G4a/b WS/SSE/egress/cpu metering coverage (independent; runtime+gateway)
G3d  tax + multi-currency (LARGE; defer — see §7)
```

**Recommended landing order** (each is a reviewable slice; `⟂` = parallelizable):

1. **G2** — payment-failure enforcement. *Lead.* Real risk; needs one small new
   table (`app_billing_status`/account-state) + a gateway suspend gate that
   *composes* with the existing `SpendState`. Schema-light, high urgency.
2. **G1** — Stream 2 Connect + `FeePolicy` + SDK rewrite. *Lead, parallel to G2*
   (different files: Connect handlers + SDK vs. spend/gateway). Biggest value;
   closes the ISS-29 fee-bypass *security* hole + ISS-30 real onboarding.
3. **G5a** ⟂ **G3c** ⟂ **G4c** — quick independent wins: register the operator
   FX/weights endpoints; spend-notification email; the retention/pruning crons.
   None touch the redesigned schema; all can run alongside G1/G2.
4. **G5b** — billing-health alerting + Stripe drift reconciliation. Pure
   observability/read; no schema; after G2/G1 so it can alert on their states.
5. **G4a/b** — metering coverage (WS/SSE/egress/cpu). Independent (runtime +
   gateway), but lower financial urgency (under-billing is customer-favourable);
   sequence after the revenue-protecting gaps.
6. **G3a/b** — credits/refunds/adjustments + proration. **Gated on the schema
   redesign** (build on its credit/invoice model). Start design now; implement
   once the redesign lands.
7. **G6** — verification (real-API smoke, full-stack provider e2e, load test,
   metering_export slowness). Threaded *through* each gap as its TDD gate, plus a
   final standalone load/scale + real-API pass.
8. **G3d (tax + multi-currency)** — **DESCOPE to post-launch** (see §7).

---

## 2. Schema-redesign dependency matrix

| Gap | New tables it wants | Wait for redesign? | Why |
| --- | --- | --- | --- |
| **G1** Connect + fee | `creator_fee_policy`; extend `creator_accounts` (capabilities, verified) | **No** | Touches the Connect side (`creator_accounts`/`payouts`), not the infra-billing tables the redesign cleans. Coordinate naming only. |
| **G2** payment-failure | `app_billing_status` (account state: `active`/`past_due`/`suspended`) | **Soft** | Stands alone, but the *invoice* it reacts to may be redesigned. Build the state table independently; reference the redesign's invoice id as an FK if present. |
| **G3a** credits/refunds | `credit_ledger`, `adjustments` (or redesign-provided) | **YES** | The redesign "may add a credits/invoice model" — do NOT duplicate it. Build credits *on* it. |
| **G3b** proration | (reuses invoice/credit model) | **YES** | Proration writes credit/debit line items into the redesigned invoice model. |
| **G3c** spend email | none | **No** | Pure notification off existing `spend_state_history` transitions. |
| **G3d** tax/multi-currency | currency columns on invoice/line-item | **YES** (then DEFER) | Needs the redesigned invoice model AND is large — defer to post-launch. |
| **G4a/b** metering coverage | none (reuses `usage_aggregates` raw rows) | **No** | New metric names flow through `AppUsage.custom` — no DDL per metric. |
| **G4c** retention/pruning | none | **No** | DELETE-sweep crons over existing ledgers. |
| **G5a** operator endpoints | none (tables exist: `pricing_config`, `metric_weights`) | **No** | Only HTTP wiring is missing. |
| **G5b** alerting + drift recon | `stripe_recon_runs` (optional bookkeeping) | **No** | Read-mostly; one small optional bookkeeping table. |
| **G6** verification | none | **No** | Tests + harness only. |

**Rule of thumb:** if a gap writes **money records** (credits, refunds, proration,
tax, invoice line items) it should land on the **redesigned invoice/credit model**
and wait. If it writes **operational state** (account status, metrics,
notifications, retention) it is independent.

---

## 3. Per-gap design

Every gap below carries the same invariants as the shipped epic: **app_id /
creator_id server-injected** (never client-set), **operator-only money controls**
(Cedar `Resource::Any` + master-key), **reduction-only / guardrailed creator
controls**, **RLS fail-closed** on app-keyed tables, **idempotent money** (no blind
re-POST, no silent clamp), **zero tokio** (compio intervals; `cyper` HTTP;
`compio-postgres`). Each gap names a **faithful TDD plan** (real path, no shims —
`feedback_faithful_e2e_tests`) and a **regression test** per fix
(`feedback_regression_test_per_fix`).

---

### G1 — Stream 2: Connect onboarding + server-stamped application fee

*The biggest value gap. A separate creator-payments epic; design standalone.*

**Goal.** Creators charge *their* end-users via Stripe **Connect** (their own
Stripe account, OAuth-linked). The platform takes a **server-controlled,
per-creator `FeePolicy`** (default `Percent{15%}`), stamped on the Connect charge
server-side so creator code cannot set or bypass it (ISS-29 done right). Replace
the placeholder onboarding with real `account_links` + ownership verification
(ISS-30). Rewrite `@zeroship/payments`.

**New schema (Connect side — NOT the redesign's infra tables):**

```sql
-- 0044_creator_fee_policy.sql
CREATE TABLE zeroship.creator_fee_policy (
    creator_id   UUID PRIMARY KEY REFERENCES zeroship.users(id) ON DELETE CASCADE,
    kind         TEXT   NOT NULL CHECK (kind IN ('fixed','percent')),
    amount_cents BIGINT,                         -- kind='fixed'
    percent_bps  INT,                            -- kind='percent', basis points (1500 = 15%)
    cap_cents    BIGINT,                         -- optional
    floor_cents  BIGINT,                         -- optional
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CHECK ( (kind='fixed'   AND amount_cents IS NOT NULL)
         OR (kind='percent' AND percent_bps  BETWEEN 0 AND 10000) )
);
-- GRANT SELECT,INSERT,UPDATE TO zeroship_control (DO-block, 0037 pattern). No RLS:
-- operator-config keyed by creator_id; control is BYPASSRLS.

-- Extend creator_accounts with the verification signal (ALTER acceptable
-- pre-launch — these are not the redesign's tables):
ALTER TABLE zeroship.creator_accounts
    ADD COLUMN charges_enabled  BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN payouts_enabled  BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN details_submitted BOOLEAN NOT NULL DEFAULT false;
```

**New code:**

- **`stripe_client.rs`** — add `StripeApi` verbs: `create_account_link(acct,
  refresh_url, return_url)` (POST `/v1/account_links`), `create_connect_account(
  email, country)` (POST `/v1/accounts`, type=`standard`/`express` per product
  call), `retrieve_account(acct)` (GET `/v1/accounts/:id` → `charges_enabled` etc.).
- **`stripe_handlers.rs::onboard`** — REPLACE the placeholder: create (or reuse) a
  Connect account for the creator, persist `acct_…`, return a real
  `account_links` URL. **Bind `:id` to the principal** (self-service, like
  `billing_setup`) or operator BillingWrite.
- **`stripe_handlers.rs::callback`** — REPLACE the trust-the-POST logic with a
  server-side `retrieve_account(acct)`: verify the `acct_…` exists, is enabled,
  and was minted for THIS creator (match against the stored pending acct or the
  `metadata.creator_id` we stamped at create). Reject a foreign `acct_…`.
  Alternatively drive the whole flow server-side (we already hold the `acct_…`
  from `onboard`) and reduce `callback` to a "refresh status" re-`retrieve`.
- **NEW `fee_policy.rs`** — `enum FeePolicy { Fixed{amount_cents}, Percent{
  bps, cap, floor} }`; pure `fn fee_cents(&self, txn_cents: u64) -> u64`
  (`Percent` → `clamp(round(txn × bps / 10000), floor, cap)`). `FeePolicyStore`
  (PG): `get(creator) -> FeePolicy` (default `Percent{1500}` when no row),
  `set(creator, policy)` (**operator-only** — creators cannot edit their own fee).
- **Server-stamped charge path.** The fee MUST be applied where the platform
  controls it, not in the SDK. Two options:
  - **(Recommended) A control endpoint** `POST /api/creators/:id/connect/checkout`
    that builds the Stripe Checkout/PaymentIntent **server-side** with
    `application_fee_amount` (or `application_fee_percent`) computed from the
    server-stored `FeePolicy` and `transfer_data[destination]=acct_…`. The SDK
    calls this endpoint; it can no longer name the fee.
  - (Rejected) keeping the SDK stamping but signing the fee — still client-trusted.
- **`@zeroship/payments` SDK rewrite** (`sdks/payments/src/checkout.ts`) — DELETE
  `applicationFeePercent` from the public options; the SDK posts the *business*
  params (amount, currency, end-user) to the control endpoint and receives a
  session URL. No fee field crosses the wire from creator code. Update
  `webhook.ts` only if the event shape changes (it does not — `invoice.paid` +
  `application_fee_amount` already handled).

**Enforcement / idempotency / security:**

- **Fee is server-held + server-stamped** — the SDK cannot read or set it. This is
  the ISS-29 fix: the bypass was the *client default*, not the fee.
- **`set(creator, policy)` is operator-only** (Cedar `BillingWrite` /
  `Resource::Any`); a creator self-editing their fee is a privilege escalation.
- **Onboarding ownership verification** closes ISS-30: a creator cannot bind a
  Connect account they don't control (server `retrieve_account` + creator match).
- **Idempotency:** the checkout-create call carries a deterministic
  `Idempotency-Key` per `(creator, end-user-cart)`; the existing `payouts.event_id
  UNIQUE` keeps `invoice.paid` ingestion idempotent.

**Test plan (TDD, faithful):**

- Unit: `fee_cents` table-driven (fixed; percent with/without cap/floor;
  rounding-half-up; `bps=0` → 0; default policy = 15%). Regression
  `percent_fee_clamped_to_cap`, `fixed_fee_ignores_txn_size`.
- Integration (PG + localhost mock-Stripe, real `cyper`): `onboard_returns_real_
  account_link`; `callback_rejects_acct_not_owned_by_creator`;
  `checkout_stamps_server_fee_not_client_value` (POST a *malicious* fee in the body
  → assert the Stripe call carries the SERVER policy's fee, ignoring the body);
  `fee_policy_set_is_operator_only` (creator principal → 403).
- SDK (vitest, cross-validated against the Rust HMAC fixture like
  `webhook.test.ts`): the SDK has no fee field; the posted body has no
  `application_fee*`.

**Effort:** L (own epic — Connect verbs, fee model, server checkout, SDK rewrite,
mock-Stripe Connect leg). **Risk:** Medium-high (money movement on a new rail +
ownership-verification security). **Parallel:** with G2 (disjoint files).

---

### G2 — Payment-failure → dunning → suspension  *(LEAD — real risk)*

**Goal.** Give an app/creator a **billing/account state** distinct from spend
state, drive a failed-payment lifecycle (Stripe-retry-aware dunning), and add a
gateway **suspend gate** that stops serving an app on non-payment. It must
**compose** with the existing `SpendState` enforcement, not replace it.

**The composition model (the key design decision).** `SpendState` (Allow/Warn/
Degrade/Block) is a **usage cap within a paid relationship**. Payment status is an
**account-level gate** orthogonal to it. They compose as an **AND at the gateway**:
a request is served iff `account_state ∈ {active, past_due}` **AND**
`spend_state ≠ Block`. `suspended` is a hard stop independent of spend. We add a
second field to `RouteEntry`, NOT a new value on `SpendState` (keeping the two
concerns separable + independently testable).

```
account_state:  active ──fail──► past_due ──dunning exhausted──► suspended
                   ▲                  │                              │
                   └──── paid ────────┴──────── paid ───────────────┘ (reactivate)
```

- **active** — current. Served (subject to spend).
- **past_due** — ≥1 invoice failed; Stripe's retries running; dunning emails sent;
  **still served** (grace), possibly with a warning header. This is the
  customer-favourable grace window.
- **suspended** — dunning exhausted (Stripe `invoice` marked uncollectible / final
  retry failed, or a configurable `max_dunning_days` elapsed). Gateway returns
  **402 `ACCOUNT_SUSPENDED`** before dispatch. Cardless/free apps never enter this
  (no invoice → no failure).

**New schema (independent; reference the redesign's invoice id if present):**

```sql
-- 0045_app_billing_status.sql  (keyed by creator_id — the billing relationship is
-- per-creator, but the gate is enforced per-app via the owner join)
CREATE TABLE zeroship.creator_billing_status (
    creator_id        UUID PRIMARY KEY REFERENCES zeroship.users(id) ON DELETE CASCADE,
    state             TEXT NOT NULL DEFAULT 'active'
                          CHECK (state IN ('active','past_due','suspended')),
    first_failed_at   TIMESTAMPTZ,           -- start of the current dunning window
    failed_invoice_id TEXT,                  -- the Stripe in_… (or redesign invoice FK)
    dunning_attempts  INT NOT NULL DEFAULT 0,
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
-- GRANT SELECT,INSERT,UPDATE; no app-RLS (creator-keyed, control BYPASSRLS).
```

**New code:**

- **`stripe_handlers.rs::handle_invoice_payment_failed`** — STOP being audit-only.
  Resolve the creator (already does, via metadata or `cus_…` reverse-resolve),
  then **transition** `creator_billing_status`: `active → past_due` on first
  failure (set `first_failed_at`), increment `dunning_attempts`. Audit as today.
- **NEW webhook events:** `invoice.paid` / `invoice.payment_succeeded` for an
  *infra* invoice → transition back to `active` (clear the dunning window). Today
  `invoice.paid` only records Connect payouts; branch on whether the invoice is an
  infra-billing invoice (its `cus_…` is the platform customer / `metadata` marks
  it) vs a Connect payout.
- **NEW `cron/dunning.rs`** (~daily compio interval) — for each `past_due`
  creator: if `now - first_failed_at > max_dunning_days` (config, default ~7)
  OR Stripe reports the invoice uncollectible → transition `past_due → suspended`,
  audit + (G3c) email. Optionally mark the Stripe invoice uncollectible. Advisory-
  locked like the other sweeps; distinct lock key.
- **`registry.rs::get_routes`** — JOIN `creator_billing_status` (via the
  `app_members owner` join already used for billing) → `RouteEntry.account_state`
  (new `#[serde(default)]` field; update gateway/worker fixtures).
- **`gateway/src/sync.rs`** — thread `account_state` onto `CompiledRoute`.
- **`gateway/src/enforce.rs`** — add `check_account(state) -> Result<(),
  HttpResponse>`: `suspended` → `402 ACCOUNT_SUSPENDED`; else `Ok`. Called in
  `dispatch.rs::handle_dispatch` + `handle_subscription_dispatch` **before**
  `check_spend` (account gate is the outer AND).
- **Creator self-reactivation:** updating the card (`billing/setup` →
  `setup_intent.succeeded`) does NOT auto-clear `suspended`; a successful invoice
  payment does. Offer an operator/creator "retry now" that re-attempts the open
  invoice via Stripe; on success the webhook reactivates.

**Enforcement / idempotency / security:**

- The account gate is **server-derived from Stripe webhook truth** — no creator
  input sets it. Suspension is reversible only by payment (or operator override).
- **In-flight requests** are not torn down (same rationale as the spend Block —
  the gate runs at the top of dispatch; live streams complete).
- **Idempotency:** webhook transitions are idempotent (re-delivered
  `payment_failed` does not double-increment if the `failed_invoice_id` is
  unchanged — guard on `(creator, failed_invoice_id)`). The general
  **webhook-event-id dedup ledger** (G6/§6) backs this.
- **Free/cardless apps** never have an infra invoice → never `past_due`/`suspended`
  — verified by construction (no `billing_runs` charge for `spend_limit=0` plans).

**Test plan (TDD, faithful):**

- Unit: the `active→past_due→suspended→active` transition table; `dunning`
  threshold logic; `check_account` pure match.
- Integration (PG + mock-Stripe): `payment_failed_moves_to_past_due`;
  `repeated_failure_is_idempotent`; `dunning_exhaustion_suspends`;
  `payment_success_reactivates`.
- **Faithful gateway e2e:** feed `RouteEntry{account_state:suspended}` through the
  REAL `RouteCache::update`, drive a request through real `handle_dispatch`, assert
  **402 ACCOUNT_SUSPENDED before any worker proxy** (fails today). Assert
  `past_due` still serves (grace). Assert account-gate AND spend-gate compose
  (suspended beats Allow; Block beats active).

**Effort:** M. **Risk:** High (it can take an app *offline* — false-suspend is
catastrophic; the grace window + reversibility + webhook-truth-only design
mitigate). **Parallel:** with G1.

---

### G3 — Billing-ops lifecycle

Four sub-gaps with very different dependencies and sizes.

#### G3a — Credits / refunds / adjustments  *(GATED on schema redesign)*

**Do not invent tables.** The parallel redesign "may add a credits/invoice model."
This gap **builds on it**: a `credit_ledger` (signed entries: grants, refunds,
adjustments) that the reconciler nets against the period charge before invoicing.

- If the redesign provides a credit/invoice model → consume it. If not → propose
  `credit_ledger(id, creator_id, cents_signed, reason, created_by, idempotency_key
  UNIQUE, created_at)` as part of *this* gap, coordinated with the redesign author.
- **Reconciler change (`billing_reconcile.rs` / `NativeProvider::invoice`):** after
  `charge_cents`, apply outstanding credits (oldest-first), write a negative
  invoice line / Stripe credit; never let a credit silently zero a charge without a
  ledger entry (auditability).
- **Refund:** an operator action → Stripe refund + a `credit_ledger` debit-back if
  the refund offsets a future charge.
- **Security:** credits/refunds/adjustments are **operator-only** (`Resource::Any`,
  master-key). Every entry carries `created_by` + an idempotency key (no
  double-credit). No client path.

**Effort:** M (mostly on the redesign's model). **Risk:** Medium (money; auditable).
**Blocked:** YES — design now, implement after the redesign lands.

#### G3b — Mid-period plan-change proration  *(GATED on schema redesign)*

When a creator changes plan mid-period, prorate: charge the old plan for the
elapsed fraction + the new plan for the remainder (or credit the difference).
Writes proration line items into the redesigned invoice model.

- **`set_plan`** records a `plan_change_events(app_id, from_plan, to_plan, at)` row
  (timeline). The reconciler integrates usage across plan segments, not one flat
  plan. **Design decision needed (operator):** prorate the *base_fee* only (simple)
  vs. re-segment *usage pricing* at the change boundary (faithful but needs
  per-segment usage). Recommend base-fee proration in v1; segment usage post-launch.
- **Blocked:** YES (proration is an invoice-model concern). **Effort:** M.
  **Risk:** Medium.

#### G3c — Spend-limit + payment notifications (email)  *(INDEPENDENT)*

Today spend transitions are **audit-log-only** (`spend_reconcile.rs::
emit_transition`). Wire an **email at Warn and Block** (and dunning emails for G2).

- **NEW `notify.rs`** — a `BillingNotifier` that, on a transition, sends via the
  existing platform email path (`@zeroship/email` / the auth relay-email
  infrastructure already in the tree). `emit_transition` calls it for
  `Allow→Warn`, `→Degrade`, `→Block`; `dunning.rs` calls it for `past_due` /
  `suspended`.
- **Idempotency:** one email per transition (the transition is already
  dedup-once-per-state-change). Rate-limit per creator (no email storm if an app
  flaps near a boundary — the deadband already prevents flapping, but cap anyway).
- **No new money table.** A small `notification_log(creator_id, kind, at)` for
  dedup/rate-limit is optional.

**Effort:** S. **Risk:** Low. **Independent** — land early alongside G2/G1.

#### G3d — Tax (Stripe Tax) + multi-currency  *(DESCOPE — see §7)*

Large, gated on the redesigned invoice model (currency + tax columns), and not
launch-blocking for a USD-first launch. **Deferred to post-launch.**

---

### G4 — Metering coverage

#### G4a — Long-lived connections (WS / SSE / streaming)  *(INDEPENDENT)*

Today: `record_request` finalises `wall_us`/`egress_bytes` once at dispatch end;
SSE/streaming responses meter egress at stream finalize (`metering_on_complete`,
`handler.rs:357`); **WS upgrades are not reachable through HTTP dispatch**
(`handler.rs:296` returns 500). A long-lived connection therefore under-counts
wall time and (for WS) is unmetered.

- **SSE/streaming:** already finalises at stream end — but a connection open for
  hours bills nothing until it closes (and is lost on crash — backlog (a)). **Add
  periodic incremental metering**: the streaming drain task flushes
  `wall_us`/`egress_bytes` deltas on an interval (e.g. every N seconds or M bytes),
  not only at finalize, so a long stream accrues continuously and a crash loses
  only the last delta.
- **WebSocket:** when WS lands as a first-class worker transport (separate epic),
  meter `wall_us` for connection lifetime + `egress_bytes`/`ingress_bytes` per
  frame, incrementally. Until then, document WS-over-dispatch as unsupported (it
  already errors, so no silent under-bill).
- **`cpu_us` sync-only:** `thread_cpu_time` at dispatch end misses CPU burned in
  async continuations / streaming callbacks. Capture CPU at each metering flush
  (delta since last capture) rather than once.

**Effort:** M (runtime + worker streaming path). **Risk:** Low (under-billing is
customer-favourable; not a financial-loss risk). **Independent.**

#### G4b — Gateway / static-asset egress  *(INDEPENDENT)*

Static assets + the asset proxy are served by the **gateway**, which **owns no
meter** (the meter lives in the worker). Asset egress is wholly unmetered.

- **Design:** the gateway accumulates per-app egress bytes for asset/proxy
  responses and POSTs them to control's `/internal/usage` as a `UsageReport` with a
  synthetic `worker_id` (`gate-<id>`) + its own sequence — reusing the **exact
  idempotent ingest path** (`(worker_id, sequence)` dedup) so no new sink is built.
  New metric `asset_egress_bytes` flows through `AppUsage.custom`.
- **Security:** the gateway already knows the authenticated `app_id` per route
  (server-side); no client input. Same server-injected-app_id invariant.

**Effort:** M (gateway producer + report plumbing). **Risk:** Low. **Independent.**

#### G4c — Retention / pruning of unbounded ledgers  *(INDEPENDENT)*

`usage_reports_seen` (grows per worker boot-epoch — backlog (c)),
`spend_state_history`, and the export/billing history grow without bound.

- **NEW `cron/billing_retention.rs`** — modelled on the existing
  `cron/audit_retention.rs` (the direct template): DELETE rows past a
  dedup-relevant / retention window. `usage_reports_seen` can prune anything older
  than the max plausible worker-report lag (e.g. a few hours) — it only guards
  short-window replay. `spend_state_history` keeps a longer audit window (months,
  like `audit_events`). Advisory-locked; distinct key.
- **Care:** never prune `usage_aggregates` for an unbilled period (a `billing_runs`
  row with `stripe_invoice_id IS NULL` means that period is still re-drivable).
  Pruning keys off "period billed + retention elapsed".

**Effort:** S. **Risk:** Low-medium (a too-aggressive prune of `usage_reports_seen`
could re-open the double-count window — the window choice is the load-bearing
decision). **Independent.**

---

### G5 — Operational / observability

#### G5a — Operator runtime FX / weights endpoints  *(INDEPENDENT)*

The tables exist (`pricing_config`, `metric_weights`) and `pricing_store.rs` reads
them, but the **`PUT /api/pricing-config` + `PUT /api/metric-weights` (and GETs)
are not registered** — FX/weights are seed-only. Without these, re-pricing the
fleet needs a redeploy.

- **`api.rs` + `main.rs` route registration:** `GET/PUT /api/pricing-config`
  (global FX) and `GET/PUT /api/metric-weights` (the cost model), **operator-only**
  (master-key / `BillingWrite` on `Resource::Any`), symmetric with `/api/plans`.
- **`pricing_store.rs`:** add the `set`/`upsert` methods (reads exist). Enforce the
  **near-zero FX floor** on write (`CHECK fx >= MIN_FX` already in the schema;
  mirror it in Rust — no silent $0 pricing).
- **Security:** these are the platform's price levers — the most sensitive money
  controls. Operator-only, audited (write an audit row on every change).

**Effort:** S. **Risk:** Low (but high-blast-radius — a bad FX re-prices the whole
fleet; the floor + audit + operator-gate are the guardrails). **Independent — land
early.**

#### G5b — Billing-health alerting + Stripe drift reconciliation  *(INDEPENDENT)*

- **Billing-health alerting:** a `cron/billing_health.rs` (or metrics + alert
  rules) that flags: export-sweep lag (`metering_export` high-water stalled),
  reconcile failures (`billing_runs` rows stuck `stripe_invoice_id IS NULL` past N
  ticks), a **$0-revenue anomaly** (a billing period closed with total invoiced
  ≈ 0 across an active fleet — the exact class a mock once hid), and a
  **`consecutive_failures` consumer** (the export-failure counter the
  `metering_export` cron records — `record_export_failure` exists; nothing alerts
  on it). Emit as structured logs / metrics the platform's observability picks up.
- **Stripe drift reconciliation:** a `cron/stripe_recon.rs` (periodic) that lists
  Stripe invoices/charges for the closed period and compares against
  `billing_runs` — flags invoices Stripe has that we don't (and vice-versa),
  amount mismatches, and orphaned customers. **Read-only / alert-only in v1** (never
  auto-mutate money on drift — surface for an operator). Optional
  `stripe_recon_runs` bookkeeping table.
- **Security:** read-only; operator-surfaced. No money mutation.

**Effort:** M. **Risk:** Low. **Independent** — after G2/G1 so it can watch their
states.

#### G5c — Creator-facing usage / bill / spend UX

Creator-facing read endpoints (+ console UI) for current-period usage, projected
bill, spend-limit + state, and invoice history. Read-only; bounded by app
ownership (the existing `app_owner` authz, RLS-scoped). Pairs with the redesign's
invoice model for bill/invoice history (**soft-gated**: usage + spend are
available now; invoice history wants the redesigned model). **Effort:** M (UI +
endpoints). **Risk:** Low.

---

### G6 — Verification

Threaded *through* each gap as its TDD gate, plus standalone passes:

- **Real-API smoke (not mocks).** A gated (`STRIPE_TEST_KEY` / `OPENMETER_URL`
  present) smoke suite that drives the **real** Stripe Meters + OpenMeter APIs in
  test mode — because a mock once hid a $0-revenue bug. Asserts a real meter_event
  / CloudEvent lands and a real invoice is produced. Run in CI behind a secret
  guard; skipped offline.
- **Full-stack provider e2e.** Extend `tests/e2e_metering_billing.sh` with the
  `METERING_PROVIDER` matrix (M8 already specifies native + stripe + openmeter legs
  over the real multi-node stack). Native is covered; build out the stripe +
  openmeter legs end-to-end.
- **Load / scale test.** A harness that drives N workers UPSERTing the same
  `(app_id, period_start, metric)` rows to **measure hot-row contention** (backlog
  (b)) — currently unmeasured. Per `feedback_never_estimate`, this is a
  *measurement*, not an estimate: only after measuring do we decide whether to shard
  to per-worker append-only rows + SUM-on-read.
- **metering_export slowness (250–643s).** Investigate the integration-test
  latency — likely advisory-lock contention across serialized sweeps or per-test DB
  fixture cost on `:5440`. Profile, then either parallelise independent cases,
  share a fixture, or shorten the tick under test. (Diagnosis-first per
  `superpowers:systematic-debugging` — do not "fix" before measuring the cause.)
- **Webhook event-id replay dedup.** Today dedup is per-`payouts.event_id UNIQUE`
  (Connect payouts only). Add a **general inbound `stripe_events_seen(event_id PK,
  received_at)` ledger** so EVERY event type (setup_intent, payment_failed,
  payment_succeeded, paid) is replay-safe — the first handler checks-and-inserts;
  a re-delivered event is a no-op. Backs G2's idempotent transitions.

**Effort:** M (spread across gaps) + S (the dedup ledger). **Risk:** Low.

---

## 4. Sequencing, parallelism, and the serialization constraint

All schema-changing gaps share `control` + the PG `:5440` test DB → **their
changesets serialize** (linear numbering + content hashes; integration tests on
`:5440` can't run two new schemas concurrently). Pure-code / SDK / runtime /
observability gaps parallelize.

```
Wave 1 (lead, parallel):
  G2  payment-failure  (changeset 0045)  ─┐ different files
  G1  Connect + fee    (changeset 0044)  ─┘ (spend/gateway vs connect/SDK)
        └─ changesets 0044/0045 land sequentially (serialize the DDL),
           but the Rust/SDK code is developed in parallel.

Wave 2 (independent quick wins, ⟂ each other and Wave 1's code phase):
  G5a  operator FX/weights endpoints   (no schema)
  G3c  spend/payment notification email (no schema)
  G4c  retention/pruning crons          (no schema)
  G6   webhook event-id dedup ledger    (changeset 0046 — small)

Wave 3:
  G5b  billing-health alerting + Stripe drift recon (no schema)
  G4a/b metering coverage (WS/SSE/egress/cpu)        (no schema; runtime+gateway)

Wave 4 (gated on schema redesign):
  G3a  credits/refunds/adjustments   (on redesign's credit model)
  G3b  mid-period proration          (on redesign's invoice model)
  G5c  creator bill/invoice UX        (invoice history on redesign)

Post-launch (descoped):
  G3d  tax + multi-currency
```

**Changeset budget:** `0044` (fee_policy + creator_accounts cols, G1), `0045`
(creator_billing_status, G2), `0046` (stripe_events_seen, G6). G3a/b/d consume the
redesign's changesets (do not pre-allocate). Each lands in its own PR with the DDL
+ the code + the TDD gate.

---

## 5. Effort + risk summary

| Gap | Effort | Risk | Schema-redesign dep | Parallelizable |
| --- | --- | --- | --- | --- |
| **G2** payment-failure → suspend | M | **High** (can take apps offline) | Soft | with G1 |
| **G1** Connect + server fee + SDK | L | **High** (money on a new rail + ownership verify) | No | with G2 |
| **G3c** spend/payment email | S | Low | No | yes |
| **G4c** retention/pruning | S | Low-med (prune window) | No | yes |
| **G5a** operator FX/weights endpts | S | Low (high blast radius) | No | yes |
| **G6** event-id dedup + smoke/load | M | Low | No | yes |
| **G5b** alerting + drift recon | M | Low | No | yes |
| **G4a/b** WS/SSE/egress/cpu metering | M | Low (under-bill = favourable) | No | yes |
| **G3a** credits/refunds | M | Med (money, auditable) | **YES** | after redesign |
| **G3b** proration | M | Med | **YES** | after redesign |
| **G5c** creator bill/invoice UX | M | Low | Soft (invoice history) | after redesign |
| **G3d** tax + multi-currency | L | Med | **YES** | **DESCOPE** |

---

## 6. The 3 highest-risk / highest-value gaps

1. **G2 — payment-failure enforcement (highest *risk*).** The only **live
   financial liability**: today a creator with a dead card accrues unbounded infra
   cost (the spend engine caps usage, not payment). It is also the highest
   *operational* risk to *implement* (a false-suspend takes a creator's app
   offline) — which is why the design is grace-windowed (`past_due` keeps serving),
   webhook-truth-only (no creator input), and reversible. Lead with it.
2. **G1 — Stream 2 Connect + server-stamped fee (highest *value* + a *security*
   fix).** It is the platform's entire second revenue stream, unbuilt, AND the
   current client-stamped `applicationFeePercent ?? 15` is a **bypass
   vulnerability** — any creator can set their own fee to 0. Server-holding +
   server-stamping the fee closes ISS-29; real `account_links` + ownership
   verification closes ISS-30.
3. **G5a + G5b — operator FX endpoints + billing-health alerting (highest
   *silent-failure* risk).** FX/weights are seed-only (no runtime re-price), and
   nothing watches for the **$0-revenue anomaly** — exactly the failure class a
   mock once hid. A billing system that silently invoices $0 across the fleet and
   nobody is paged is the worst outcome; G5b is cheap insurance and G5a removes the
   redeploy-to-reprice footgun.

---

## 7. What to DESCOPE / defer to post-launch

- **G3d — Tax (Stripe Tax) + multi-currency.** Large, gated on the redesigned
  invoice model's currency/tax columns, and **not launch-blocking** for a USD-first
  launch. Stripe Tax can be enabled later with a focused epic. Defer.
- **G3b — usage-segment proration** (the *faithful* variant). Ship **base-fee
  proration** in v1; defer per-segment usage re-pricing (needs per-segment usage
  integration) to post-launch.
- **G4a — WebSocket metering.** WS is not a first-class worker transport yet
  (errors over HTTP dispatch). Meter it when WS lands; until then it cannot
  silently under-bill (it errors). The SSE/streaming incremental-metering piece of
  G4a is worth doing now; the WS piece waits on the WS transport epic.
- **Hardening backlog (a) crash-loss durability** (`feedback`-noted) and **(b)
  hot-row sharding** stay deferred until a durability SLA / a *measured* contention
  problem exists (do not pre-optimise — `feedback_never_estimate`).

---

## 8. Invariants checklist (applied to every gap)

- **Server-injected identity** — `app_id` (worker/gateway) and `creator_id`
  (control) are derived server-side at every emit/charge; no JS/SDK path sets them.
- **Operator-only money controls** — fee policy, FX, weights, credits, refunds,
  adjustments, plan upserts, suspension overrides → Cedar `Resource::Any` /
  master-key, audited.
- **Reduction-only / guardrailed creator controls** — spend-limit override is
  reduction-only (plan default = ceiling); plan self-select bounded to
  `assignable_by_creator` tiers. No creator self-edit of fee or status.
- **RLS fail-closed** — app-keyed billing tables ENABLE+FORCE RLS with the 0037
  tenant-isolation pattern; control is BYPASSRLS for fleet sweeps; creator-keyed
  config tables are operator-only (no app RLS).
- **Idempotent money** — claim-then-call + deterministic Stripe `Idempotency-Key`;
  webhook event-id dedup ledger; no blind re-POST; no silent money clamp (validate
  + fail-closed, never clamp-and-continue).
- **Zero tokio** — every new cron is a `compio::time` interval; every HTTP client
  is `cyper`; all storage is `compio-postgres`.
- **Faithful TDD + a regression test per fix** — real path (live runtime /
  dispatcher / gateway / mock-or-real Stripe over `cyper`), never a shim; every fix
  ships a test that would fail pre-fix.

---

## 9. Open questions for the operator

1. **Dunning window** — `max_dunning_days` default (7?) and whether to mark the
   Stripe invoice uncollectible at suspension or leave Stripe's own retry running.
2. **Connect account type** — Standard vs Express (affects onboarding UX +
   `account_links` shape + who owns dispute liability).
3. **Proration** — base-fee-only (v1, recommended) vs full usage-segment proration.
4. **Credits model** — confirm the schema redesign provides the credit/invoice
   model G3a/b build on, or whether `credit_ledger` belongs to this epic.
5. **$0-revenue alert threshold** — what fleet size / active-app count makes a
   $0-invoiced period an anomaly worth paging on.
6. **Tax/multi-currency** — confirmed deferred to post-launch?
