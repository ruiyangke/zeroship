# Billing-Ops Lifecycle Design (gap #26)

**Status: DECIDED — implementing.** All twelve PRODUCT DECISIONS are ratified (D1 implement-now, FULL usage-segment proration, tax seam computing 0, Connect = Express, plus the accepted defaults); the design is greenlit and PR-0 … PR-8 are cleared to build. Pre-launch clean extension of the billing changesets (`0042` reshape + `0048`–`0054`). Not a live migration. No back-compat. Provider-agnostic, zero-tokio (compio/cyper).
**Revision note (round 4):** proration hardened — segment-aware idempotency + reconcile-loop rewrite, half-open day-partition, delta floor, cap-collapse fix, plan_id NOT NULL. Closes the round-4 proration critic (64/100): **CRITICAL-1** — the `0054` PK widening to `(invoice_id, app_id, segment_no)` was breaking every per-app guard in `billing_reconcile.rs` (segment-blind Stripe `Idempotency-Key` → N segments collide to ONE item → silent under-bill); now the item key, the `posted`/`line_exists` guards, the line UPSERT `ON CONFLICT`, and the provider-ref INSERT are all segment-keyed, with a `BilledSegment` reconcile-loop SKELETON added to PR-4. **CRITICAL-2** — pinned the half-open calendar-day partition rule (no shared boundary day, `Σ segment_days = days_in_period` exactly), re-derived the worked example's day-split + amounts FROM that rule, stated the base-fee invariant + deterministic remainder-cents rule. **MAJOR-3** — usage-delta floored at `max(0, end−start)` (END-missing-metric defended). **MAJOR-4** — past the change cap, the tail prices under the actually-running `apps.plan_id`, not the last recorded (cheap) plan — a real revenue leak now that FX varies per plan. **MAJOR-5** — `invoice_lines.plan_id` is `NOT NULL` (no legacy pre-launch). **MINOR-6** — zero-day segments merge into their neighbour (no quota-starved over-bill spike). **MINOR-7** — `0042`'s line-provider-ref composite FK is explicitly named so `0054` drops a known name. The four MISSING items are closed: overflow surface unchanged by segmentation, per-segment Stripe description + item-count growth, concurrent `set_plan`-vs-reconcile race rule. Item 8 (spend-enforcement decoupling) verified clean and unchanged. N=0 degeneration stays byte-identical-behaviour to today.
**Revision note (round 3):** proration upgraded to full usage-segment per user decision; decisions stamped. The user OVERRODE the round-1/2 recommendation of base-fee-only proration: v1 now does **full usage-segment proration** — at every plan change the app's cumulative `usage_aggregates` totals are snapshotted onto the `plan_change_events` row (`usage_at_change` JSONB), and at month-end the period splits into N+1 segments whose usage is the cumulative *delta* between consecutive snapshots, each priced under its own plan and frozen as a SEPARATE invoice line. This required a **line-grain reshape** of `invoice_lines` (a new `segment_no` discriminator in the PK, a new changeset `0054`) and pulls usage-segment proration from *deferred* to *build-now* (PR-4). The twelve decisions are stamped DECIDED/ACCEPTED in the PRODUCT DECISIONS section. All round-1/round-2 money-correctness fixes (cash-collected as `Σ(invoice_payments)`, `void_reversal` conservation, claim-before-send, server-derived proration money, Refund-vs-credit-note split) are preserved.
**Revision note (round 1):** Closed five money-correctness CRITICALs — (1) over-refund cap is now anchored on cash actually collected, not the credit-inflated `total_cents`, killing the credit-laundering path; (2) void+reissue now appends a `void_reversal` ledger entry so consumed credit is conserved across a void; (3) the notify cron now claims-BEFORE-send via a two-phase `pending→sent` row under the dunning advisory lock, fixing the multi-node double-send; (4) `plan_change_events` proration money fields are asserted server-derived from the catalog (never client-supplied) with a per-period change cap; (5) the refund mechanism is split to use the **correct Stripe object** — a `Refund` (`re_…`) on the charge/PaymentIntent for cash-back on a paid invoice (a credit note is only an optional bookkeeping wrapper), and a platform-native ledger grant (or `credit_amount` customer-balance transaction) for credit-back. Plus all MAJORs, MINORs, and seven MISSING-CONCEPT sections.
**Revision note (round 2):** Closed the round-2 confirming critic's residual findings. **CRITICAL-A** (the headline fix was mechanically impossible as written): the round-1 plan wrote `cash_collected_cents` onto an *already-finalized* `invoices` row from the payment webhook — but `invoices_immutable()` RAISEs on **every** finalized→finalized UPDATE (only `→void` with money held equal is legal), so the write failed every time. **FIXED by moving payment tracking OFF `invoices` into a new append-only side table `invoice_payments`** (`0053`, FK→`invoices(id)`, one row per payment/partial-pay/dispute-clawback); cash-collected is now `Σ(invoice_payments)`, computed, never a column on the frozen invoice — sidestepping the trigger entirely and matching the doc's own "money records are append-only side facts, never mutate a finalized invoice" principle. The over-refund trigger, dispute check, true-up bridge, and worked example all now read cash-collected from this source. **CRITICAL-B** (true-up double-refund): the bridge now subtracts cash refunds *already issued* on the voided invoice before computing the over-collection, floored at 0. **MAJOR-A**: downgraded "exactly-once effect across nodes" to "at-least-once delivery, exactly-once claim," with a provider-side `Idempotency-Key` on `Mailer::send` making the delivery effect idempotent. **MAJOR-B**: pinned `NOTIFY_REDRIVE_HORIZON = 15min`. **MINOR-A**: `refunds` and `credit_ledger` idempotency keys now store + compare a request-body fingerprint (reject reuse-with-different-body, mirroring Stripe's 400). **MINOR-B**: softened the dead `ON DELETE RESTRICT` comment. The 4 genuinely-closed round-1 CRITICALs (void_reversal conservation; claim-before-send + advisory lock; server-derived proration + cap; Refund-vs-credit-note split) and per-grant consume/expiry are preserved.
**Date:** 2026-06-14
**Branch / worktree:** `feat/billing-metering` · `/home/ruiyang/Projects/appbase-billing`
**Depends on:** `docs/proposals/2026-06-14-billing-schema-redesign.md` (the `0037`–`0043`/`0047` consolidation). This proposal closes that doc's **Defer item 5** ("Credits / proration / tax-line / multi-currency / double-entry GL — shapes already present") and adds the operational lifecycle around a finalized invoice: credits, refunds, void+reissue, proration, tax seam, notifications, and creator read APIs.
**Scope:** the money lifecycle *after* a bill is computed. The redesign got the invoice *shape* right (`credit_cents`/`tax_cents`/`currency`/balance CHECK/immutability triggers). This proposal makes those columns *live* and wires the surrounding operational seams, **without ever mutating a finalized invoice**.

---

## Executive summary

The billing-schema redesign froze a correct, reproducible invoice: a balance CHECK (`total = subtotal − credit + tax`), two immutability triggers (a finalized invoice and its lines are append-only; the only legal transition is `finalized → void`), and a frozen snapshot-onto-line so any finalized bill replays bit-for-bit. But it left `credit_cents`/`tax_cents` hard-wired to `0`, the `void` transition legally-permitted-but-never-driven, and there is **no refund path, no proration record, no tax computation, and no way for a creator to ever see a single invoice**. The dunning lifecycle (`creator_billing_status` + history) and the spend lifecycle (`app_spend_state` + history) both already *write* their transition rows — but nothing turns those rows into an email. The platform can bill, suspend, and reconcile; it cannot credit, refund, correct, prorate, tax, notify, or expose.

This proposal closes that gap with **one core principle that makes the whole lifecycle compose with the immutability triggers**:

> **Money records are append-only side facts FROZEN onto the invoice at finalize — NEVER mutations of a finalized invoice.**

A credit is consumed *at finalize* and written into `credit_cents` inside the existing one-statement finalize UPDATE (it is an *input* to the frozen total, not an after-the-fact edit). A refund is a **separate `refunds` row** with a real FK to the paid invoice; for cash-back on an already-paid invoice the money-movement object is a Stripe **`Refund` (`re_…`) on the original charge/PaymentIntent** (a credit note `cn_…` is an *optional* bookkeeping wrapper, never the thing that moves the money) — the invoice stays `finalized`, untouched. For credit-back the refund is a **platform-native `credit_ledger` grant** (or, on Stripe, a `credit_amount` customer-balance transaction) with no charge reversal. A correction is a **void + a fresh reissued invoice**, never an in-place rewrite — and the void appends a compensating `void_reversal` ledger entry so any credit the voided invoice consumed is restored before the reissue re-consumes (balance is conserved). Tax is computed *at finalize* and frozen into `tax_cents`; a refund of a taxed invoice refunds proportional tax (`refunds` splits `amount_cents` into `subtotal_cents`/`tax_cents`). Proration is an append-only timeline event with **server-derived** money fields that re-prices the *next* draft, never the last finalized one. **Payment receipts are append-only `invoice_payments` rows** — never a mutable column on the finalized invoice — so cash-collected is `Σ(invoice_payments)` and the immutability trigger is never even challenged. Every one of these is a side fact or a new row; none of them touch a frozen invoice. That single discipline is why the lifecycle is safe by construction against the triggers that already exist.

<!-- Rewritten in round 2: CRITICAL-A — cash_collected is a SUM over an append-only side table, never a column on the finalized invoice (the round-1 column write was rejected by invoices_immutable()) -->
> **The over-refund cap is anchored on cash actually collected — computed as `Σ(invoice_payments)`, never on the credit-inflated `total_cents`.** A naive "Σ refunds ≤ `total_cents`" cap is a credit-laundering hole: a creator who consumed $40 credit + paid $60 cash on a $100 invoice could refund $60 *to credit* and net +$60 balance for $60 cash, repeatably. **Round 1 tried to freeze this as `invoices.cash_collected_cents`, but that write is mechanically impossible:** the redesign's `invoices_immutable()` trigger RAISEs on *any* finalized→finalized UPDATE — the only permitted transition is `→void` with all money columns held equal — so a payment webhook writing a cash column onto a finalized invoice fails every time, and adding the column to the held-equal set makes the void path worse. **Round 2 moves payment tracking off the invoice entirely** into an append-only `invoice_payments` side table (`0053`, FK→`invoices(id)`, one row per payment / partial-pay / dispute-clawback). Cash-collected is then a derived `Σ(invoice_payments.amount_cents)` — no column on the frozen invoice, no trigger to fight, and exactly the doc's own "money records are append-only side facts" principle. The over-refund trigger enforces three bounds simultaneously against this sum: `Σ(cash refunds) ≤ cash_collected`, `Σ(credit-destination refunds) ≤ cash_collected`, and combined `Σ ≤ cash_collected`. Credit-funded value is never re-granted as cash or as fresh credit.

Around that core, nine changesets and nine PRs (round 2 added `0053 invoice_payments`; round 3 added `0054` invoice-lines line-grain reshape for full usage-segment proration):

- **`0048` credits** — an append-only `credit_ledger` (the Stripe customer-balance model: balance is `SUM(entries)`, never a stored column) with a `CHECK` coupling kind↔sign; the reconciler consumes oldest-first at finalize, writes `invoices.credit_cents` in the one-statement finalize UPDATE (credit applied *before* tax, matching the balance-CHECK ordering), and appends **one `consumed` entry per drawn grant** (`consumed_from_grant_id`) so credit-expiry attribution is exact (decision 7). `void_reversal` and `refund_to_credit` are entry kinds.
- **`0049` refunds** — a `refunds` table with a real FK to `invoices(id)`, a **cash-collected-anchored** over-refund backstop (reading `Σ(invoice_payments)`, not `total_cents`), a `subtotal_cents`/`tax_cents` split (tax-on-refund), and a `RefundProvider` seam whose Stripe path is **split by destination**: `destination='cash'` → a Stripe **`Refund` (`re_…`)** on the paid invoice's charge/PaymentIntent (optionally wrapped in a credit note `cn_…` for bookkeeping); `destination='credit'` → a platform-native `credit_ledger('refund_to_credit')` grant (no Stripe charge reversal) or a `credit_amount` customer-balance transaction. `ref_kind` allows `'refund'` (re_…) and `'credit_note'` (cn_…). Claim-then-call idempotency mirrors the line provider-refs; operator endpoints take an **idempotency key + a request-body fingerprint** (reuse-with-different-body is rejected, mirroring Stripe).
- **`0042` reshape** — replace `UNIQUE(creator_id, period)` (the auto-named `invoices_creator_id_period_key` — verify against live `\d`) with a **partial unique index `WHERE status <> 'void'`** so voiding releases the period claim and a corrected invoice can reissue + re-price reproducibly. The operator void+reissue path takes the **same per-creator reconcile advisory lock** so it can't race a cron reconcile. (Round 2: `0042` no longer adds a `cash_collected_cents` column — that write was impossible against `invoices_immutable()`; payment tracking moved to `0053 invoice_payments`.)
- **`0053` invoice payments** <!-- Added in round 2: CRITICAL-A --> — an append-only `invoice_payments` side table (FK→`invoices(id)`) recording each payment / partial-pay / dispute-clawback against a finalized invoice. Cash-collected is `Σ(invoice_payments.amount_cents)`, the anchor for the over-refund cap. Written by the payment-confirmation webhook **without touching the frozen invoice**, so the immutability trigger is never challenged.
- **`0050` proration + `0054` line-grain reshape** — an append-only `plan_change_events` timeline row recorded on *every* `set_plan`, with **server-derived** frozen base-fee money fields AND a **`usage_at_change` cumulative-usage snapshot** (never client-supplied) and a per-period change cap. round 3 (decision 1 OVERRIDE): v1 = **FULL usage-segment proration** — an app with N plan-change events splits into N+1 segments, each priced under its own plan over the cumulative-snapshot usage delta and frozen as a separate `invoice_lines` segment line. `0054` widens the `invoice_lines` PK with `segment_no` and retargets the `billing_line_provider_refs` FK; it degenerates to one line per app when there is no plan change.
- **`0051` notifications** — a `BillingNotifier` seam over the existing `Mailer` (relocated to a new shared `crates/mailer` in **PR-0**, ahead of this work), driven off the already-written `spend_state_history` + `creator_billing_status_history` rows, with a `billing_notifications` send-ledger that **claims BEFORE send** (two-phase `pending→sent`) under the dunning advisory lock — fixing the multi-node double-send.
- **PR-5 tax (seam only)** — a `TaxProvider` trait computing tax at finalize; Native returns `0`; enabling Stripe Tax later is a provider swap, **not** a schema change. Refunds carry a proportional tax split so a taxed invoice refunds tax correctly.
- **PR-7 read APIs** — creator-scoped `BillingRead` endpoints on `api.rs`: invoice history, frozen-snapshot line detail, current-period projected charge (**cached, non-authoritative**), credit balance, payment-method status, plan/spend-state.

All money writes stay operator-only (`Action::BillingWrite` on `Resource::Any`); all idempotency is claim-then-call with durable real-FK guards **plus operator-supplied idempotency keys on the two grant/refund endpoints**; all fixes ship with a named regression test that fails pre-fix and runs the real reconciler against a cyper/mock-Stripe. Disputes/chargebacks, a negative-invoice true-up bridge, and money-seam observability metrics are first-class (new sections below).

---

## Design principles (industrial best practice applied)

1. **Append-only money; corrections are new facts (Stripe / double-entry).** Stripe never edits a finalized invoice: a credit is a customer-balance transaction, a refund is a credit note or refund object, a correction is a void + reissue. We adopt the same posture verbatim — it is the *only* posture that composes with the redesign's immutability triggers, which already reject every UPDATE to a finalized invoice except `→ void`.

2. **Credit as a SUM-of-entries ledger, never a stored balance (Stripe customer balance / Metronome credit grants).** The balance is `SUM(amount_cents)` over the ledger, so a grant and its consumption are *two rows* and the balance can never drift from its history. No mutable balance column to clobber.

3. **Refund a paid invoice with a Stripe `Refund` (`re_…`), never a void (verified against the Stripe API, round 1).** A void erases the whole bill and violates the immutability trigger. For an **already-paid** invoice, money-back to the card is a Stripe **`Refund` object (`re_…`)** issued against the original charge/PaymentIntent — this is the object that decreases the platform's Stripe balance and returns funds. A **credit note (`cn_…`)** is a different instrument: it adjusts the *recorded* amount of an invoice and, on Stripe, can *orchestrate* a refund (its `refund_amount`/`refunds[]` parameters **create or link** a `re_…`) or a customer-balance credit (its `credit_amount`, which never moves money). So our split (verified at `docs.stripe.com/api/credit_notes/create` and `/api/refunds`):
   - **`destination='cash'`** → issue a `Refund` (`re_…`) on the paid invoice's charge/PaymentIntent. We MAY additionally emit a credit note (`cn_…`) purely for the customer's PDF/bookkeeping, but the `re_…` is the authoritative money-movement ref.
   - **`destination='credit'`** → no charge reversal. Platform-native: append a `credit_ledger('refund_to_credit')` grant. (If we later want the credit to live on Stripe's side too, the equivalent is a `credit_amount` customer-balance transaction / a credit note with `credit_amount` — a negative customer balance auto-applies to the next finalized invoice.)
   
   In all cases the invoice stays `finalized` and is never mutated; the refund is the new fact. The earlier draft's "credit note = the refund mechanism" was wrong: a credit note alone does not return cash on a paid invoice. (Stripe customer-balance semantics: negative balance = customer credit, auto-applied on next finalize.)

4. **Provider-agnostic seams mirror the metering-provider trait (Lago / OpenMeter pluggability).** `RefundProvider`, `TaxProvider`, and `BillingNotifier` each follow the exact `MeteringProvider` pattern in `crates/control/src/metering/provider/mod.rs`: an `#[async_trait(?Send)]` object-safe trait, a `Native` default that is a no-op / zero, and an export/integration impl behind it. Swapping Stripe Tax or a webhook notifier touches only the seam.

5. **Effective-dated lifecycle events as append-only timelines (Orb / Lago subscription events).** A plan change is a `plan_change_events` row, not an in-place edit of `apps.plan_id` semantics. Proration reads the timeline; the timeline is the audit record.

6. **Reproducibility extends to credit/tax (snapshot-onto-line, continued).** Credit and tax are *inputs* to the frozen total, computed and written in the SAME one-statement finalize UPDATE the redesign already uses — so the balance CHECK never sees a half-written row and the finalized invoice still replays bit-for-bit.

7. **Send-once notifications via a claim-BEFORE-send ledger under the dunning advisory lock (multi-node-safe).** <!-- Changed in round 1: addressing CRITICAL #3 — claim-after-success is at-most-once only under single-flight --> Claim-*after*-success is at-most-once **only under single-flight**: two control instances scanning the same unsent transition both send, then both `ON CONFLICT DO NOTHING` — but the email already went twice. We close this two ways, both required: (a) the notify cron holds a dedicated `pg_try_advisory_lock` for the whole sweep, exactly as `dunning.rs` does (it lives next door, key `0x7a73_6475_6e6e_0001` family), so only one instance sweeps per tick; and (b) the send is **two-phase**: the cron first `INSERT … billing_notifications (…, status='pending') ON CONFLICT DO NOTHING RETURNING` — the row whose INSERT *wins* (returns) is the only one cleared to send; it then sends and flips `status='sent'`. The claim INSERT, not the send, arbitrates the race. A crash after claim-before-send leaves a `pending` row whose `claimed_at` is past `NOTIFY_REDRIVE_HORIZON` (= 15min), so it is retried. <!-- Corrected in round 2, MAJOR-A: this is at-least-once DELIVERY / exactly-once CLAIM, NOT exactly-once delivery — a crash in the send→flip window re-drives and re-sends. The delivery EFFECT is made idempotent by a provider-side Mailer Idempotency-Key = (creator_id, kind, transition_id), mirroring the Stripe Idempotency-Key the refund path uses. --> The honest guarantee is **at-least-once delivery, exactly-once claim**; the provider idempotency key makes the delivery effect idempotent (effectively-once) on any provider that honours it.

8. **Fail-closed RLS + least-privilege grants (existing platform pattern).** App-keyed tables FORCE RLS on `current_setting('zeroship.tenant_app')`; creator-keyed control bookkeeping is RLS-exempt (control runs BYPASSRLS; the key is a `creator_id`, not an `app_id`). All five new tables here are creator-keyed or invoice-keyed (FK→creator-keyed) ⇒ RLS-exempt, role-guarded grants per table.

9. **Immutability where money is frozen (continued).** A `credit_ledger` entry, once written, is append-only (a `consumed` entry offsets a grant; a grant is never edited). A `refunds` row, once its provider credit-note ref is written, is frozen by trigger. The discipline is uniform with the invoice/line triggers.

10. **Operator-only money writes; creator read-only (least privilege).** Every credit grant, refund, and void is `Action::BillingWrite` on `Resource::Any` (operator / master-key). Creators get only `BillingRead` on their owned apps. Self-serve refunds are a documented future, fail-closed off by default.

   <!-- Added in round 1: addressing CRITICAL #4 — plan_change_events is creator-writable via set_plan's Resource::App fallback -->
   **Reconciling principle #10 with `plan_change_events` (which a creator CAN write).** `set_plan` is reachable by a creator: its authz probes operator (`Resource::Any`) first and falls back to `Resource::App{id}` (`api.rs:602`, the `assignable_by_creator` path). So a creator legitimately triggers a `plan_change_events` INSERT, yet that row carries **frozen money fields** (`from/to_base_fee_cents`) that feed proration — which would let a creator manufacture a favorable bill if those fields were client-supplied. They are NOT: the proration money fields are **derived SERVER-SIDE from the plan catalog at write time** (`PlanCatalog::get(from_plan).base_fee_cents` / `…(to_plan)…`), never read from the request body — the `set_plan` body carries only `plan_id`. The proration record is therefore the *one* class of creator-reachable money write that is permitted under #10, **because it is server-derived, not client-supplied** — a tightly-scoped, named exemption, not a hole. Two further guardrails bound abuse: (a) a **per-period plan-change cap** (`MAX_PLAN_CHANGES_PER_PERIOD`, default 8) debounces a creator manufacturing many favorable micro-segments — past the cap, `set_plan` still switches `apps.plan_id` (the creator *is* on the new plan) but records **no new** `plan_change_events` snapshot; the over-cap tail then **merges** into the final segment, priced under the **actually-running** `apps.plan_id` (round 4, MAJOR-4 — never under the last *recorded* cheap plan, which with per-plan FX overrides would be a deliberate under-bill); (b) the change is rate-limited per app. Every other money write (credit grant, refund, void) remains strictly operator-only on `Resource::Any`.

---

## Full schema — new + reshaped Liquibase changesets

File order preserves FK precedence: the `0042` reshape lands first (it gates the partial-unique-index), then `0048` credits (FK→`creator_billing`), `0049` refunds (FK→`invoices`, reads `Σ(invoice_payments)`), `0050` proration (FK→`apps`/`plans`), `0051` notifications (FK→`creator_billing`), `0052` disputes (FK→`invoices`), `0053` `invoice_payments` (FK→`invoices`), `0054` invoice-lines segment reshape (ALTERs `invoice_lines`/`billing_line_provider_refs` that `0042` created). The tax seam (PR-5) is **code-only** — no changeset, because `tax_cents` already exists. The `Mailer` relocation is **PR-0** (a `crates/mailer` move, ahead of all of the above — round 1, MAJOR-4). <!-- round 3: 0054 added for full usage-segment proration (decision 1 OVERRIDE) — it widens invoice_lines's PK with segment_no and retargets the billing_line_provider_refs composite FK; lands after 0042/0053 in the changelog since it ALTERs tables those created. --> <!-- round 4, MINOR-7: 0042's inline billing_line_provider_refs composite FK is NAMED `billing_line_provider_refs_line_fk` (a one-line edit to 0042, like the surrogate-id ALTERs this doc already asks of the redesign author) so 0054 DROPs a KNOWN constraint name, not a guessed one. --> <!-- Round 2, CRITICAL-A: 0042 no longer adds cash_collected_cents; payment tracking is the new 0053 invoice_payments side table. 0049's over-refund trigger reads Σ(invoice_payments) via a helper, NOT a column. invoice_payments must exist before the 0049 trigger function references it — see ordering note under 0053. -->

<!-- Round 2, CRITICAL-A: cash-collected is a derived helper over the side table, referenced by 0049's trigger, the dispute check, the true-up bridge, and the worked example. -->
> **Cash-collected is a derived sum, never a stored column.** Every site that round 1 read `invoices.cash_collected_cents` (the `0049` over-refund trigger, the dispute-aware endpoint check, the true-up bridge, the read API) now reads `COALESCE(SUM(amount_cents), 0)` over `invoice_payments WHERE invoice_id = $inv` — the `0049` trigger **inlines** that `SELECT` (shown below) so it is self-contained; the Rust paths run the same query. Because `0049`'s trigger function references `invoice_payments`, the **`0053` changeset must land BEFORE `0049`**: Liquibase runs changesets in changelog order, not numeric order, so the changelog master lists `0053` ahead of `0049` with a pinning comment. (`0053` carries a higher number only because it was added in round 2; the changelog order is what matters.)

### `0042` reshape — partial unique index on `invoices` (void releases the period claim)

```sql
--changeset zeroship:invoices-period-claim-partial-unique splitStatements:true
-- VOID + REISSUE (gap #26 C). The redesign's `UNIQUE (creator_id, period)` is the
-- no-double-bill claim — but it ALSO permanently blocks reissuing a corrected
-- invoice for a period whose first invoice was voided. A void is the legal
-- correction transition (the immutability trigger permits exactly finalized→void);
-- once an invoice is voided it must RELEASE its period claim so a fresh, re-priced
-- invoice can take the slot. Replace the unconditional UNIQUE with a PARTIAL unique
-- index that ignores voided rows: AT MOST ONE non-void invoice per (creator, period),
-- UNBOUNDED void rows. The reconciler's `ON CONFLICT (creator_id, period) DO NOTHING`
-- claim is rewritten to target this partial index (`ON CONFLICT … WHERE status <> 'void'`).
-- NOTE: this RESHAPES the redesign's `0042` invoices UNIQUE — called out explicitly.
-- CONSTRAINT NAME (round 1, MAJOR-3): the redesign declares `UNIQUE (creator_id, period)`
-- INLINE in the `invoices` table (no explicit name), so Postgres auto-names it
-- `invoices_creator_id_period_key` (table_columns_key). That is the EXACT name to DROP.
-- VERIFY against live `\d zeroship.invoices` before merge — if a later redesign edit
-- pins an explicit CONSTRAINT name, use that instead. The DROP is name-pinned, not
-- IF-EXISTS-guessed, so a name skew fails the migration loudly rather than silently
-- leaving the unconditional UNIQUE in place (which would defeat void+reissue).
ALTER TABLE zeroship.invoices DROP CONSTRAINT invoices_creator_id_period_key;
CREATE UNIQUE INDEX invoices_active_period_claim
    ON zeroship.invoices (creator_id, period)
    WHERE status <> 'void';
--rollback DROP INDEX IF EXISTS zeroship.invoices_active_period_claim;
--rollback ALTER TABLE zeroship.invoices ADD CONSTRAINT invoices_creator_id_period_key UNIQUE (creator_id, period);
```

<!-- Removed in round 2 (CRITICAL-A): the round-1 `invoices-cash-collected` ALTER ... ADD COLUMN
     was a money DEFECT, not a feature. cash_collected_cents was to be WRITTEN by the payment
     webhook onto an ALREADY-finalized invoice — but invoices_immutable() RAISEs on EVERY
     finalized→finalized UPDATE (only →void with money held equal is legal). The write fails
     every time; adding the column to the held-equal set only constrains the void path further
     and still never lets the webhook write it. Payment tracking moves to the 0053
     invoice_payments side table below; cash-collected becomes a derived Σ. The column, its
     CHECK (cash_collected_cents <= total_cents), and the proposed invoices_immutable() amendment
     are ALL deleted — the redesign's invoices_immutable() is left exactly as committed. -->

> **There is no `cash_collected_cents` column.** Round 1 proposed freezing one on `invoices` at payment confirmation; that write is mechanically impossible (the committed `invoices_immutable()` rejects any finalized→finalized UPDATE), so round 2 deletes it. Cash-collected lives in the append-only `invoice_payments` side table (`0053`) and is read as `Σ(invoice_payments.amount_cents)`. **The redesign's `invoices_immutable()` is UNCHANGED** — `0042` no longer touches it. This is strictly more consistent with the doc's core principle: a payment is a side fact, not a mutation of a frozen invoice.

> The auto-generated constraint name (`invoices_creator_id_period_key`) is Postgres's default for `UNIQUE (creator_id, period)`; confirm against the live `\d zeroship.invoices` before merge and pin the name if the redesign named it explicitly.

### `0048` — `credit_ledger.sql`

```sql
--liquibase formatted sql

--changeset zeroship:credit-ledger splitStatements:true
-- CREDITS (gap #26 A). The Stripe customer-balance model: an append-only ledger
-- whose BALANCE is SUM(amount_cents), NEVER a stored column (it cannot drift from
-- its history). A grant is a POSITIVE entry; at finalize the reconciler appends ONE
-- NEGATIVE `consumed` entry PER DRAWN GRANT (round 1, MAJOR-2 — per-grant, not a
-- single aggregate companion, so credit-expiry attribution is exact). Balance for a
-- creator = SUM(amount_cents) >= 0 (enforced not by a column but by the reconciler
-- consuming at most the balance).
-- Creator-keyed control bookkeeping ⇒ NO app RLS (control is BYPASSRLS). Real FK to
-- creator_billing(creator_id) ⇒ a stray writer cannot orphan an entry; CASCADE is
-- uniform with the redesign's erase model (anonymize-retained creators keep the FK
-- target alive; never-billed creators CASCADE clean).
CREATE TABLE zeroship.credit_ledger (
    id          TEXT        PRIMARY KEY,                -- crd_<base62>
    creator_id  UUID        NOT NULL REFERENCES zeroship.creator_billing(creator_id) ON DELETE CASCADE,
    -- entry provenance. Positive kinds GRANT balance; 'consumed' is the negative
    -- companion written PER DRAWN GRANT at finalize. 'refund_to_credit' is the
    -- destination of a refund routed to balance (gap #26 B). 'void_reversal' is the
    -- compensating positive entry written INSIDE a void txn to restore credit the
    -- voided invoice consumed (round 1, CRITICAL-2). Membership only; ordering in Rust.
    kind        zeroship.credit_entry_kind NOT NULL,
    -- SIGNED: all positive kinds > 0; consumed < 0. The sign convention is what makes
    -- the balance a pure SUM. round 1 MAJOR-1: the convention is now ENFORCED by a
    -- CHECK coupling kind↔sign (below), not only by a test.
    amount_cents BIGINT     NOT NULL CHECK (amount_cents <> 0),
    currency    CHAR(3)     NOT NULL DEFAULT 'usd' CHECK (currency ~ '^[a-z]{3}$'),
    -- The invoice a 'consumed'/'void_reversal'/'refund_to_credit' entry relates to
    -- (NULL for grants). Real FK ⇒ such an entry can never reference a non-existent
    -- invoice; the audit join is DB-guaranteed. round 2, MINOR-B: the ON DELETE RESTRICT
    -- is NOT load-bearing — invoices are append-only and NEVER deleted (their own
    -- immutability trigger rejects DELETE), so the referential action can never fire. It
    -- is kept only as a uniform, defensive default (matching the redesign's other
    -- invoice-FK side tables), not as an active guard.
    applied_invoice_id TEXT REFERENCES zeroship.invoices(id) ON DELETE RESTRICT,
    -- round 1, MAJOR-2 + DECISION 7: PER-GRANT consume. A 'consumed' entry draws from
    -- exactly ONE grant, named here, so credit-expiry attribution is EXACT (a single
    -- aggregate companion cannot say WHICH grants were spent — false once expires_at
    -- exists). A void_reversal references the consumed grant it restores via the same
    -- column. Real self-FK to the granting row; NULL for grant kinds.
    consumed_from_grant_id TEXT REFERENCES zeroship.credit_ledger(id) ON DELETE RESTRICT,
    -- Optional expiry (PRODUCT DECISION 7): a grant past expires_at is NOT consumable.
    -- NULL ⇒ never expires. The reconciler's consume-query filters
    -- `expires_at IS NULL OR expires_at > NOW()` and draws grant-by-grant oldest-first.
    expires_at  TIMESTAMPTZ,
    note        TEXT,                                   -- operator audit ('promo X', 'goodwill ticket #…')
    -- round 1, MISSING-CONCEPT 7 (idempotency): operator-supplied key on GRANT-class
    -- entries written via POST /billing/credit. A double-clicked grant carries the same
    -- key, so the partial UNIQUE index below makes the second INSERT a no-op (no double
    -- grant — which had NO guard in the draft). NULL for reconciler-internal entries
    -- (consumed / void_reversal / refund_to_credit are guarded by their own claim paths,
    -- not by an operator key), so the column is nullable and the UNIQUE is PARTIAL.
    idempotency_key TEXT,
    -- round 2, MINOR-A: request-body fingerprint, same posture as refunds. A grant key
    -- reused with a DIFFERENT amount/currency/expiry must NOT silently return the first
    -- grant (Stripe 400s on key-reuse-with-different-body). The endpoint stores a SHA-256
    -- over the canonical grant request (creator_id, amount_cents, currency, expires_at,
    -- kind) here; on a key hit it returns 409 unless the fingerprint matches. NULL
    -- whenever idempotency_key is NULL (reconciler-internal entries).
    request_fingerprint TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- round 1, MAJOR-1: kind↔sign coupling. consumed is the ONLY negative kind; every
    -- other kind (grant/promo/goodwill/refund_to_credit/void_reversal) is positive.
    -- This makes the SUM-balance invariant a DB fact, not a test convention.
    CONSTRAINT credit_ledger_kind_sign
        CHECK ((kind = 'consumed' AND amount_cents < 0)
            OR (kind <> 'consumed' AND amount_cents > 0)),
    -- A consumed/void_reversal entry MUST name the grant it draws from / restores;
    -- a grant-class entry MUST NOT (it is itself a source).
    CONSTRAINT credit_ledger_grant_ref
        CHECK ((kind IN ('consumed','void_reversal') AND consumed_from_grant_id IS NOT NULL)
            OR (kind NOT IN ('consumed','void_reversal') AND consumed_from_grant_id IS NULL))
);
-- The hot read is "this creator's consumable balance, oldest-first" (FIFO consume).
CREATE INDEX credit_ledger_creator_created_idx
    ON zeroship.credit_ledger (creator_id, created_at);
-- round 1, MISSING-CONCEPT 7: operator-grant idempotency. PARTIAL unique (only
-- grant-class entries carry a key) so a retried POST /billing/credit is a no-op.
CREATE UNIQUE INDEX credit_ledger_idempotency_key_idx
    ON zeroship.credit_ledger (idempotency_key)
    WHERE idempotency_key IS NOT NULL;
--rollback DROP INDEX IF EXISTS zeroship.credit_ledger_idempotency_key_idx;
--rollback DROP INDEX IF EXISTS zeroship.credit_ledger_creator_created_idx;
--rollback DROP TABLE zeroship.credit_ledger;

--changeset zeroship:credit-entry-kind-domain splitStatements:true
-- Entry kinds: membership only (the DOMAIN encodes the SET; the kind↔sign coupling
-- is a TABLE CHECK above, round 1 MAJOR-1). 'void_reversal' added (CRITICAL-2): the
-- compensating positive entry restoring credit a voided invoice consumed.
CREATE DOMAIN zeroship.credit_entry_kind AS TEXT
    CHECK (VALUE IN ('grant','promo','goodwill','refund_to_credit','consumed','void_reversal'));
--rollback DROP DOMAIN IF EXISTS zeroship.credit_entry_kind;

--changeset zeroship:credit-ledger-immutable splitStatements:false
-- A credit entry is append-only: a balance correction is a NEW offsetting entry,
-- never an edit (uniform with the invoice/line immutability discipline). Reject
-- UPDATE and DELETE outright.
CREATE FUNCTION zeroship.credit_ledger_immutable() RETURNS trigger AS $fn$
BEGIN
    RAISE EXCEPTION 'credit_ledger is append-only (no UPDATE/DELETE) — correct via a new offsetting entry';
END;
$fn$ LANGUAGE plpgsql;
CREATE TRIGGER credit_ledger_immutable_trg
    BEFORE UPDATE OR DELETE ON zeroship.credit_ledger
    FOR EACH ROW EXECUTE FUNCTION zeroship.credit_ledger_immutable();
--rollback DROP TRIGGER IF EXISTS credit_ledger_immutable_trg ON zeroship.credit_ledger;
--rollback DROP FUNCTION IF EXISTS zeroship.credit_ledger_immutable();

--changeset zeroship:credit-ledger-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    -- INSERT-only beyond SELECT: append-only, no UPDATE/DELETE grant (the trigger
    -- is the backstop; the grant is the first line of defence — least privilege).
    EXECUTE 'GRANT SELECT, INSERT ON zeroship.credit_ledger TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.credit_ledger FROM zeroship_control'; END IF; END $rb$;
```

> **Changeset ordering note:** the `credit_entry_kind` domain is referenced by the `credit_ledger` table, so in the actual file the `credit-entry-kind-domain` changeset is authored **first** (domains before tables, mirroring `0037`). It is shown second above only for narrative grouping.

### `0049` — `refunds.sql`

```sql
--liquibase formatted sql

--changeset zeroship:refunds splitStatements:true
-- REFUNDS (gap #26 B, round 1 CRITICAL-5 + tax-on-refund + idempotency). A refund is
-- a SEPARATE append-only fact with a REAL FK to the PAID invoice — NEVER a mutation
-- of the finalized invoice (the immutability trigger would reject it anyway). The
-- invoice stays status='finalized'. For destination='cash' the money-movement object
-- is a Stripe REFUND (re_…) on the original charge/PaymentIntent (a credit note cn_…
-- is an OPTIONAL bookkeeping wrapper, NOT the refund). For destination='credit' the
-- refund is a platform-native credit_ledger('refund_to_credit') grant — no charge
-- reversal. Creator-keyed via the invoice's creator ⇒ NO app RLS.
CREATE TABLE zeroship.refunds (
    id          TEXT        PRIMARY KEY,                -- ref_<base62>
    invoice_id  TEXT        NOT NULL REFERENCES zeroship.invoices(id) ON DELETE RESTRICT,
    -- round 1, MISSING-CONCEPT 6 (tax-on-refund): a refund of a TAXED invoice must
    -- return proportional tax. amount_cents is split into the pre-tax portion and the
    -- tax portion so the Stripe Refund / credit-note line carries the right tax split
    -- and the platform's tax-liability books stay correct. amount_cents = subtotal +
    -- tax (CHECK). For a zero-tax (USD-launch) invoice tax_cents = 0 and the split is
    -- degenerate, but the column exists so enabling Stripe Tax later needs no reshape.
    amount_cents   BIGINT  NOT NULL CHECK (amount_cents > 0),
    subtotal_cents BIGINT  NOT NULL CHECK (subtotal_cents >= 0),
    tax_cents      BIGINT  NOT NULL DEFAULT 0 CHECK (tax_cents >= 0),
    CONSTRAINT refund_amount_split CHECK (amount_cents = subtotal_cents + tax_cents),
    currency    CHAR(3)     NOT NULL DEFAULT 'usd' CHECK (currency ~ '^[a-z]{3}$'),
    -- Destination (PRODUCT DECISION 3): 'cash' = a Stripe Refund (re_…) back to the
    -- card; 'credit' = a credit_ledger 'refund_to_credit' grant instead of cash.
    -- Membership only.
    destination zeroship.refund_destination NOT NULL,
    reason      TEXT,                                   -- operator audit; PII posture: see redaction note
    -- round 1, MISSING-CONCEPT 7 (idempotency): the operator-supplied idempotency key.
    -- A double-clicked / retried POST /invoices/{id}/refunds carries the SAME key, so
    -- the UNIQUE(idempotency_key) below makes the second INSERT a no-op (the first
    -- refund is returned) — NO double refund. NOT NULL: the endpoint requires it.
    idempotency_key TEXT NOT NULL,
    -- round 2, MINOR-A: request-body fingerprint. A globally-UNIQUE key with NO body
    -- check silently returns the FIRST refund even if a caller REUSES the key with a
    -- DIFFERENT amount/destination — masking a real bug. Stripe 400s on key-reuse-with-
    -- different-body (verified at docs.stripe.com/api/idempotent_requests: "compares
    -- incoming parameters to those of the original request and errors if they're not the
    -- same"). We mirror that: the endpoint computes a SHA-256 over the canonical refund
    -- request (invoice_id, amount_cents, subtotal_cents, tax_cents, destination) and
    -- stores it here. On a key hit, if the stored fingerprint != the new request's
    -- fingerprint the endpoint returns 409/422 (idempotency-key-reuse-conflict), NOT the
    -- first refund. Same key + same body ⇒ return the first refund (safe retry).
    request_fingerprint TEXT NOT NULL,
    -- Lifecycle: 'pending' the instant the row is claimed (intent), 'issued' after
    -- the provider call succeeds and its ref is written. A failed call leaves 'pending'
    -- for the re-drive. (round 1, MINOR-2: 'void' DROPPED — you cannot un-refund cash;
    -- a mistaken refund is corrected by a fresh re-charge / negative-invoice true-up,
    -- not by un-refunding. See the Negative-invoice true-up section.) Membership only.
    status      zeroship.refund_status NOT NULL DEFAULT 'pending',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    issued_at   TIMESTAMPTZ,
    UNIQUE (idempotency_key)
);
CREATE INDEX refunds_invoice_idx ON zeroship.refunds (invoice_id);
--rollback DROP INDEX IF EXISTS zeroship.refunds_invoice_idx;
--rollback DROP TABLE zeroship.refunds;

--changeset zeroship:refund-domains splitStatements:true
CREATE DOMAIN zeroship.refund_destination AS TEXT
    CHECK (VALUE IN ('cash','credit'));
-- round 1, MINOR-2: 'void' removed — cash refunds are irreversible; there is no
-- un-refund. A refund either succeeds ('issued') or is re-driven ('pending').
CREATE DOMAIN zeroship.refund_status AS TEXT
    CHECK (VALUE IN ('pending','issued'));
--rollback DROP DOMAIN IF EXISTS zeroship.refund_status;
--rollback DROP DOMAIN IF EXISTS zeroship.refund_destination;

--changeset zeroship:refund-provider-refs splitStatements:true
-- The provider seam for refunds. round 1, CRITICAL-5: ref_kind allows BOTH
-- 'refund' (re_… — the cash money-movement object, the authoritative ref for
-- destination='cash') AND 'credit_note' (cn_… — the OPTIONAL bookkeeping wrapper)
-- AND 'customer_balance_txn' (cbtxn_… — the Stripe-side credit path, if used).
-- Mirrors billing_line_provider_refs: written CLAIM-AFTER-SUCCESS so a refund whose
-- POST errored has NO ref and the re-drive re-issues it (idempotent on the
-- deterministic provider idempotency key); a refund WITH a ref is skipped. A
-- malformed key cannot be inserted (the FK rejects it), so the double-refund guard
-- cannot silently fail-to-match.
CREATE TABLE zeroship.refund_provider_refs (
    refund_id   TEXT NOT NULL REFERENCES zeroship.refunds(id) ON DELETE CASCADE,
    provider    TEXT NOT NULL,                -- 'stripe'
    ref_kind    TEXT NOT NULL,                -- 'refund' (re_…) | 'credit_note' (cn_…) | 'customer_balance_txn'
    external_id TEXT NOT NULL,                -- re_… / cn_… / cbtxn_…
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (refund_id, provider, ref_kind),
    UNIQUE (provider, ref_kind, external_id)
);
--rollback DROP TABLE zeroship.refund_provider_refs;

--changeset zeroship:refund-over-refund-guard splitStatements:false
-- OVER-REFUND BACKSTOP (round 1 CRITICAL-1; round 2 CRITICAL-A: read Σ(invoice_payments),
-- NOT a cash_collected_cents column). The cap is anchored on the cash ACTUALLY collected —
-- the SUM of the append-only invoice_payments rows for this invoice — NOT total_cents
-- (which is credit-inflated; capping on it enables credit laundering). THREE bounds
-- enforced simultaneously:
--   (i)  Σ(cash refunds)               ≤ cash_collected
--   (ii) Σ(credit-destination refunds) ≤ cash_collected
--   (iii) Σ(all refunds, any dest)     ≤ cash_collected
-- (i)+(ii) stop EITHER channel alone exceeding the cash actually collected; (iii)
-- stops the two channels SUMMING past it. Credit-funded value (total_cents −
-- cash_collected) is NEVER re-granted as cash OR as fresh credit. A CHECK cannot span
-- rows, so this is a BEFORE-INSERT trigger (the DB-level backstop; the Rust path also
-- checks before claiming). status carries no 'void' now, so every existing refund row
-- counts. NOTE: invoice_payments (0053) must exist before this function is created —
-- the changelog master lists 0053 ahead of 0049 (see the ordering note in the schema
-- intro); the SELECT below references it directly so 0049 needs no helper function.
CREATE FUNCTION zeroship.refunds_no_over_refund() RETURNS trigger AS $fn$
DECLARE cash BIGINT; sum_cash BIGINT; sum_credit BIGINT;
BEGIN
    -- round 2 CRITICAL-A: cash collected = Σ(invoice_payments), not a frozen column.
    SELECT COALESCE(SUM(amount_cents), 0) INTO cash
      FROM zeroship.invoice_payments WHERE invoice_id = NEW.invoice_id;
    SELECT COALESCE(SUM(amount_cents) FILTER (WHERE destination = 'cash'),   0),
           COALESCE(SUM(amount_cents) FILTER (WHERE destination = 'credit'), 0)
      INTO sum_cash, sum_credit
      FROM zeroship.refunds
      WHERE invoice_id = NEW.invoice_id AND id <> NEW.id;
    IF NEW.destination = 'cash'   THEN sum_cash   := sum_cash   + NEW.amount_cents; END IF;
    IF NEW.destination = 'credit' THEN sum_credit := sum_credit + NEW.amount_cents; END IF;
    IF sum_cash > cash THEN
        RAISE EXCEPTION 'refund % over-refunds CASH on invoice % (cash refunds % > Σ(invoice_payments) %)',
            NEW.id, NEW.invoice_id, sum_cash, cash;
    END IF;
    IF sum_credit > cash THEN
        RAISE EXCEPTION 'refund % over-refunds CREDIT-DEST on invoice % (credit refunds % > Σ(invoice_payments) %)',
            NEW.id, NEW.invoice_id, sum_credit, cash;
    END IF;
    IF sum_cash + sum_credit > cash THEN
        RAISE EXCEPTION 'refund % over-refunds COMBINED on invoice % (% > Σ(invoice_payments) %)',
            NEW.id, NEW.invoice_id, sum_cash + sum_credit, cash;
    END IF;
    RETURN NEW;
END;
$fn$ LANGUAGE plpgsql;
-- INSERT-only: a refund row's amount/destination never change after claim (no 'void'
-- to flip), so re-checking on UPDATE is unnecessary; the pending→issued status flip
-- does not touch money columns.
CREATE TRIGGER refunds_no_over_refund_trg
    BEFORE INSERT ON zeroship.refunds
    FOR EACH ROW EXECUTE FUNCTION zeroship.refunds_no_over_refund();
--rollback DROP TRIGGER IF EXISTS refunds_no_over_refund_trg ON zeroship.refunds;
--rollback DROP FUNCTION IF EXISTS zeroship.refunds_no_over_refund();

--changeset zeroship:refunds-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    -- refunds: SELECT/INSERT/UPDATE (pending→issued status flip + issued_at). No
    -- DELETE (append-only; a mistaken refund is corrected by a true-up, not deleted).
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.refunds            TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT         ON zeroship.refund_provider_refs TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.refunds FROM zeroship_control'; EXECUTE 'REVOKE ALL ON zeroship.refund_provider_refs FROM zeroship_control'; END IF; END $rb$;
```

### `0050` — `plan_change_events.sql`

```sql
--liquibase formatted sql

--changeset zeroship:plan-change-events splitStatements:true
-- PRORATION (gap #26 D) — FULL USAGE-SEGMENT (round 3, user decision 1 OVERRIDE).
-- Recorded on EVERY set_plan (always, cheap, append-only). App-keyed (the plan is
-- per-app) ⇒ FORCE RLS (uniform with app_spend_*). A mid-period plan change creates
-- ONE row; an app with N change rows in a period splits into N+1 SEGMENTS at month-end,
-- and EACH segment is priced under its own plan and frozen as a SEPARATE invoice line.
--
-- THE HARD PROBLEM this row solves. usage_aggregates is keyed (app_id, period, metric)
-- with a SINGLE running cumulative `total` for the WHOLE calendar month — there is NO
-- per-event timestamp and NO sub-period bucket. So you CANNOT split a period's usage
-- across a pre-change and a post-change plan segment by time-filtering events: the
-- events are gone, only the running total remains.
--
-- THE SOLUTION. Because the counters are MONOTONIC cumulative totals, we SNAPSHOT the
-- app's cumulative usage_aggregates totals (per metric) ONTO this row AT THE CHANGE
-- INSTANT — `usage_at_change` JSONB {metric: cumulative_total_at_change}, read SERVER-
-- SIDE in the SAME txn as the plan flip (so it reflects exactly the running total at the
-- instant the plan switched, frozen, un-racy). At month-end a segment's usage-delta per
-- metric is `(cumulative at segment END) − (cumulative at segment START)` — start = the
-- prior change's snapshot (or 0 for the first segment), end = the next change's snapshot
-- (or the period-end totals for the last segment). The delta is non-negative because the
-- counters only grow. This is the SAME snapshot-onto-line discipline the redesign uses,
-- applied to a CUMULATIVE checkpoint instead of a final total.
--
-- round 1, CRITICAL-4 (creator-writable money fields). set_plan is creator-reachable
-- (api.rs:602 Resource::App fallback), so a creator CAN cause this INSERT. The frozen
-- money fields (from/to_base_fee_cents) AND `usage_at_change` are therefore DERIVED
-- SERVER-SIDE at write time — base fees from PlanCatalog::get(plan_id).base_fee_cents,
-- usage from a server-issued `SELECT metric,total FROM usage_aggregates WHERE app_id=$1
-- AND period=$2` — and are NEVER read from the request body (which carries only
-- plan_id). This is the named, scoped exemption to principle #10: a creator-reachable
-- money write that is safe BECAUSE it is server-derived. Two guardrails bound abuse: a
-- per-period change cap (MAX_PLAN_CHANGES_PER_PERIOD, default 8) and per-app rate-limiting
-- on set_plan.
--
-- round 4, MAJOR-4 (cap-collapse must NOT under-bill — a REAL revenue leak now that FX
-- varies per plan). Past the cap, set_plan must STILL flip apps.plan_id (the creator IS
-- running the new plan) but records NO fresh usage snapshot. The reconciler then prices
-- the over-cap TAIL under the LAST-RECORDED snapshot's plan span up to period-end. The
-- bug to avoid: if the cap froze the plan at the last RECORDED (cheap) plan while the
-- creator actually runs an EXPENSIVE plan with a higher per-plan FX override, the tail
-- would price under the cheap FX = deliberate under-bill. FIX: past the cap, set_plan
-- updates apps.plan_id AND stamps a lightweight marker (apps.plan_id is the source of
-- truth) so the FINAL segment is priced under the ACTUALLY-RUNNING plan, not the last
-- recorded one. Concretely: the reconciler's last segment uses the plan named by the
-- final plan_change_events row IF it equals apps.plan_id, ELSE apps.plan_id (the
-- over-cap flips moved the running plan past the last snapshot). The segments simply
-- MERGE past the cap (no new boundary), but the tail's PLAN is correct. This bounds the
-- under-bill to "the usage-delta and base for the merged tail is priced under one plan
-- (the current one), losing only the intra-tail micro-segmentation" — never under a
-- cheaper-than-running plan.
--
-- round 1, MINOR-1 (proration basis): the BASE-FEE day-weighting uses CALENDAR DAYS in
-- the billing period (days-in-month of `period`) as the denominator; each segment's
-- base-fee weight is its calendar-day span ÷ days-in-month, HALF-UP rounding matching
-- charge_cents's `÷ FX_SCALE` rounding. Segment days sum to the period day count
-- exactly, so the prorated base fees sum to at most one full fee. USAGE, by contrast, is
-- split by the cumulative-snapshot DELTA (above), NOT day-weighted — usage is metered,
-- not assumed-uniform, so a spike lands in whatever segment it actually occurred in.
--
-- round 4, CRITICAL-2 (day-partition rule, PINNED — no shared boundary day, no double
-- count). Segment day-spans are HALF-OPEN calendar-day intervals:
--   segment_days[k] = date_trunc('day', next_effective_at)::date
--                   − date_trunc('day', this_effective_at)::date
-- where this_effective_at = segment k's plan's effective_at (period_start for segment 0),
-- next_effective_at = segment k+1's effective_at, and for the LAST segment
-- next_effective_at := period_end (the first-of-next-month boundary). Each calendar day
-- belongs to EXACTLY ONE segment [start, next) — the change day is owned by the OPENING
-- segment, never shared. Therefore Σ segment_days[k] = days_in_period EXACTLY (it
-- telescopes: (d1−d0)+(d2−d1)+…+(end−dn) = end−d0 = days_in_period), so the prorated base
-- fees sum to ≤ ONE full base fee (BASE-FEE INVARIANT). The leftover cent from rounding N
-- segment base fees is assigned DETERMINISTICALLY to the LAST segment (remainder-cents
-- rule), so Σ(prorated base) is exactly round_half_up(one full fee × days_active/
-- days_in_period) and never exceeds one full fee.
--
-- round 4, MAJOR-3 (delta non-negativity FLOOR). A segment's usage-delta per metric is
-- FLOORED at 0: segment_delta[m] = max(0, end[m] − start[m]). The counters are monotonic
-- so end ≥ start normally, but the floor defends the END-MISSING-METRIC case: a metric
-- present in an earlier `usage_at_change` snapshot whose `usage_aggregates` row is ABSENT
-- at the segment END (e.g. a metric retired mid-period, or a period-end read that returns
-- no row) reads end = 0, which would make end − start NEGATIVE; the floor pins it to 0 so
-- a vanished metric never CREDITS the bill. (App-delete mid-open-period is OUT OF SCOPE:
-- apps.id FK is ON DELETE CASCADE, so deleting an app erases its plan_change_events +
-- usage_aggregates; a half-deleted app mid-period is not a state this reconciler bills.)
CREATE TABLE zeroship.plan_change_events (
    id              TEXT        PRIMARY KEY,            -- pce_<base62>
    app_id          UUID        NOT NULL REFERENCES zeroship.apps(id)  ON DELETE CASCADE,
    period          zeroship.billing_period NOT NULL,  -- first-of-month the change falls in
    from_plan_id    TEXT        REFERENCES zeroship.plans(id) ON DELETE RESTRICT,  -- NULL = initial assignment
    to_plan_id      TEXT        NOT NULL REFERENCES zeroship.plans(id) ON DELETE RESTRICT,
    -- The instant the change took effect, for the base-fee day-fraction. Frozen at
    -- write so a later clock read can't move it.
    effective_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- Frozen base-fee snapshot of BOTH plans at change time (snapshot-onto-line:
    -- proration stays reproducible without reading the live plans table). NULL from-fee
    -- for the initial assignment.
    from_base_fee_cents BIGINT  CHECK (from_base_fee_cents IS NULL OR from_base_fee_cents >= 0),
    to_base_fee_cents   BIGINT  NOT NULL CHECK (to_base_fee_cents >= 0),
    -- THE CUMULATIVE USAGE CHECKPOINT (round 3). {metric: cumulative_total} read from
    -- usage_aggregates in the SAME txn as the plan flip. This is the segment boundary
    -- marker: the END of the segment that just closed and the START of the one opening.
    -- Server-derived, never client-supplied. A metric absent from the JSON is treated as
    -- cumulative 0 at this instant (it had no usage yet). For the initial assignment
    -- (from_plan_id IS NULL) this is typically {} (no usage before the app existed).
    usage_at_change JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX plan_change_events_app_period_idx
    ON zeroship.plan_change_events (app_id, period);
--rollback DROP INDEX IF EXISTS zeroship.plan_change_events_app_period_idx;
--rollback DROP TABLE zeroship.plan_change_events;

--changeset zeroship:plan-change-events-rls splitStatements:true
ALTER TABLE zeroship.plan_change_events ENABLE ROW LEVEL SECURITY;
ALTER TABLE zeroship.plan_change_events FORCE  ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON zeroship.plan_change_events
    USING (app_id = current_setting('zeroship.tenant_app', true)::uuid);
--rollback DROP POLICY IF EXISTS tenant_isolation ON zeroship.plan_change_events;
--rollback ALTER TABLE zeroship.plan_change_events NO FORCE ROW LEVEL SECURITY;
--rollback ALTER TABLE zeroship.plan_change_events DISABLE ROW LEVEL SECURITY;

--changeset zeroship:plan-change-events-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    -- Append-only: SELECT/INSERT, no UPDATE/DELETE.
    EXECUTE 'GRANT SELECT, INSERT ON zeroship.plan_change_events TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.plan_change_events FROM zeroship_control'; END IF; END $rb$;
```

> **Why app-keyed FORCE RLS here (vs creator-keyed exempt for credits/refunds):** a plan is a property of an *app*, and the app-keyed spend tables (`app_spend_limit`/`app_spend_state`/`spend_state_history`) all FORCE RLS on `zeroship.tenant_app`. `plan_change_events` joins that family. Credits and refunds are *creator*-level money facts (a creator holds one balance across all their apps; a refund is against a creator's invoice), so they follow the creator-keyed-exempt posture of `creator_billing`/`invoices`.

### `0051` — `billing_notifications.sql`

```sql
--liquibase formatted sql

--changeset zeroship:billing-notifications splitStatements:true
-- NOTIFICATIONS (gap #26 F). The SEND-LEDGER, round 1 CRITICAL-3: written
-- CLAIM-BEFORE-SEND, TWO-PHASE (status 'pending'→'sent'), so the claim INSERT — not
-- the send — arbitrates the multi-node race. Two instances scanning the same unsent
-- transition both attempt `INSERT … status='pending' ON CONFLICT DO NOTHING RETURNING`;
-- only the winner (returns a row) is cleared to send, then flips to 'sent'. The notify
-- cron ALSO holds the dunning-family advisory lock for the whole sweep (defence in
-- depth). Dedup key = (creator_id, kind, transition_id): the transition_id is the
-- stable identity of the source history row (its surrogate id — see note), so a given
-- transition fires a given notification kind AT MOST ONCE, even across nodes and
-- crashes. Creator-keyed ⇒ NO app RLS. Append-only (a 'pending' row that never sent is
-- re-driven, not deleted; the cron picks up rows whose claimed_at is past
-- NOTIFY_REDRIVE_HORIZON (= 15min, round 2 MAJOR-B) and retries the SEND for the SAME
-- row — never a new claim). round 2, MAJOR-A: the guarantee is at-least-once DELIVERY /
-- exactly-once CLAIM (a crash in the send→flip window re-drives → a second email), made
-- idempotent at the provider by a Mailer Idempotency-Key = (creator_id, kind,
-- transition_id).
CREATE TABLE zeroship.billing_notifications (
    creator_id    UUID NOT NULL REFERENCES zeroship.creator_billing(creator_id) ON DELETE CASCADE,
    -- The notification kind, driven off the source transition:
    --   'payment_failed' | 'past_due' | 'suspended' | 'recovered'  (creator_billing_status_history)
    --   'invoice_finalized' | 'refunded' | 'disputed'              (invoices / refunds / disputes)
    --   'spend_warn' | 'spend_degrade' | 'spend_block'             (spend_state_history)
    kind          zeroship.billing_notification_kind NOT NULL,
    -- The identity of the SOURCE row that triggered this send. For history-row-driven
    -- kinds it is that row's surrogate id; for invoice/refund/dispute kinds it is the
    -- invoice_id / refund_id / dispute_id. TEXT so all sources share one column.
    -- round 1, MINOR-3 (cross-source dedup correctness): the typed-id PREFIXES of the
    -- contributing sources MUST be pairwise-disjoint so a transition_id from one source
    -- can never collide with another's. The sources are: she_… (spend_state_history),
    -- cbh_… (creator_billing_status_history), inv_… (invoices), ref_… (refunds),
    -- dsp_… (disputes) — all distinct prefixes, asserted by a typed_id-registry test.
    transition_id TEXT NOT NULL,
    -- TWO-PHASE STATE (round 1, CRITICAL-3). 'pending' on claim; 'sent' after delivery.
    status        zeroship.notification_status NOT NULL DEFAULT 'pending',
    claimed_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),  -- when the claim INSERT won; drives re-drive
    sent_at       TIMESTAMPTZ,                         -- set on the 'pending'→'sent' flip
    PRIMARY KEY (creator_id, kind, transition_id)
);
--rollback DROP TABLE zeroship.billing_notifications;

--changeset zeroship:billing-notification-kind-domain splitStatements:true
CREATE DOMAIN zeroship.billing_notification_kind AS TEXT
    CHECK (VALUE IN ('payment_failed','past_due','suspended','recovered',
                     'invoice_finalized','refunded','disputed',
                     'spend_warn','spend_degrade','spend_block'));
CREATE DOMAIN zeroship.notification_status AS TEXT
    CHECK (VALUE IN ('pending','sent'));
--rollback DROP DOMAIN IF EXISTS zeroship.notification_status;
--rollback DROP DOMAIN IF EXISTS zeroship.billing_notification_kind;

--changeset zeroship:billing-notifications-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    -- SELECT/INSERT/UPDATE: UPDATE is needed for the 'pending'→'sent' flip (round 1,
    -- CRITICAL-3 two-phase claim). No DELETE (append-only; un-sent rows re-drive).
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.billing_notifications TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.billing_notifications FROM zeroship_control'; END IF; END $rb$;
```

### `0052` — `disputes.sql` <!-- Added in round 1: addressing MISSING-CONCEPT 1 (disputes/chargebacks) -->

```sql
--liquibase formatted sql

--changeset zeroship:billing-disputes splitStatements:true
-- DISPUTES / CHARGEBACKS (round 1, MISSING-CONCEPT 1). A cardholder disputes a charge;
-- Stripe fires `charge.dispute.created` and DEBITS the platform's balance immediately
-- (the funds are held by the network). This is NOT a refund we initiated — it is a
-- forced reversal — so it is its OWN fact, append-only, FK→the disputed invoice. The
-- webhook handler (a new branch in stripe_handlers, claim-after-success on
-- stripe_events_seen as usual) inserts/updates this row off the dispute lifecycle
-- events (created → won/lost). Creator-keyed via the invoice ⇒ NO app RLS.
CREATE TABLE zeroship.billing_disputes (
    id          TEXT        PRIMARY KEY,                -- dsp_<base62> (disjoint prefix, MINOR-3)
    invoice_id  TEXT        NOT NULL REFERENCES zeroship.invoices(id) ON DELETE RESTRICT,
    amount_cents BIGINT     NOT NULL CHECK (amount_cents > 0),  -- disputed amount (network-held)
    currency    CHAR(3)     NOT NULL DEFAULT 'usd' CHECK (currency ~ '^[a-z]{3}$'),
    -- Stripe dispute status, membership only: 'open' (needs_response/under_review),
    -- 'won' (funds returned to platform), 'lost' (chargeback final, funds gone).
    status      zeroship.dispute_status NOT NULL DEFAULT 'open',
    -- The Stripe dispute id (dp_…). A real provider ref, kept inline because a dispute
    -- is ALWAYS provider-originated (no Native dispute concept) — unlike refunds, there
    -- is no provider-agnostic dispute we initiate, so a side table buys nothing.
    provider_dispute_id TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    resolved_at TIMESTAMPTZ,
    UNIQUE (provider_dispute_id)
);
CREATE INDEX billing_disputes_invoice_idx ON zeroship.billing_disputes (invoice_id);
--rollback DROP INDEX IF EXISTS zeroship.billing_disputes_invoice_idx;
--rollback DROP TABLE zeroship.billing_disputes;

--changeset zeroship:dispute-status-domain splitStatements:true
CREATE DOMAIN zeroship.dispute_status AS TEXT
    CHECK (VALUE IN ('open','won','lost'));
--rollback DROP DOMAIN IF EXISTS zeroship.dispute_status;

--changeset zeroship:billing-disputes-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.billing_disputes TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.billing_disputes FROM zeroship_control'; END IF; END $rb$;
```

### `0053` — `invoice_payments.sql` <!-- Added in round 2: addressing CRITICAL-A (cash-collected as an append-only side fact, not a column on the finalized invoice) -->

```sql
--liquibase formatted sql

--changeset zeroship:invoice-payments splitStatements:true
-- PAYMENTS (round 2, CRITICAL-A). Round 1 tried to freeze cash-collected as a column on
-- `invoices` written by the payment webhook — but invoices_immutable() RAISEs on EVERY
-- finalized→finalized UPDATE (only →void with money held equal is legal), so that write
-- is IMPOSSIBLE. This is the doc's own principle applied correctly: a payment is an
-- APPEND-ONLY SIDE FACT against a finalized invoice, not a mutation of it. One row per
-- payment / partial-pay / dispute-clawback. cash_collected(invoice) = Σ(amount_cents)
-- over these rows. The over-refund trigger (0049), the dispute check, the true-up bridge
-- and the read API all read THIS sum. Real FK→invoices(id) ⇒ no orphan payment. Because
-- the invoice row is never touched, the immutability trigger is never even challenged.
-- Creator-keyed via the invoice ⇒ NO app RLS (control is BYPASSRLS), uniform with
-- refunds/disputes. MUST land BEFORE 0049 in the changelog master (0049's trigger
-- function references this table); the master lists 0053 ahead of 0049 with a pinning
-- comment, since Liquibase runs changesets in changelog order, not numeric order.
CREATE TABLE zeroship.invoice_payments (
    id          TEXT        PRIMARY KEY,                -- ipay_<base62> (disjoint prefix)
    invoice_id  TEXT        NOT NULL REFERENCES zeroship.invoices(id) ON DELETE RESTRICT,
    -- SIGNED: a normal payment / partial-pay is POSITIVE (cash came in). A dispute that
    -- claws cash back is NEGATIVE (cash left), so Σ(amount_cents) is the NET cash the
    -- platform currently holds for this invoice — exactly the right over-refund anchor:
    -- a lost dispute lowers refundable cash automatically, no cross-table trigger. (A
    -- 'won' dispute that returns funds appends a compensating POSITIVE row.) A zero
    -- payment is meaningless (CHECK <> 0).
    amount_cents BIGINT     NOT NULL CHECK (amount_cents <> 0),
    currency    CHAR(3)     NOT NULL DEFAULT 'usd' CHECK (currency ~ '^[a-z]{3}$'),
    -- Provenance. 'charge' = invoice.paid / charge.succeeded (full or partial, positive);
    -- 'dispute_debit' = charge.dispute.created clawback (negative); 'dispute_reversal' =
    -- dispute won, funds back (positive). NOTE a FULLY-credited invoice (total_cents=0)
    -- writes NO row — there was no charge — so cash-collected stays 0, exactly right.
    -- Membership only.
    kind        zeroship.invoice_payment_kind NOT NULL,
    -- The provider event that produced this row (pi_…/ch_…/dp_…), for audit + dedup join.
    provider_ref TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX invoice_payments_invoice_idx ON zeroship.invoice_payments (invoice_id);
--rollback DROP INDEX IF EXISTS zeroship.invoice_payments_invoice_idx;
--rollback DROP TABLE zeroship.invoice_payments;

--changeset zeroship:invoice-payment-kind-domain splitStatements:true
CREATE DOMAIN zeroship.invoice_payment_kind AS TEXT
    CHECK (VALUE IN ('charge','dispute_debit','dispute_reversal'));
--rollback DROP DOMAIN IF EXISTS zeroship.invoice_payment_kind;

--changeset zeroship:invoice-payments-immutable splitStatements:false
-- A payment receipt is append-only (uniform with credit_ledger / invoice immutability):
-- a correction is a NEW offsetting row (a negative dispute_debit), never an edit.
CREATE FUNCTION zeroship.invoice_payments_immutable() RETURNS trigger AS $fn$
BEGIN
    RAISE EXCEPTION 'invoice_payments is append-only (no UPDATE/DELETE) — correct via a new offsetting row';
END;
$fn$ LANGUAGE plpgsql;
CREATE TRIGGER invoice_payments_immutable_trg
    BEFORE UPDATE OR DELETE ON zeroship.invoice_payments
    FOR EACH ROW EXECUTE FUNCTION zeroship.invoice_payments_immutable();
--rollback DROP TRIGGER IF EXISTS invoice_payments_immutable_trg ON zeroship.invoice_payments;
--rollback DROP FUNCTION IF EXISTS zeroship.invoice_payments_immutable();

--changeset zeroship:invoice-payments-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    -- INSERT-only beyond SELECT: append-only (the trigger is the backstop).
    EXECUTE 'GRANT SELECT, INSERT ON zeroship.invoice_payments TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.invoice_payments FROM zeroship_control'; END IF; END $rb$;
```

### `0054` — `invoice_lines_segment.sql` (line-grain reshape for usage-segment proration) <!-- Added in round 3: user decision 1 OVERRIDE — full usage-segment proration -->

```sql
--liquibase formatted sql

--changeset zeroship:invoice-lines-segment-reshape splitStatements:true
-- LINE-GRAIN RESHAPE (round 3, decision 1 OVERRIDE — full usage-segment proration).
-- The redesign's invoice_lines PK is (invoice_id, app_id): ONE line per app per invoice.
-- Full usage-segment proration needs MULTIPLE lines per app per invoice — one per plan
-- SEGMENT (an app with N plan-change events in the period has N+1 segments). Add a
-- `segment_no SMALLINT` discriminator to the PK so (invoice_id, app_id, segment_no) is
-- unique: segment_no = 0 is the first (or only) segment, 1 the next, … in effective_at
-- order. Each segment line freezes its OWN plan's snapshot so it replays bit-for-bit.
--
-- WHY A NEW CHANGESET (0054), NOT A RESHAPE-IN-PLACE OF 0042's invoice_lines. The redesign
-- (2026-06-14-billing-schema-redesign.md, score 93/100) is a hardened, separately-reviewed
-- document; this ops-lifecycle proposal is its clean EXTENSION and has consistently
-- expressed every change to a redesign table as a NEW, clearly-attributed changeset
-- (0042's partial-index reshape is the one surgical exception, and it is a constraint
-- swap, not a PK widening). Widening invoice_lines's PRIMARY KEY + retargeting the
-- billing_line_provider_refs COMPOSITE FK is a structural change that two FK-bound tables
-- depend on; doing it as a self-contained 0054 (a) keeps the redesign's 0042 authoritative
-- and un-edited for its own reviewers, (b) makes the proration feature land/rollback as
-- ONE reversible unit, and (c) respects this doc's own changelog-ORDER discipline (the
-- 0053-before-0049 pin). 0054 lands AFTER 0042/0053 in the changelog (it ALTERs tables
-- 0042 created); since it is pre-launch on a clean re-migrate, the ALTER is applied to a
-- freshly-created table with no rows, so there is no data backfill.
--
-- NOTE the segment_no default 0: every existing/no-change line is segment 0, so the
-- no-change path (0 plan-change events) produces exactly ONE line per app — segment_no=0,
-- full-period usage, no proration — byte-for-byte identical to today. The reshape
-- DEGENERATES cleanly.

-- 1. Drop the composite FK on billing_line_provider_refs FIRST (it references the PK we
--    are about to widen). round 4, MINOR-7: the FK is now EXPLICITLY NAMED in 0042 —
--    `billing_line_provider_refs_line_fk` (a one-line edit to 0042's inline FOREIGN KEY,
--    exactly like the surrogate-id ALTERs this doc already asks of the redesign author):
--      0042: CONSTRAINT billing_line_provider_refs_line_fk
--              FOREIGN KEY (invoice_id, app_id)
--              REFERENCES zeroship.invoice_lines(invoice_id, app_id) ON DELETE CASCADE
--    so 0054 DROPs a KNOWN name, not a Postgres-guessed one. The DROP is name-pinned (no
--    IF EXISTS), so a name skew fails the migration LOUDLY rather than silently leaving the
--    old 2-col FK in place (which would let a per-segment ref point at a non-existent line).
ALTER TABLE zeroship.billing_line_provider_refs
    DROP CONSTRAINT billing_line_provider_refs_line_fk;

-- 2. Widen invoice_lines: add segment_no, re-key the PK to include it.
ALTER TABLE zeroship.invoice_lines
    ADD COLUMN segment_no SMALLINT NOT NULL DEFAULT 0 CHECK (segment_no >= 0);
-- round 4, MAJOR-5: plan_id is NOT NULL. There is no legacy pre-launch (no published
-- users, no existing rows), so "nullable for legacy" is dead surface. Every segment line
-- — including the degenerate segment_no=0 single line on the no-change path — carries the
-- plan it was priced under. The reconciler ALWAYS writes it (the N=0 path writes the app's
-- current apps.plan_id at segment_no=0). NOT NULL is added in the SAME statement as the
-- column so the freshly-created, empty invoice_lines accepts it with no backfill.
ALTER TABLE zeroship.invoice_lines
    ADD COLUMN plan_id TEXT NOT NULL REFERENCES zeroship.plans(id) ON DELETE RESTRICT;  -- the segment's plan; always set by the reconciler (NOT NULL — no legacy pre-launch)
ALTER TABLE zeroship.invoice_lines DROP CONSTRAINT invoice_lines_pkey;
ALTER TABLE zeroship.invoice_lines
    ADD CONSTRAINT invoice_lines_pkey PRIMARY KEY (invoice_id, app_id, segment_no);

-- 3. Widen billing_line_provider_refs to carry segment_no and re-add the composite FK to
--    the new 3-col PK, so a per-segment provider ref (each segment is its own Stripe
--    invoice_item) cannot reference a non-existent (invoice, app, segment) line.
ALTER TABLE zeroship.billing_line_provider_refs
    ADD COLUMN segment_no SMALLINT NOT NULL DEFAULT 0 CHECK (segment_no >= 0);
ALTER TABLE zeroship.billing_line_provider_refs DROP CONSTRAINT billing_line_provider_refs_pkey;
ALTER TABLE zeroship.billing_line_provider_refs
    ADD CONSTRAINT billing_line_provider_refs_pkey
    PRIMARY KEY (invoice_id, app_id, segment_no, provider, ref_kind);
ALTER TABLE zeroship.billing_line_provider_refs
    ADD CONSTRAINT billing_line_provider_refs_line_fk
    FOREIGN KEY (invoice_id, app_id, segment_no)
    REFERENCES zeroship.invoice_lines(invoice_id, app_id, segment_no) ON DELETE CASCADE;
--rollback ALTER TABLE zeroship.billing_line_provider_refs DROP CONSTRAINT billing_line_provider_refs_line_fk;
--rollback ALTER TABLE zeroship.billing_line_provider_refs DROP CONSTRAINT billing_line_provider_refs_pkey;
--rollback ALTER TABLE zeroship.billing_line_provider_refs DROP COLUMN segment_no;
--rollback ALTER TABLE zeroship.billing_line_provider_refs ADD CONSTRAINT billing_line_provider_refs_pkey PRIMARY KEY (invoice_id, app_id, provider, ref_kind);
--rollback ALTER TABLE zeroship.invoice_lines DROP CONSTRAINT invoice_lines_pkey;
--rollback ALTER TABLE zeroship.invoice_lines ADD CONSTRAINT invoice_lines_pkey PRIMARY KEY (invoice_id, app_id);
--rollback ALTER TABLE zeroship.invoice_lines DROP COLUMN plan_id;
--rollback ALTER TABLE zeroship.invoice_lines DROP COLUMN segment_no;
--rollback ALTER TABLE zeroship.billing_line_provider_refs ADD CONSTRAINT billing_line_provider_refs_line_fk FOREIGN KEY (invoice_id, app_id) REFERENCES zeroship.invoice_lines(invoice_id, app_id) ON DELETE CASCADE;
```

> **What each segment line freezes (snapshot-onto-line, per segment).** A segment line carries the existing snapshot block PLUS the per-segment discriminators, so each segment replays bit-for-bit by re-running `charge_cents` over its own frozen inputs: `{segment_no, plan_id (NOT NULL — round 4, MAJOR-5), usage_snapshot = the segment's usage-DELTA (not the cumulative), weights_snapshot = the GLOBAL applied weights (weights are global, NOT per-plan), included_units = that plan's quota PRO-RATED by segment-days (see policy below), fx_pico_cents_per_unit = the segment's plan's resolved FX (the PER-PLAN override lever), base_fee_cents = the segment's plan base fee PRO-RATED by segment-days (passed via price.base_fee_cents; charge_cents adds it INTERNALLY), amount_cents = charge_cents(...) for THIS segment}`. Note `charge_cents`'s contract: it takes `price.base_fee_cents` (pro-rated base) and `price.fx_pico_cents_per_unit` (the per-plan FX) and the GLOBAL `weights` separately, and adds the base fee internally — so the per-segment pro-rated base is passed as `price.base_fee_cents`, never added a second time. The `usage_snapshot` is the delta `(cumulative at segment end) − (cumulative at segment start)` per metric — so summing the segments' `usage_snapshot` maps reconstructs the full-period usage exactly (telescoping sum of cumulative checkpoints). No line stores a cumulative total; each stores only its own segment's metered slice, which is the source of truth for that line's charge.

> **Why a signed `invoice_payments` side table instead of patching the trigger (PRODUCT DECISION 12).** The alternative — keep `cash_collected_cents` on `invoices` and amend `invoices_immutable()` to permit a finalized→finalized UPDATE iff the *only* changed column is `cash_collected_cents` (monotonic `OLD <= NEW <= total_cents`, all other money columns held equal) — *would* work mechanically, but it **weakens the very invariant the redesign hardened over three rounds**: it opens a second legal finalized→finalized transition, forces the void path to reason about the new column, and makes "a finalized invoice is immutable" no longer literally true. The side table is strictly better: the invoice stays byte-for-byte frozen, `invoices_immutable()` is untouched, partial pays and dispute clawbacks are *naturally* additional rows (a single column would need its own monotonicity story for a clawback — a *decrease* the monotonic CHECK would forbid), and it is the same append-only-side-fact shape as `credit_ledger` / `refunds` / `billing_disputes`. The signed-row design also makes the dispute-aware cap fall out for free: a `dispute_debit` row lowers `Σ(invoice_payments)`, so the over-refund trigger *automatically* sees less refundable cash — the cross-table dispute check (below) becomes a belt-and-braces double-check rather than the sole guard.

> **Disputes interact with already-credited/refunded invoices (the double-recovery guard).** <!-- Round 2, CRITICAL-A: dispute clawback is now a negative invoice_payments row, so it lowers Σ(invoice_payments) — the over-refund anchor — automatically. --> A dispute reverses cash the cardholder already paid — so it consumes the SAME cash budget the refund cap (CRITICAL-1) protects. When `charge.dispute.created` lands, the handler treats the disputed amount as cash *clawed back*: it does NOT auto-issue a refund (the network already pulled the funds), and it appends a **negative `invoice_payments` row** (`kind='dispute_debit'`, `amount_cents = −disputed`). Because the over-refund trigger reads `Σ(invoice_payments)`, the refundable-cash budget is **automatically reduced** by the dispute — no cross-table trigger required (this is the round-2 win of the signed side table). The operator endpoint *additionally* double-checks `Σ(cash refunds) ≤ Σ(invoice_payments)` before claiming a `destination='cash'` refund (belt-and-braces; the trigger is the authoritative guard). If a dispute is later `won`, the handler appends a compensating **positive `dispute_reversal` row**, restoring the budget. A `disputed` notification fires once (dedup on `dsp_…` transition_id). An invoice that was already `refund_to_credit`-granted and is THEN disputed does not double-pay: the credit grant was funded by cash the dispute now claws back, so the operator is alerted to claw back the credit via a `consumed`/offsetting entry (manual, audited) — surfaced by the outstanding-credit-liability metric.

> **`transition_id` requires a stable id on the history rows (round 1, MINOR-4 — surrogate-id mechanism + cron audit).** The redesign's `spend_state_history` and `creator_billing_status_history` are written as bare append-only rows with no surrogate PK. For the send-ledger dedup to be exact, each gets a surrogate `id TEXT PRIMARY KEY` (a `she_…` / `cbh_…` typed id) added in the SAME PR-6 changeset (a tiny `ALTER TABLE … ADD COLUMN id` against the redesign tables — pre-launch, so edited in place; call out to the redesign author). **DEFAULT mechanism:** these ids are minted in Rust by the existing `typed_id` generator (UUIDv7 + base62 + prefix), not by a SQL `DEFAULT` — there is no in-DB base62 generator, so the column is `NOT NULL` with the value bound by the INSERT in `spend.rs::persist_transition` / `account_status.rs::*record*` (both already do a single-row INSERT, so adding one bound column is mechanical). A `gen_random_uuid()::text` SQL DEFAULT is rejected: it would not carry the `she_`/`cbh_` prefix the disjointness assertion (MINOR-3) relies on. **Cron audit:** the two crons that scan these tables — `dunning.rs` (reads `creator_billing_status`) and the new `billing_notify` cron — are audited to confirm neither selects `*` into a fixed-arity struct that a new column would break (both name columns explicitly; verified). The `spend_reconcile`/`billing_reconcile` crons do not read these history tables. Without the surrogate id, the notifier would have to dedup on `(creator_id, kind, at)`, which is not collision-proof (two transitions can share a timestamp at clock resolution).

---

## Table-by-table rationale

- **`invoices` (reshape).** ONE change (round 2, CRITICAL-A — the round-1 second change is withdrawn): the partial unique index `WHERE status <> 'void'` turns the legal-but-inert `void` transition into a usable correction path (voiding releases the period claim, so a re-priced reissue can take the slot; unbounded void rows are the audit trail). The round-1 `cash_collected_cents` column is **removed** — writing it onto a finalized invoice was rejected by `invoices_immutable()`; cash-collected moved to the `invoice_payments` side table. The committed `invoices_immutable()` is left untouched.
- **`invoice_payments` (new, `0053`).** <!-- Added in round 2: CRITICAL-A --> The cash-collected anchor for the over-refund cap, as an append-only side fact rather than a column on the frozen invoice. Signed `amount_cents` (positive charge / partial-pay; negative `dispute_debit`; positive `dispute_reversal`), so `Σ(invoice_payments)` is the net cash the platform currently holds for the invoice — read by the over-refund trigger, the dispute check, the true-up bridge, and the read API. Append-only by trigger + INSERT-only grant. The invoice row is never touched, so the immutability trigger is never challenged — the doc's core principle, applied to payments.
- **`credit_ledger` (new).** The credit primitive. SUM-of-entries (no stored balance) makes the balance un-clobberable; **per-grant** FIFO consume (one `consumed` entry per drawn grant, `consumed_from_grant_id`, `ORDER BY created_at`) matches Stripe's oldest-first application AND makes credit-expiry attribution exact (round 1, MAJOR-2). A kind↔sign CHECK (MAJOR-1) makes the SUM invariant a DB fact. `void_reversal` (CRITICAL-2) conserves balance across a void; `refund_to_credit` is the credit-destination refund landing. Operator grants carry an idempotency key (MISSING-7). Append-only by trigger + INSERT-only grant. Optional `expires_at` is the launch hook for promo credits.
- **`refunds` + `refund_provider_refs` (new).** A refund is a side fact against a *paid* invoice, never an edit of it. The real FK to `invoices(id)` + the over-refund trigger make the three-bound cap a DB invariant, **anchored on `Σ(invoice_payments)`** (round 1 CRITICAL-1; round 2 CRITICAL-A — the anchor is now a sum over the side table, not a frozen column). `amount_cents` splits into `subtotal_cents`/`tax_cents` for proportional tax-on-refund (MISSING-6). `idempotency_key` (UNIQUE) + `request_fingerprint` kill double-refunds from retried operator calls and reject key-reuse-with-different-body (MISSING-7, round 2 MINOR-A). The provider-ref `ref_kind` allows `'refund'` (re_…, the cash money-movement object), `'credit_note'` (cn_…, optional bookkeeping), and `'customer_balance_txn'` — written claim-after-success, idempotent across crashes via the Stripe `Idempotency-Key`. `destination` routes to cash (a Stripe `Refund` re_…) or balance (a `credit_ledger refund_to_credit` grant). `'void'` status removed (MINOR-2 — cash refunds are irreversible).
- **`billing_disputes` (new, `0052`).** Chargebacks (round 1, MISSING-1). Forced cash reversals Stripe originates (`charge.dispute.created`), append-only, FK→invoice. Reduces the refund cap's effective cash budget so a disputed-then-refunded invoice can't double-pay. `dp_…` ref inline (disputes are always provider-originated; no Native dispute). Disjoint `dsp_…` typed-id prefix for the notify dedup key.
- **`plan_change_events` (new).** The proration timeline. Recorded on every `set_plan` (cheap, append-only). round 3 (decision 1 OVERRIDE): it carries a **`usage_at_change` JSONB cumulative-usage checkpoint** read server-side in the plan-flip txn, which is what makes FULL usage-segment proration possible despite `usage_aggregates` holding only a single running monthly total per metric — at month-end an app splits into N+1 segments whose usage is the cumulative DELTA between consecutive snapshots, each priced under its own plan and frozen as a separate `invoice_lines` segment line (the `0054` line-grain reshape). Frozen `from/to_base_fee_cents` keep the base-fee day-proration reproducible without reading the live `plans` table (snapshot discipline). App-keyed FORCE RLS, joining the spend-table family.
- **`invoice_lines` line-grain reshape (`0054`, new).** <!-- round 3: decision 1 OVERRIDE --> Widens the `invoice_lines` PK with `segment_no` (and adds per-segment `plan_id`) so an app can have MULTIPLE lines per invoice — one per plan-segment — and retargets the `billing_line_provider_refs` composite FK to the new 3-column line key. A new changeset (not an edit to the redesign's `0042`) so the proration feature lands/rolls back as one unit and the redesign stays authoritative for its own reviewers; lands after `0042`/`0053` in the changelog (it ALTERs tables those created). Degenerates to one `segment_no=0` line per app when there is no plan change — byte-for-byte identical to today.
- **`billing_notifications` (new).** The send-once ledger. Claim-after-success on `(creator_id, kind, transition_id)` is the exact `stripe_events_seen` pattern: a send that failed has no row and is retried; a send that succeeded is never repeated. Creator-keyed, append-only.
- **`credit_entry_kind` / `refund_destination` / `refund_status` / `billing_notification_kind` / `notification_status` / `dispute_status` (new domains).** Membership-only TEXT domains, mirroring the redesign's `spend_state`/`account_state`/`invoice_status` — ordering and legality live in Rust, the DB encodes the set. (Note: `credit_ledger`'s kind↔sign coupling is a TABLE CHECK, not the domain — the domain stays a pure set; round 1, MAJOR-1.)

---

## Key flows

**A. Credit-apply at finalize** (`billing_reconcile.rs::bill_creator`, per-creator advisory-locked). The reconciler already computes the per-app `ChargeBreakdown` and snapshots inputs onto each line, then finalizes the invoice in ONE UPDATE (`subtotal/credit/tax/total/status/finalized_at`). The credit step slots in just before that UPDATE, inside the same `conn.transaction()`:
  1. Read the creator's **consumable, non-expired, same-currency grants oldest-first** (round 1, MINOR-5 + MAJOR-2): `SELECT id, (amount_cents + COALESCE(drawn,0)) AS remaining FROM credit_ledger g LEFT JOIN (per-grant drawn sums) WHERE g.creator_id=$1 AND g.kind <> 'consumed' AND g.amount_cents > 0 AND g.currency = $invoice_currency AND (g.expires_at IS NULL OR g.expires_at > NOW()) ORDER BY g.created_at` — the `currency = invoice.currency` filter is mandatory **even in v1** (USD-pinned), so a stray non-USD grant can never be drawn against a USD bill.
  2. `applied_credit = min(Σ remaining, subtotal_cents)` — credit applied to the **subtotal BEFORE tax**, matching the balance CHECK's `total = subtotal − credit + tax` ordering (you credit the pre-tax amount; tax is computed on the post-credit base by the tax seam — see flow E).
  3. **Per-grant FIFO drawdown (round 1, MAJOR-2 + DECISION 7):** draw `applied_credit` from the oldest grants in turn, appending **one `consumed` entry PER drawn grant**: `INSERT credit_ledger (id, creator_id, 'consumed', -drawn_from_this_grant, applied_invoice_id=invoice_id, consumed_from_grant_id=grant.id)`. This is what makes credit-expiry attribution exact — each `consumed` entry names the grant it drew from, so "which grants were spent before they expired" is answerable. A single aggregate companion CANNOT answer that once `expires_at` exists; the earlier draft's "single aggregate companion is exact" claim was false and is replaced. (Cost: O(grants-drawn) rows per finalize, typically 1–2.)
  4. The finalize UPDATE writes `credit_cents = applied_credit` (replacing today's hard-wired `credit_cents = 0`) and `total_cents = subtotal − applied_credit + tax` — still ONE statement, so the CHECK never sees a half-written row.

  Because credit is an *input* to the frozen total (not a later edit), the finalized invoice still replays bit-for-bit and the immutability trigger never fires.

**B. Refund a finalized invoice** (`refund.rs`, operator-only, claim-then-call, idempotency-keyed). Operator calls `POST /invoices/{id}/refunds {amount_cents, subtotal_cents, tax_cents, destination, reason, Idempotency-Key: <hdr>}` (`Action::BillingWrite` on `Resource::Any`). round 1 — CRITICAL-5 (correct Stripe object), CRITICAL-1 (`Σ(invoice_payments)` cap; round 2, CRITICAL-A), tax-on-refund, idempotency (round 2, MINOR-A — body fingerprint):
  1. **Idempotency precheck (round 2, MINOR-A — fingerprint):** compute `request_fingerprint = sha256(invoice_id ∥ amount_cents ∥ subtotal_cents ∥ tax_cents ∥ destination)`, then `INSERT refunds (…, idempotency_key, request_fingerprint) ON CONFLICT (idempotency_key) DO NOTHING RETURNING id, request_fingerprint`. 0 rows ⇒ a key hit ⇒ read the existing row: if its stored `request_fingerprint` **matches**, return the existing refund (safe retry, no second refund); if it **differs**, return `409 idempotency-key-reuse-conflict` (the caller reused a key with a different body — mirroring Stripe's 400, verified at `docs.stripe.com/api/idempotent_requests`: "compares incoming parameters … and errors if they're not the same"). The prior draft returned the first refund regardless of body — a silent bug-masker.
  2. **Claim:** the same INSERT carries `(id, invoice_id, amount_cents, subtotal_cents, tax_cents, destination, 'pending', idempotency_key, request_fingerprint)`. The over-refund trigger rejects it unless all three bounds hold (`Σcash`, `Σcredit`, combined `≤ Σ(invoice_payments)` — round 2 CRITICAL-A: the anchor is the payment-sum, not a column). The endpoint additionally double-checks `Σ(cash refunds) ≤ Σ(invoice_payments)` for `destination='cash'` (see Disputes — dispute clawbacks already lower the sum). The intent row is the durable claim.
  3. **Call** via the `RefundProvider` seam, split by destination (verified against the Stripe API):
     - **`destination='cash'`** → create a Stripe **`Refund` (`re_…`)** on the paid invoice's charge/PaymentIntent (resolved from `billing_provider_refs(provider='stripe', ref_kind='invoice')` → the invoice's `charge`/`payment_intent`). The `Refund` is created with the Stripe **`Idempotency-Key`** header = a deterministic key derived from `refund.id`, so a crash-retry never double-refunds at Stripe. We MAY additionally `POST /credit_notes {invoice, refund: re_…, lines:[… tax_amounts …]}` purely to emit the customer's credit-note PDF — but the `re_…` is the authoritative money ref. **Tax-on-refund:** the credit-note line (if emitted) and the books split `subtotal_cents`/`tax_cents`; the `Refund` amount is the full `amount_cents` (Stripe refunds the gross charge).
     - **`destination='credit'`** → **no charge reversal.** Append a `credit_ledger('refund_to_credit', +amount_cents)` grant (platform-native). (Optionally, to mirror on Stripe, a `credit_amount` customer-balance transaction / credit note with `credit_amount`, yielding a `cbtxn_…` ref — but v1 is platform-native, no Stripe call needed.)
  4. **Record claim-after-success:** on success, in one txn: `INSERT refund_provider_refs(refund_id,'stripe','refund', re_…)` (and/or `'credit_note', cn_…`), flip `refunds.status='issued'`/`issued_at=NOW()`, and for `destination='credit'` the `refund_to_credit` grant. A crash before this leaves `status='pending'` + no ref ⇒ re-drive re-issues (idempotent on the deterministic Stripe `Idempotency-Key` — Stripe returns the SAME `re_…`, mirroring `find_invoice_item_by_key`); `status='issued'` + ref present ⇒ skip.
  The invoice is NEVER mutated. The refund is the new fact.

**C. Void + reissue** (`void_reissue.rs`, operator-only). To correct a wrong finalized invoice. round 1 — CRITICAL-2 (conserve consumed credit), MAJOR-3 (advisory lock):
  **Lock first.** The whole void+reissue runs under the **same per-creator reconcile advisory lock** the cron's `bill_creator` takes, so it cannot race a concurrent reconcile claim/finalize for the same creator (the redesign's `invoice_lines_immutable` comment explicitly demands every non-cron line writer take this lock; void+reissue IS a non-cron line writer). Acquire `pg_advisory_xact_lock(<per-creator key>)` at the top of the txn.
  1. **Void + RESTORE consumed credit, in ONE txn (CRITICAL-2):** before/with the `finalized → void` transition (the committed `invoices_immutable()` permits it with the four money columns `subtotal/credit/tax/total` held equal — round 2, CRITICAL-A: there is NO `cash_collected_cents` column to hold equal, since payments live in the `invoice_payments` side table; the void leaves those payment rows in place as the voided invoice's permanent cash record; `voided_at=NOW()`), append a compensating positive entry for every credit the voided invoice consumed:
     ```sql
     INSERT INTO zeroship.credit_ledger (id, creator_id, kind, amount_cents,
                                         currency, applied_invoice_id, consumed_from_grant_id)
     SELECT zeroship_typed_id('crd'), c.creator_id, 'void_reversal', -c.amount_cents,
            c.currency, c.applied_invoice_id, c.consumed_from_grant_id
       FROM zeroship.credit_ledger c
      WHERE c.applied_invoice_id = $voided_invoice_id AND c.kind = 'consumed';
     ```
     `-c.amount_cents` flips the negative `consumed` back to a positive restore (each `consumed` row is negative, so `-amount` is positive — matching the kind↔sign CHECK for `void_reversal`). Balance is now conserved: `Σ` over the ledger is exactly what it was before the voided invoice consumed anything. Without this, the reissue (next step) re-consumes from a balance that is short by the voided invoice's credit → silent money loss + double-consume. (The `id` minting uses the typed-id generator; `zeroship_typed_id` shown for brevity.)
  2. **Reissue:** re-run the reconciler's per-creator path for the same `(creator, period)`, STILL holding the lock. The claim `INSERT … 'draft' ON CONFLICT (creator_id, period) WHERE status <> 'void' DO NOTHING` now succeeds (the void row doesn't block it), mints a fresh `inv_…`, re-prices from the live aggregates + (corrected) weights, re-consumes credit from the now-restored balance, snapshots onto the new lines, and finalizes. Reproducible because it re-runs `charge_cents` over a fresh snapshot; balance-conserving because step 1 restored what step 2 re-draws.
  The old invoice survives as the voided audit record; the new invoice is the authoritative bill.
  *Regression intent (CRITICAL-2):* grant $10 → invoice A consumes $6 (balance $4) → void A (balance back to $10 via `void_reversal +$6`) → reissue A′ re-consumes $6 (balance $4). The net balance after the whole sequence equals the balance had A been correct the first time — never $-2 (the double-consume bug).

**D. Proration on plan change** (`api.rs::set_plan` → write the snapshot; `bill_creator` → split into segments at month-end). round 1, CRITICAL-4 — server-derived money + change cap; round 3 — FULL usage-segment proration (decision 1 OVERRIDE). Every `set_plan` (after the existing operator-vs-creator authz probe at `api.rs:602`) appends a `plan_change_events` row that snapshots BOTH the plan base fees AND the cumulative usage at the change instant, **unless** the per-period change cap is already hit:
  - The row's money fields are **derived server-side**: `from_base_fee_cents = PlanCatalog::get(current_plan_id).base_fee_cents`, `to_base_fee_cents = PlanCatalog::get(body.plan_id).base_fee_cents`. The request body carries ONLY `plan_id` — the creator cannot supply a base fee.
  - **The cumulative usage checkpoint** is read SERVER-SIDE in the SAME txn as the `apps.plan_id` flip: `SELECT metric, total FROM usage_aggregates WHERE app_id=$1 AND period=$2` → frozen as `usage_at_change` JSONB `{metric: cumulative_total}`. Reading it inside the plan-flip txn pins the segment boundary to exactly the running total at the instant the plan switched (no race with a concurrent ingest). `period = first-of-month(now)`, `effective_at=NOW()`.
  - **Cap (round 4, MAJOR-4 — must not under-bill):** if the app already has `MAX_PLAN_CHANGES_PER_PERIOD` (default 8) rows for this period, `set_plan` still switches `apps.plan_id` but records **no new** `plan_change_events` row — so a creator cannot manufacture an unbounded number of favorable micro-segments. The over-cap tail **merges** into the final segment, which runs from the last recorded snapshot to period-end. **The merged tail is priced under the ACTUALLY-RUNNING plan (`apps.plan_id`), not the last *recorded* plan.** The naive "price the tail under the last recorded (possibly cheap) plan" is a real revenue leak now that FX varies per plan: a creator could change to an expensive high-FX plan past the cap and have the tail priced at the cheap recorded FX. So past the cap, `apps.plan_id` is the source of truth and the reconciler's last segment uses it (see the cap-collapse rule in the proration section). The merge loses only intra-tail micro-segmentation, never the correct tail *plan*. Per-app rate-limiting on `set_plan` is the second guardrail.
  At month-end, `bill_creator` reads the period's plan-change rows for each app, **ordered by `effective_at`**, and splits the period into **N+1 segments** (N = change rows). For each segment it builds a SEPARATE `invoice_lines` row (`segment_no` 0,1,…):
  - **Segment usage** (per metric) = `max(0, (cumulative at segment END) − (cumulative at segment START))`, where START = the prior change's `usage_at_change` snapshot (or 0 for segment 0) and END = the next change's `usage_at_change` snapshot (or the period-end `usage_aggregates` totals for the last segment). The counters only grow, so end ≥ start normally; the `max(0, …)` floor (round 4, MAJOR-3) defends the **END-missing-metric** case — a metric present in an earlier snapshot whose `usage_aggregates` row is absent at the segment end reads end = 0 and would otherwise produce a NEGATIVE delta that credits the bill. The floor pins it to 0. This delta is the segment's `usage_snapshot`.
  - **Segment price** = `charge_cents(segment_plan.weights, segment_usage_delta, segment_plan.fx, segment_included_units, segment_base_fee)` — each segment priced under ITS OWN plan's weights/FX/included_units, with the per-metric floor applied PER SEGMENT (same `total_units = Σ_m floor(...)` discipline, just over the segment's delta).
  - **Segment base fee** = the plan's `base_fee_cents × (segment_days / days_in_month)`, half-up rounding matching `charge_cents`.
  - **Segment included_units** = the plan's `included_units × (segment_days / days_in_month)`, pro-rated by segment-days (the recommended policy — see the dedicated section). Each segment's delta is charged against ITS OWN pro-rated quota.
  Each segment line freezes `{segment_no, plan_id, usage_snapshot=delta, weights_snapshot, included_units (pro-rated), fx, base_fee (pro-rated), amount_cents}` so it replays bit-for-bit. The **no-change case (0 rows)** produces exactly ONE line per app (`segment_no=0`, full-period usage, full base fee, full quota) — byte-for-byte identical to today. The full mechanism, the day-count basis, the `included_units` policy and edge cases, the reconcile composition, the idempotency/re-run argument, and a worked two-segment trace are in the **"Full usage-segment proration"** section below.

**E. Tax at finalize (seam)** (`TaxProvider`, PR-5, no changeset). Just before the finalize UPDATE (after credit is applied), the reconciler calls `TaxProvider::compute(post_credit_subtotal, creator, period) -> tax_cents`. `NativeTaxProvider` returns `0` (USD launch, no tax). The result is written into the finalize UPDATE's `tax_cents` (replacing today's hard-wired `0`) so `total = subtotal − credit + tax` holds in one statement. Enabling Stripe Tax later is `build_tax_provider` returning a `StripeTaxProvider` that calls Stripe `automatic_tax` — **no schema change**, because `tax_cents` already exists.
  **Tax-on-refund (round 1, MISSING-CONCEPT 6).** A refund of a taxed invoice must return *proportional* tax. The operator endpoint computes the refund's tax split as `tax_cents = round_half_up(amount_cents × invoice.tax_cents / invoice.total_cents)` and `subtotal_cents = amount_cents − tax_cents` (or the operator supplies the split explicitly), stored on the `refunds` row. The Stripe credit-note line (when emitted) carries the matching `tax_amounts`/`tax_rates` so Stripe's tax-reporting books reverse the right amount. For the USD launch (tax = 0) the split is degenerate (`tax_cents = 0`), but the column + computation exist so enabling Stripe Tax is a provider swap, not a refund-schema reshape.

**F. Notification send** (`cron/billing_notify.rs`, ~5min tick, NEW cron). round 1, CRITICAL-3 — multi-node-safe claim-BEFORE-send. round 2, MAJOR-B — the re-drive horizon is the named constant **`NOTIFY_REDRIVE_HORIZON = 15min`** (≥ 3 cron ticks, so a crashed send is retried but a still-in-flight send is not prematurely re-driven). The cron sends via the `BillingNotifier` seam over `Mailer`:
  0. **Advisory lock the sweep.** Acquire a dedicated `pg_try_advisory_lock` on a notify-specific key (`0x7a73_6e6f_7466_0001`, "zsnotf"-family, distinct from dunning/spend/export/reconcile keys) on a dedicated connection for the whole tick — exactly as `dunning.rs` does. A loser instance skips this tick. This alone makes the send single-flight; the two-phase claim below is defence in depth (and covers the lock-handoff window).
  1. **Scan unsent transitions:** `creator_billing_status_history` (`payment_failed`/`past_due`/`suspended`/`recovered`), `spend_state_history` (`warn`/`degrade`/`block`), newly-`finalized` invoices, newly-`issued` refunds, newly-`open` disputes — each LEFT JOIN `billing_notifications` on `(creator_id, kind, transition_id)` for `status` NULL (never claimed) OR (`status='pending'` AND `claimed_at < now() - NOTIFY_REDRIVE_HORIZON`) (claimed but crashed before send).
  2. **Claim BEFORE send:** `INSERT billing_notifications (creator_id, kind, transition_id, status='pending', claimed_at=NOW()) ON CONFLICT (creator_id, kind, transition_id) DO NOTHING RETURNING ...`. **The row that RETURNS is the only one cleared to send.** A concurrent instance (in the lock-handoff window) that loses the INSERT gets 0 rows and does NOT send. For a re-drive of a stale `pending` row, the cron instead `UPDATE … SET claimed_at=NOW() WHERE status='pending' AND claimed_at < now() - NOTIFY_REDRIVE_HORIZON RETURNING` to re-take it.
  3. **Send:** resolve the creator's email (via `users`), render the template, `BillingNotifier::send` (calls `Mailer::send`, honouring the suppression-list contract). On success, flip `UPDATE billing_notifications SET status='sent', sent_at=NOW()`. On failure, leave `status='pending'` (the next tick past `NOTIFY_REDRIVE_HORIZON` re-drives the SAME row — never a new claim).
     **The exactly-once boundary (round 2, MAJOR-A — corrected).** The two-phase claim gives exactly-once **CLAIM**, not exactly-once **DELIVERY**: a crash in the window *after* `Mailer::send` succeeds but *before* the `status='sent'` flip commits leaves a `pending` row that re-drives past the horizon → a **second email** to the same address. So the honest guarantee is **at-least-once delivery, exactly-once claim** (the round-1 "exactly-once effect even across nodes" was overstated for the send→flip window). To make the delivery *effect* idempotent and recover effectively-once-with-caveat, `Mailer::send` carries a **provider-side idempotency key** — `idempotency_key = (creator_id, kind, transition_id)` (the same tuple as the claim PK), mirroring the Stripe `Idempotency-Key` pattern this doc already uses for refunds. A provider that honours it (Resend/SES via a per-message dedup id) drops the duplicate, so the re-driven send is a no-op at the provider and the recipient sees one email. The `stdout`/SMTP dev drivers do not dedup, so in dev the send→flip window can still double-print — documented, not a launch blocker. Net: **at-least-once delivery, exactly-once claim, and an idempotent delivery effect on any provider that honours the key** — because the claim INSERT, not the send, arbitrates the multi-node race, and the provider key arbitrates the send→flip dup window.

**G. Creator read APIs** (`api.rs`, `BillingRead`). Creator-scoped read endpoints, each gated `authz.require(Action::BillingRead, Resource::App{id}, &state)` for owned apps (the same membership gate `get_spend_limit` uses), with operators reading any via the `Resource::Any` probe-first idiom (`is_operator` fallback, exactly as `set_plan` does):
  - `GET /apps/{id}/invoices` — invoice history (id/period/status/total/finalized_at) for the owning creator, owned-apps only.
  - `GET /invoices/{id}` — frozen-snapshot line detail (`usage_snapshot`/`weights_snapshot`/`amount_cents` per line + credit/tax/total), authz'd via the invoice's creator → app membership.
  - `GET /apps/{id}/projected-charge` — current-period projected charge: re-run `charge_cents` over the *live* `usage_aggregates` for the open period. round 1, MAJOR-5: this re-prices on every call (an unbudgeted compute amplifier if a creator polls it), so it is **cached with a short TTL (60s) in `env.kv`** keyed `(app_id, period)` and the response is **explicitly labelled non-authoritative** (`{"projected_charge_cents": …, "authoritative": false, "as_of": <ts>}`). It is a projection, never a bill; the only authoritative charge is a finalized invoice. The cache is invalidated naturally by TTL (usage only grows within a period). Per-creator rate-limit as a backstop.
  - `GET /billing/credit-balance` — `SUM(credit_ledger.amount_cents)` consumable balance for the caller's creator id.
  - `GET /billing/payment-method` — `creator_billing.default_pm_set` + `billing_customer_refs` presence (status only, never the PAN).
  - `GET /apps/{id}/billing-status` — plan id + `app_spend_state.state` + `creator_billing_status.state`.
  All reads are creator-scoped: a creator sees only owned apps; an operator sees any via `Resource::Any`. No money-write capability is exposed creator-side.

**H. Finalize → collect handoff** (`billing_reconcile.rs`, after finalize). round 1, MAJOR-6 — the new credit/tax charges need a defined collection path. round 2, CRITICAL-A: collection is recorded by **appending `invoice_payments` rows**, never by writing a column on the finalized invoice:
  - **`total_cents = 0` after full credit (or a $0 bill):** there is NOTHING to charge. The reconciler does NOT issue a payment-method charge and writes **no `invoice_payments` row** (so `Σ(invoice_payments) = 0`, the correct cash-collected). It marks the invoice **paid-by-credit** immediately (no `invoice.paid` webhook to wait for — there is no Stripe charge). It does NOT enter dunning, does NOT touch `creator_billing_status`, and emits an `invoice_finalized` notification noting "$0 — covered by credit". This is the common promo-credit case; treating it as an unpaid bill would wrongly suspend the creator. (A $0 invoice can never be over-refunded: the cap is `Σ(invoice_payments) = 0`.)
  - **`total_cents > 0`:** the existing Stripe rail charges the default payment method (the same path that exists today for the base/usage bill — credit and tax just change the amount). On `invoice.paid` / `charge.succeeded` the handler **appends an `invoice_payments` row** (`kind='charge'`, `amount_cents` = the amount actually collected — full or partial, positive; `provider_ref` = the `pi_`/`ch_`) in the SAME txn that records payment. **This is a plain INSERT into a side table — it never touches the finalized invoice, so `invoices_immutable()` is never invoked.** On `invoice.payment_failed` the existing dunning lifecycle (`account_status.rs` → `past_due` → `dunning.rs` → suspend) runs unchanged.
  - **Credit-covered-but-nonzero (partial credit):** `total_cents > 0` with `credit_cents > 0` is just a normal `> 0` charge — credit reduced the amount, the remainder is collected by card. If that card charge then fails, dunning runs on the **reduced** `total_cents`; the consumed credit is NOT refunded on payment failure (the credit was applied to a real, owed bill — it is only restored on a *void*, flow C). A creator whose card fails on a credit-reduced bill enters `past_due` on the reduced amount, exactly like any other unpaid bill.

**I. Dispute / chargeback** (`stripe_handlers.rs`, new webhook branch). round 1, MISSING-CONCEPT 1; round 2, CRITICAL-A — the clawback is recorded as a negative `invoice_payments` row. On `charge.dispute.created` (claim-after-success on `stripe_events_seen` like every webhook): resolve the invoice from the disputed charge → in ONE txn, `billing_disputes` INSERT (`status='open'`, the network-held `amount_cents`, `dp_…`) **and** an `invoice_payments` INSERT (`kind='dispute_debit'`, `amount_cents = −disputed`, `provider_ref = dp_…`). The negative payment row **automatically lowers `Σ(invoice_payments)`**, so the over-refund cap (which reads that sum) tightens with no cross-table trigger — the dispute-aware endpoint check is then a belt-and-braces double-check. Fire a `disputed` notification once. On `charge.dispute.closed` with `won` → `billing_disputes.status='won'`, `resolved_at`, **and** an `invoice_payments` INSERT (`kind='dispute_reversal'`, `amount_cents = +disputed`) restoring the budget; with `lost` → `status='lost'` (chargeback final; the negative row stays). A dispute is NEVER auto-refunded (the funds already moved) and NEVER mutates the invoice — it is its own append-only fact (plus its payment-side row), mirroring the refund discipline.

---

## Worked end-to-end example — balance conservation across credit → refund_to_credit → consume → void <!-- Added in round 1: addressing MISSING-CONCEPT 2 -->

A single sequence that crosses every money seam and proves both `Σ(credit_ledger)` is conserved AND the over-refund cap holds against `Σ(invoice_payments)`. <!-- Re-traced in round 2: cash-collected is now Σ(invoice_payments), and the void is REORDERED to come AFTER a cash refund so the true-up correctly nets out the already-issued refund (CRITICAL-B). --> Creator `usr_X`, USD, days-in-month = 30. `cash` column = `Σ(invoice_payments)` for the named invoice (the over-refund anchor).

| # | Action | Ledger entries | `invoice_payments` rows | Σ balance | Invoice money | `cash` = Σ(invoice_payments) |
|---|--------|----------------|--------------------------|-----------|---------------|------------------------------|
| 0 | Operator grants $50 promo (idem-key K0, fingerprint F0) | `grant +5000` (g1) | — | **$50.00** | — | — |
| 1 | Month-end: invoice A, subtotal $30, tax $0. Credit drawn $30 from g1 | `consumed −3000` (g1) | — (total=0, no charge) | **$20.00** | A: sub 3000, credit 3000, total **0** | A: **0** (paid-by-credit) |
| 2 | Month-end: invoice B, subtotal $80, tax $0. Credit drawn $20 (g1 remaining) | `consumed −2000` (g1) | B: `charge +6000` | **$0.00** | B: sub 8000, credit 2000, total **6000** | B: **6000** |
| 3 | Cardholder over-charged: operator refunds $25 of B **to cash** (re_…), tax split 0 | — (cash refund) | — (refund is a `refunds` row, not a payment) | **$0.00** | B unchanged (finalized) | B: 6000; cap: Σcash $25 ≤ $60 ✓ |
| 4 | Operator refunds a further $20 of B **to credit** (goodwill) | `refund_to_credit +2000` | — | **$20.00** | B unchanged | Σcash $25 + Σcredit $20 = $45 ≤ $60 ✓ |
| 5 | B mis-priced (wrong app weight). Operator **voids** B | `void_reversal +2000` (restores the $20 B consumed at step 2) | — (B's `charge +6000` row stays — permanent cash record of the voided invoice) | **$40.00** | B → void | B: still **6000** (payments untouched by void) |
| 6 | **Reissue** B′, corrected subtotal $50. Credit drawn $40 | `consumed −4000` | B′: `charge +1000` | **$0.00** | B′: sub 5000, credit 4000, total **1000** | B′: **1000** |
| 7 | **True-up (CRITICAL-B):** B collected $60 cash, but $25 was ALREADY refunded (step 3) and B′ only owes $10. Over-collection on the *voided* B = `Σcash-paid(B) − Σcash-refunds-already-issued(B) − total(B′)` = `$60 − $25 − $10 = $25`, floored at 0 | — | B: `dispute`-style note? No — a true-up `refunds` row `cash −2500`? See below: it is a **refund** (cash $25 to card), recorded as a `refunds` row on B | **$0.00** | B unchanged | cap on B: Σcash now $25 + $25 = $50 ≤ $60 ✓ |

**Conservation check (credit).** Granted = $50 (g1) + $20 (refund_to_credit, step 4) = $70. Consumed = $30 (A) + $20 (B) + $40 (B′) = $90, restored = $20 (void_reversal). Net consumed = $70. Balance = $70 − $70 = **$0** — matches. The step-5 `void_reversal` prevents the double-consume: without it, step 6 would draw $40 from a balance of only $20, going negative.

**True-up correctness (CRITICAL-B).** The round-1 bridge would have computed the true-up as `cash_collected(B) − total(B′)` = `$60 − $10 = $50` and tried to refund $50 to cash — but $25 was **already refunded at step 3**. That $50 refund *plus* the existing $25 = $75 cash refunds on B, which **exceeds** B's `Σ(invoice_payments)` = $60, so the over-refund trigger would silently REJECT the true-up, leaving $25 of over-collection stuck with the creator out-of-pocket. The round-2 bridge subtracts already-issued cash refunds: true-up = `$60 − $25 − $10 = $25`, and the cap check `Σcash = $25 + $25 = $50 ≤ $60` passes. The creator's net cash position: paid $60, refunded $25 (step 3) + $25 (true-up) = $50 back, net cash $10 = exactly `total(B′)`. Correct.

**Cash-cap.** The over-refund cap held throughout against B's `Σ(invoice_payments)` = $60 (the `charge +6000` row), never against the credit-inflated `total_cents` (here $60 happens to equal it because credit was applied; on an invoice where credit exceeds cash the distinction is load-bearing — that is the credit-laundering defence). Note the void at step 5 left B's payment row in place, so the true-up at step 7 correctly bounds against the *real* cash B collected.

---

## Full usage-segment proration <!-- Added in round 3: decision 1 OVERRIDE — the real design work -->

The user overrode the recommended base-fee-only proration: a mid-period plan change must be money-correct on **usage**, not just the monthly base fee. This section is the complete, money-correct design.

### The hard problem (stated explicitly)

`usage_aggregates` is keyed `(app_id, period, metric)` with a **single running cumulative `total`** for the whole calendar month (changeset `0039`). There is **no per-event timestamp and no sub-period bucket** — only one monotonically-growing counter per metric per month. Consequently you **cannot** split a period's usage across a pre-change plan segment and a post-change plan segment by time-filtering the underlying events: the events were never retained; the counter is all that survives. A naive "re-price the whole period's `usage_aggregates` total under the new plan" retroactively reprices usage that happened under the old plan — exactly the retroactive-rewrite bug the redesign's snapshot-onto-line discipline exists to kill.

### The solution (cumulative checkpoint → segment delta)

Because the counters are **monotonic cumulative totals**, a single checkpoint at the change instant is enough to reconstruct each segment's usage:

1. **At every `set_plan` (the change instant):** snapshot the app's cumulative `usage_aggregates` totals per metric onto the `plan_change_events` row — `usage_at_change` JSONB `{metric: cumulative_total}` — read **server-side in the same txn as the `apps.plan_id` flip**, so it reflects exactly the running total at the moment the plan switched, frozen and un-racy.
2. **At month-end reconcile:** an app with **N** plan-change events in the period splits into **N+1 segments** (ordered by `effective_at`). For each segment and each metric,

   > **segment usage = (cumulative at segment END) − (cumulative at segment START)**

   where START = the prior change's `usage_at_change` (or 0 for the first segment) and END = the next change's `usage_at_change` (or the **period-end `usage_aggregates` totals** for the last segment). Each delta is `≥ 0` because the counters only grow.
3. **Each segment prices its own delta** under its own plan's weights, FX, and (pro-rated) included_units, frozen as a **separate invoice line**. Summing the segments' deltas telescopes back to the full-period total exactly: `(c₁−0) + (c₂−c₁) + … + (cₑₙ𝒹 − cₙ) = cₑₙ𝒹`. No usage is double-counted or dropped.

This is the redesign's snapshot-onto-line discipline applied to a **cumulative checkpoint** instead of a final total. It needs no per-event metering store and no schema change to `usage_aggregates`.

### Line-grain reshape (`0054`)

`invoice_lines`'s PK was `(invoice_id, app_id)` — one line per app per invoice. Full usage-segment proration needs **multiple lines per app per invoice** (one per segment). Changeset `0054` (above) adds **`segment_no SMALLINT`** to the PK → `(invoice_id, app_id, segment_no)`, adds a per-segment `plan_id`, and **retargets the `billing_line_provider_refs` composite FK** to the new 3-column line key (so each segment can carry its own Stripe `invoice_item` ref without the FK ever pointing at a non-existent segment line).

**New changeset, not a reshape-in-place of `0042`.** The redesign (`2026-06-14-billing-schema-redesign.md`, separately hardened to 93/100) stays authoritative and un-edited for its own reviewers; this proposal has consistently expressed every redesign-table change as a new, attributed changeset (`0042`'s partial-index swap is the one surgical exception, and it is a constraint swap, not a PK widening). A PK widening that two FK-bound tables depend on is cleaner as a self-contained, single-unit-reversible `0054` that **lands after `0042`/`0053`** in the changelog (it ALTERs tables those created) — honouring this doc's own changelog-ORDER discipline (the `0053`-before-`0049` pin). Pre-launch on a clean re-migrate, the ALTER hits a freshly-created, empty `invoice_lines`, so there is no backfill.

### base_fee proration

Each segment's base fee is **day-weighted**, not usage-weighted (a monthly base fee is a flat subscription charge, owed in proportion to the days the plan was active):

> **segment_base_fee = round_half_up( plan.base_fee_cents × segment_days / days_in_period )**

- **Day-count basis (pinned, half-open — round 4, CRITICAL-2):** `days_in_period` = the calendar days in the month of `period` (28/29/30/31 from `EXTRACT(DAY FROM (period + INTERVAL '1 month' - INTERVAL '1 day'))`). Each segment's day-span is a **half-open calendar-day interval**:

  > **segment_days[k] = date_trunc('day', next_effective_at)::date − date_trunc('day', this_effective_at)::date**

  where `this_effective_at` is segment k's plan's `effective_at` (= `period_start` for segment 0), `next_effective_at` is segment k+1's `effective_at`, and **for the last segment `next_effective_at := period_end`** (the first-of-next-month boundary). The change day belongs to the **opening** segment `[start, next)`, never shared — so days partition the month **exactly** and telescope: `Σ segment_days[k] = (d₁−d₀)+(d₂−d₁)+…+(end−dₙ) = end−d₀ = days_in_period`. There is **no shared boundary day** and no double-count.
- **Base-fee invariant:** because the day-spans partition exactly, **`Σ pro-rated base ≤ one full base fee`** of whichever plans were active (a mid-month downgrade never charges two full base fees). On the SAME plan throughout (no change), the single segment's days = `days_in_period`, so the base fee is exactly the full fee.
- **Rounding + remainder-cents rule (deterministic):** half-up to whole cents, matching `charge_cents`'s `÷ FX_SCALE` half-up rounding. Rounding N segment base fees independently can leave a sub-cent residue vs. one full fee; the leftover cent(s) are assigned **deterministically to the LAST segment** so `Σ(prorated base)` lands exactly at `round_half_up(full_fee × days_active / days_in_period)` and never exceeds one full fee.

### included_units (free quota) proration across segments — POLICY DECISION

Free quota (`plans.included_units`, the CU covered before overage) must be split across segments. Two policies:

- **(a) Per-segment pro-rated quota [RECOMMENDED].** Each segment gets `plan.included_units × (segment_days / days_in_period)` of free CU, applied to **that segment's usage-delta** under **that segment's plan**. A segment overages only when its own delta exceeds its own pro-rated quota.
- **(b) Global period quota consumed in segment order.** One quota pool for the period (whose size is itself ambiguous when the plans differ), drawn down segment-by-segment in `effective_at` order; later segments see whatever is left.

**Recommend (a).** It matches the per-segment-plan model the whole design is built on: each segment is priced *entirely* under its own plan, and a plan's free quota is a property *of that plan*, so a segment should get the slice of *its* plan's quota proportional to the days it ran. It is also the only policy that composes cleanly with snapshot-onto-line — the frozen `included_units` on each line is `plan.included_units × day-fraction`, a pure function of the segment, so the line replays bit-for-bit with no cross-segment state. Policy (b) needs a defined "period quota" when the two plans' quotas differ (whose quota? a blend?) and makes each line's replay depend on the prior segments' consumption — breaking the per-line reproducibility the redesign guarantees.

**Edge cases (a) must own, stated honestly:**
- **A usage spike concentrated in a short cheap segment vs. a long expensive one.** With (a), quota is split by *days*, not by *where the usage landed*. If a creator runs on a cheap plan (large quota) for 27 days with almost no usage, then upgrades to an expensive plan (small quota) for the last 3 days and a usage spike lands in those 3 days, the spike is charged against only `3/30` of the expensive plan's already-small quota — i.e. the creator pays more than if the same usage had been spread evenly. This is **correct under a per-segment-plan model**: the usage happened while the expensive plan (with its small quota) was active, so it is priced under that plan's economics. The inverse (spike during the long cheap segment) is charged against `27/30` of the cheap plan's large quota — cheap, again correctly, because that is the plan that was active. The day-proration of quota does **not** try to chase the usage; it tracks the *plan*, which is the whole point of segmenting. Documented so a reviewer doesn't read it as a bug.
- **A zero-day segment** (two `set_plan`s on the same calendar day): `segment_days = 0`. round 4, MINOR-6 — **v1 MERGES a `segment_days == 0` segment into its neighbour** rather than emitting it as a standalone line. Reason: a standalone zero-day segment gets zero pro-rated quota (`included_units × 0/days = 0`) yet still carries a usage delta, so its entire delta bills as overage with NO free quota — an over-bill SPIKE for usage that merely *straddled* a same-day flip. The merge rule (applied in `build_segments` before pricing): when a segment's half-open day-span is 0, fold its `usage_delta` into the **next** segment (the one that actually has days; if it is the LAST segment, fold into the **previous**), summing the deltas, and DROP the zero-day boundary. The surviving neighbour prices the combined delta against ITS pro-rated quota under ITS plan — no quota-starved spike. Day-spans still partition exactly (the dropped boundary contributed 0 days). This keeps `Σ segment_days = days_in_period` and never creates a quota-less line.
- **A metric present in one segment's snapshot but absent in another** is treated as cumulative 0 at the absent snapshot, so its delta is well-defined. round 4, MAJOR-3 — when the absent snapshot is the segment **END** (a metric in `usage_at_change` whose `usage_aggregates` row is gone at period-end), end = 0 makes `end − start` negative; the `segment_delta[m] = max(0, end − start)` floor pins it to 0 so a vanished metric never credits the bill. **App-delete mid-open-period is out of scope:** `apps.id` FK is `ON DELETE CASCADE`, so deleting an app erases its `plan_change_events` + `usage_aggregates` rows together; a half-deleted app mid-period is not a billable state this reconciler reaches.

### Composition with the existing reconcile (degenerates cleanly)

The finalize machinery is **unchanged**. `bill_creator` still: builds draft lines, finalizes the invoice in **one UPDATE** writing `subtotal/credit/tax/total/status`, with the **balance CHECK** and the **immutability triggers** as committed. The only change is that the per-app line-build step now emits **1..N+1** lines instead of always 1:
- **No plan change (0 rows) ⇒ exactly ONE line** per app: `segment_no=0`, `plan_id` = the app's current `apps.plan_id`, `usage_snapshot` = the full-period `usage_aggregates` totals (delta from 0 to period-end), full `base_fee_cents`, full `included_units`. This is **byte-for-byte identical to today** — the reshape degenerates with no behavioural change for the common case (one Stripe item, one line, one provider-ref, all at `segment_no=0`).
- **The per-metric floor-before-sum** (`total_units = Σ_m floor(usage[m] × units_per_op / per_units)`) still applies **per segment** — each segment floors its own delta per metric before summing, exactly as a single line floors the full total today.
- **`subtotal_cents`** on the invoice is `Σ` over **all** segment lines of **all** apps (the finalize UPDATE sums `amount_cents` across the now-multiple lines per app). Credit, tax, and the balance CHECK operate on that subtotal exactly as before — they are invoice-level, segment-agnostic.
- Lines are built while the invoice is `draft` (mutable), then **frozen at finalize** by the existing `invoice_lines_immutable` trigger (which fires per row, so it freezes every segment line). The per-creator reconcile advisory lock still single-flights the whole build+finalize.

### The reconcile-loop rewrite — `BilledSegment` (the missing 60% of PR-4) <!-- Added in round 4, CRITICAL-1: the 0054 PK widening to (invoice_id, app_id, segment_no) breaks EVERY per-app guard in billing_reconcile.rs — those guards MUST be rekeyed (invoice_id, app_id, segment_no) or N segments collide to ONE Stripe item and only segment 0 posts (silent under-bill). -->

`0054` widens the line PK to `(invoice_id, app_id, segment_no)`. **Every per-app construct in `billing_reconcile.rs`'s per-app loop is therefore now segment-blind and MUST be rekeyed**, or N segments collapse onto one Stripe invoice-item and the bill is silently short by N−1 segments. This is the concrete rewrite PR-4 lands; the four collision points and their fixes:

**(1) Stripe invoice-item idempotency key MUST include the segment.** Today `invoice_item_idempotency_key(creator, app, period)` (`billing_reconcile.rs:120-124`) is per-`(creator, app, period)`. Post-`0054`, an app posts N+1 items in one period — all sharing that one key. Stripe's `Idempotency-Key` dedups them to a SINGLE item, so **only the first segment posts and the rest are silently dropped (under-bill).** The key gains the segment discriminator:

```rust
// round 4, CRITICAL-1: segment-aware item key — one Stripe item PER SEGMENT.
// Was: format!("billitem:{creator}:{app}:{period}")  (N segments → 1 item → under-bill).
pub fn invoice_item_idempotency_key(
    creator_id: &Uuid, app_id: &Uuid, period_start_unix: i64, segment_no: i16,
) -> String {
    format!("billitem:{creator_id}:{app_id}:{period_start_unix}:{segment_no}")
}
```
The N=0 degenerate path passes `segment_no = 0`, so its key is `billitem:{creator}:{app}:{period}:0` — a stable superset of today's key shape (the `:0` suffix is the only change), and still one item per app on the no-change path.

**(2) The posted/line-exists guards rekey to `(app_id, segment_no)`.** Today (`L504-523`) the reconcile loads `posted: HashSet<Uuid>` (apps with a `billing_line_provider_refs` row) and `line_exists: HashSet<Uuid>` (apps with an `invoice_lines` row), both keyed by `app_id` alone. Post-`0054` both become `HashSet<(Uuid, i16)>` keyed `(app_id, segment_no)` — the SELECTs add `segment_no` to the projection:

```rust
// posted: a (app, segment) is confirmed-posted iff its provider-ref row exists.
let posted: HashSet<(Uuid, i16)> = conn.query(
    "SELECT app_id, segment_no FROM zeroship.billing_line_provider_refs \
     WHERE invoice_id = $1 AND provider = 'stripe' AND ref_kind = 'invoice_item'",
    &[&invoice_id]).await?
  .iter().map(|r| (r.get("app_id"), r.get("segment_no"))).collect();
// line_exists: a (app, segment) line (intent/snapshot) already written?
let line_exists: HashSet<(Uuid, i16)> = conn.query(
    "SELECT app_id, segment_no FROM zeroship.invoice_lines WHERE invoice_id = $1",
    &[&invoice_id]).await?
  .iter().map(|r| (r.get("app_id"), r.get("segment_no"))).collect();
```

**(3) The line UPSERT `ON CONFLICT` widens to the 3-col PK.** Today (`L552-562`) the degenerate INSERT is `ON CONFLICT (invoice_id, app_id) DO UPDATE …`. Post-`0054` it is `ON CONFLICT (invoice_id, app_id, segment_no) DO UPDATE …` AND the column list adds `segment_no, plan_id` (both NOT NULL). The N=0 path writes `(segment_no=0, plan_id=current apps.plan_id)` and is otherwise byte-identical to today's single-line write.

**(4) `billing_line_provider_refs` writes/reads carry `segment_no`.** The confirm-post INSERT (`L614-621`) adds `segment_no` to the column list and to its `ON CONFLICT (invoice_id, app_id, segment_no, provider, ref_kind)`. The composite FK (now `billing_line_provider_refs_line_fk` → the 3-col line PK, MINOR-7) and the line-provider-ref presence guard (the refund/double-bill guard: "a ref EXISTS for this (invoice, app, segment) ⇒ skip the re-POST") therefore operate per-`(invoice_id, app_id, segment_no)`. A malformed `(invoice, app, segment)` key is rejected at write by the FK, so the guard can never silently fail-to-match.

**The rewritten loop SKELETON** (replaces the `for line in &lines` body):

```rust
struct BilledSegment {
    app_id: Uuid,
    segment_no: i16,            // 0..N in effective_at order
    plan_id: String,            // the segment's plan (NOT NULL on the line)
    usage_delta: HashMap<String, i64>,   // max(0, end−start) per metric (MAJOR-3 floor)
    included_units: u64,        // plan.included_units × segment_days/days_in_period
    base_fee_cents: u64,        // plan.base_fee_cents × segment_days/days_in_period (+remainder on last)
    fx_pico_cents_per_unit: u64,// the segment plan's resolved FX (per-plan override lever)
    amount_cents: i64,          // charge_cents(price{base,included,fx}, usage_delta, GLOBAL weights)
    desc: String,               // per-segment Stripe description (see below)
}

// Per app: compute its segments ONCE (degenerates to a single segment_no=0 when N=0).
for app in &billable_apps {
    let segments: Vec<BilledSegment> = build_segments(app, &period_window, &plan_change_events);
    for seg in &segments {
        let key = (seg.app_id, seg.segment_no);
        if posted.contains(&key) { continue; }          // confirmed-posted: skip re-POST
        let intent_only = line_exists.contains(&key);
        // snapshot-onto-(segment-)line BEFORE the POST — note charge_cents adds base
        // INTERNALLY, so the PRO-RATED base is passed via price.base_fee_cents and the
        // PER-PLAN FX override via price.fx_pico_cents_per_unit; weights stay GLOBAL.
        upsert_segment_line(&conn, &invoice_id, seg).await?;   // ON CONFLICT (inv, app, segment_no)
        let item_key = invoice_item_idempotency_key(
            creator_id, &seg.app_id, period_start, seg.segment_no);
        let item_id = post_or_adopt_stripe_item(            // one item PER SEGMENT
            &stripe, &customer, seg.amount_cents, &seg.desc, period_window,
            &item_key, intent_only).await?;
        insert_segment_provider_ref(&conn, &invoice_id, seg, &item_id).await?; // ON CONFLICT (inv,app,segment_no,provider,ref_kind)
    }
}
```
Everything OUTSIDE this loop — the invoice claim (`creator_id, period`), the draft-invoice persist, the finalize converge, the `i64::try_from` overflow guards — is **unchanged**: segmentation is entirely WITHIN the per-app line build. `build_segments` returns exactly one `segment_no=0` segment when the app has no `plan_change_events` row for the period, so the N=0 path is byte-identical-behaviour to today (one line, one item, one ref).

**Regression-test intent (PR-4):** a 2-segment app (one mid-period plan change) posts **exactly 2 DISTINCT Stripe items** (2 distinct `Idempotency-Key`s) and **2 `invoice_lines` rows** (`segment_no` 0 and 1) with 2 `billing_line_provider_refs` rows — and the test FAILS against the segment-blind key (which would post 1 item and 1 line, under-billing segment 1). A re-drive of the same period adopts both items idempotently and posts NEITHER twice.

**Per-segment Stripe line-item DESCRIPTION (MISSING-3).** Each segment posts its own Stripe `invoice_item` with a human-readable, segment-scoped description so the creator's Stripe-hosted invoice reads correctly per segment, e.g. `"Infra usage — app {name} (Pro, days 11–30)"` / `"Infra usage — app {name} (Free, days 1–10)"`. The `desc` is built from `{app name, segment plan_id, the segment's half-open day-span}`; the no-change path keeps today's single full-period description (no day-span suffix). **Item-count growth:** an app with N plan changes now posts **N+1** items instead of 1, so the per-invoice Stripe item count grows from `#apps` to `Σ_app (segments_app)` ≈ `#apps × (1 + avg_changes)`. The per-period change cap (8) bounds it to ≤ `9 × #apps`; Stripe imposes no hard per-invoice item ceiling at this scale, and the reconcile loop posts them sequentially under the same advisory lock, so the only cost is more API calls per sweep (bounded, logged via the existing per-item tracing).

### Idempotency / reproducibility / crash-re-run

The cumulative snapshot makes every segment **deterministic and re-runnable**:
- A segment's usage-delta is a pure function of two frozen snapshots (`usage_at_change` rows) and, for the last segment, the period-end `usage_aggregates` totals. Re-running `bill_creator` for the same `(creator, period)` recomputes **identical** deltas — the snapshots are immutable (append-only `plan_change_events`), and within a *closed* period the period-end totals no longer change.
- **Crash mid-build:** the invoice is still `draft` (finalize is the single committing UPDATE), so a re-run rebuilds the draft lines from scratch from the same frozen inputs — same segments, same amounts. No partial-segment state survives a crash because nothing is committed until the one finalize UPDATE.
- **Reconcile re-run after finalize:** the period-claim short-circuit (`status='finalized' ⇒ skip`) prevents a re-bill; a deliberate **void+reissue** (flow C) re-runs the segment split over the *same* frozen `plan_change_events` snapshots + the (now-closed) period-end totals, reproducing identical segment deltas. This is why the design is replay-safe: the segment boundaries live in immutable rows, not in a re-derived time filter.
- **Open-period projected charge** (read API G) re-runs the same segment split over the *live* (still-growing) period-end totals — so it is explicitly non-authoritative and TTL-cached, exactly as today; only a finalized period's totals are stable.

### Overflow surface is unchanged by segmentation (MISSING-2)

Segmentation does **not** widen the i64 money surface. The per-creator `total_cents` is **unchanged** by splitting an app into segments: the segments' usage deltas telescope back to the same full-period total (`Σ deltas = period-end total`), and the segments' pro-rated base fees sum to **≤ one full base fee** (the base-fee invariant) — so `Σ_segments amount_cents ≤` the single-line full-period amount the app would have billed un-segmented (it can be strictly *less* when usage straddled a cheaper plan with a larger quota, never more). Therefore the existing `i64::try_from(total_cents)` / per-item `i64::try_from(line.amount)` overflow guards (`billing_reconcile.rs:455-462, 538-543`) bound the same maximum as today; no new i64 surface is introduced. Per-segment, each `amount_cents` is a fraction of (or equal to) what one line carried, so a segment can never overflow where the full line did not. The hard-error-not-clamp discipline (refuse to bill, skip the creator) carries through per segment unchanged.

### Concurrent `set_plan` racing month-end reconcile (MISSING-4)

A `set_plan` can land while the month-end reconcile is finalizing — or after a period has already finalized. The rule that keeps this safe:
- **`set_plan` respects the period-finalized short-circuit and the per-creator advisory lock.** The reconcile holds the per-creator advisory lock for the whole build+finalize; `set_plan`'s `plan_change_events` INSERT + `apps.plan_id` flip take the **same per-creator advisory lock** (the app's creator), so a plan change cannot interleave *inside* a finalize — it serializes either fully before or fully after.
- **A change whose `effective_at` falls in an ALREADY-FINALIZED period is attributed to the NEXT period.** Once an invoice for `(creator, period)` is `finalized`, that period's bill is frozen (immutability triggers). A `set_plan` arriving after that must NOT mutate the finalized bill (it cannot — the triggers reject it, and proration never touches a finalized invoice anyway). So the `plan_change_events` row it writes carries `period = first-of-month(now)`; if the current wall-clock month's invoice is already finalized (a late reconcile-then-change race), the change's `effective_at` is in the *closing* period but its proration effect lands in the **next** period's segment split (the next period opens with `apps.plan_id` already flipped, so its segment 0 is the new plan). The plan flip itself (`apps.plan_id`) always applies immediately — only the *billing attribution* respects the period-finalized boundary.
- **Interaction with the void+reissue path.** A void+reissue re-runs the segment split over the *same immutable* `plan_change_events` snapshots; a `set_plan` that raced in after the original finalize but before the void carries the next period's attribution and is NOT swept into the reissued (prior-period) invoice. Both paths take the per-creator advisory lock, so they never interleave.

### Spend enforcement is unaffected

`usage_aggregates` and the spend cap stay **period-grained and local**. Proration is a **finalize-time invoice concern only** — it reads `usage_at_change` snapshots and the period-end totals to *split a bill*, and never touches the running counter the gateway's Warn/Degrade/Block path enforces against. The spend state is evaluated against the whole-period cumulative spend as before; a mid-period plan change does not re-segment the spend cap (the cap is a safety limit on total spend, not a per-plan bill). The two concerns share the `usage_aggregates` table read-only and never contend.

### Worked sub-trace — one mid-period plan change, two segments

App `app_Y`, a **30-day month** (`period` = 1st; `days_in_period = 30`), single metric `requests` (global weight `units_per_op=1, per_units=1` ⇒ 1 CU per request). Plan **Free**: `base_fee=0`, `included_units=1000`, `fx=1 cent/CU`. Plan **Pro**: `base_fee=3000` ($30), `included_units=10000`, `fx=1 cent/CU` (per-plan FX overrides are equal here for clarity; the segment machinery prices each under its OWN `fx_pico_cents_per_unit` regardless). The creator runs Free from the 1st, then `set_plan Free→Pro` with `effective_at` = **the 11th, 00:00**.

| checkpoint | `effective_at` | cumulative `requests` | event |
|---|---|---|---|
| period start | the 1st | 0 | initial assignment, `usage_at_change={}` (treated 0) |
| upgrade | the 11th | 4,000 | `set_plan Free→Pro` writes `usage_at_change={"requests":4000}` |
| period end | the 1st (next month) | 30,000 | `usage_aggregates.total = 30000` |

**Day-split DERIVED from the half-open rule** (not asserted): N=1 change ⇒ 2 segments.
- segment 0 days = `trunc(11th) − trunc(1st) = 11 − 1 = 10` days.
- segment 1 days = `period_end − trunc(11th) = 30 − 10 = 20` days (last segment ends at `period_end`).
- check: `10 + 20 = 30 = days_in_period` exactly — the 11th belongs only to segment 1 (no shared boundary day).

- **Segment 0 (Free, `[1st, 11th)`, 10 days):**
  - usage delta = `max(0, 4000 − 0) = 4000` CU (MAJOR-3 floor; trivially satisfied).
  - pro-rated quota = `round_half_up(1000 × 10/30) = round_half_up(333.33) = 333` CU. base fee = `round_half_up(0 × 10/30) = 0`.
  - `charge_cents` passes `price{base_fee_cents:0, included_units:333, fx:1cent/CU}` over delta 4000 against the GLOBAL weights: billable = `max(0, 4000 − 333) = 3667` CU ⇒ overage `round_half_up(3667 × 1) = 3667` cents `+ base 0` = **3667 cents ($36.67)**.
  - frozen line: `{segment_no:0, plan_id:Free, usage_snapshot:{requests:4000}, included_units:333, base_fee_cents:0, fx:1cent/CU, amount_cents:3667}`.
- **Segment 1 (Pro, `[11th, period_end)`, 20 days):**
  - usage delta = `max(0, 30000 − 4000) = 26000` CU.
  - pro-rated quota = `round_half_up(10000 × 20/30) = round_half_up(6666.67) = 6667` CU. base fee = `round_half_up(3000 × 20/30) = 2000`.
  - `charge_cents` passes `price{base_fee_cents:2000, included_units:6667, fx:1cent/CU}` over delta 26000: billable = `max(0, 26000 − 6667) = 19333` CU ⇒ overage `19333` cents `+ base 2000` (added INTERNALLY by `charge_cents`) = **21333 cents ($213.33)**.
  - frozen line: `{segment_no:1, plan_id:Pro, usage_snapshot:{requests:26000}, included_units:6667, base_fee_cents:2000, fx:1cent/CU, amount_cents:21333}`.

**Sum check (usage telescopes):** segment deltas `4000 + 26000 = 30000` = the period-end cumulative total — no usage lost or double-counted. **Invoice subtotal** = `3667 + 21333 = 25000` cents ($250.00) across the two segment lines for `app_Y`. **Base-fee invariant:** `0 + 2000 = 2000` ≤ one full Pro base fee ($30) — and equals `round_half_up(3000 × 20/30)`, the days Pro was active; no double base fee, remainder-cents rule is a no-op here (both base fees divided evenly). These hard-coded numbers (`333, 0, 3667`; `6667, 2000, 21333`; subtotal `25000`) are what PR-4's two-segment regression test asserts — each is now DERIVED from the pinned day-partition rule, not asserted, so the test codifies provably-correct numbers.

**Replay:** re-running `charge_cents` over each frozen segment line — Free over `{requests:4000}` with quota 333, Pro over `{requests:26000}` with quota 6667 — reproduces `3667` and `21333` bit-for-bit. A void+reissue re-derives the SAME two segments from the immutable `usage_at_change={requests:4000}` snapshot + the closed period-end total 30000, so the corrected invoice's segments match (modulo any deliberately-corrected weights). Crash mid-build leaves the invoice `draft` and a re-run rebuilds the identical two lines.

---

## Negative-invoice / true-up bridge <!-- Added in round 1: addressing MISSING-CONCEPT 3 -->

A void+reissue can produce a corrected invoice that is **lower than what was already collected** on the original. Example: invoice A collected $100 cash; correction shows the true bill was $70. The $30 over-collection must come back to the creator. The bridge (round 2: reads `Σ(invoice_payments)`, and — CRITICAL-B — subtracts refunds already issued on the voided invoice):
  - On reissue, the void+reissue path computes the over-collection on the *voided* invoice as:

    `over_collection = cash_paid(old) − cash_refunds_already_issued(old) − total(new)`, **floored at 0**

    where `cash_paid(old) = Σ(invoice_payments.amount_cents WHERE invoice_id = old)` (the net cash actually collected, already reduced by any `dispute_debit` rows) and `cash_refunds_already_issued(old) = Σ(refunds.amount_cents WHERE invoice_id = old AND destination='cash')`. **The `cash_refunds_already_issued` subtraction is the CRITICAL-B fix**: round 1 used `cash_paid(old) − total(new)` and IGNORED earlier refunds, so on an invoice that had already been partially cash-refunded the bridge over-refunded — and its own over-refund trigger (cap = `Σ(invoice_payments)`) then silently REJECTED the bridge refund, leaving the over-collection stuck.
  - If `over_collection > 0`, the operator path **auto-creates a refund** on the *voided* invoice for that amount (`destination='cash'` by default, `idempotency_key` derived from `(old_invoice_id, 'true_up')`, fingerprint over the canonical body). The over-refund trigger is satisfied by construction: `cash_refunds_already_issued + over_collection = cash_paid(old) − total(new) ≤ cash_paid(old) = Σ(invoice_payments)`, so the new total cash refunds never exceed the cash anchor.
  - The reissued invoice then collects its own `total(new)` normally (flow H, appending a fresh `invoice_payments` row on B′). Net: the creator's cash position is exactly `total(new)`.
  - This is recorded as a normal `refunds` row (the true-up is a refund, not a special object), so it flows through the same observability + notification surfaces. There is no "negative invoice" object in v1 — a negative balance is expressed as a refund against the over-collected (now-voided) invoice, which is the Stripe-aligned model (Stripe also expresses over-collection as a refund/credit-note, not a negative invoice).

---

## Notification template inventory <!-- Added in round 1: addressing MISSING-CONCEPT 4 -->

Every `billing_notification_kind` maps to exactly one template. All billing notifications are **transactional** (account/money state the creator must know), NOT marketing — so they are NOT subject to a marketing unsubscribe, but they DO honour the existing `email_suppressions` hard-bounce/complaint suppression contract (a creator who hard-bounced gets no mail; that is deliverability, not preference). v1 is **email-only, locale `en` only**; the template key carries a locale segment (`billing/past_due/en`) so adding locales later is a template-pack drop, not a code change.

| kind | trigger | content (subject gist) | transactional |
|---|---|---|---|
| `payment_failed` | first failed charge | "We couldn't charge your card — retrying" | yes |
| `past_due` | active→past_due | "Your account is past due — update payment" | yes |
| `suspended` | dunning exhausted | "Your apps are suspended for non-payment" | yes |
| `recovered` | past_due→active | "Payment received — your account is active" | yes |
| `invoice_finalized` | new finalized invoice (incl. $0 paid-by-credit) | "Your {month} invoice: {total}" | yes |
| `refunded` | refund issued | "We refunded {amount} ({cash to card / credit to balance})" | yes |
| `disputed` | charge.dispute.created | "A charge was disputed — what happens next" | yes |
| `spend_warn` / `spend_degrade` / `spend_block` | spend-state transition | "App {name} reached {N}% of its spend limit" | yes |

Each template is rendered server-side; money amounts are formatted from the frozen invoice/refund fields (never re-priced). There is no per-template opt-out (all transactional); the global suppression list is the only suppression. Unsubscribe is therefore N/A — documented explicitly so a reviewer doesn't expect a preference UI.

---

## Observability — money-seam metrics <!-- Added in round 1: addressing MISSING-CONCEPT 5 -->

Mirroring the export `consecutive_failures`/`last_error` failure surface (`metering_exports`), the new money seams expose metrics so a stuck money path is visible BEFORE a creator complains. All are derivable from the schema (no new columns beyond `refunds.status`, `billing_notifications.status`, `credit_ledger`):
  - **`billing_refund_provider_failure_rate`** — ratio of refund attempts whose provider call errored (refunds left `pending` with no ref) over a window. The Stripe `Refund`/credit-note path is the highest-risk external call; a spike means card refunds are silently not happening.
  - **`billing_refund_stuck_pending_age_seconds`** (gauge, max) — age of the oldest `refunds.status='pending'` row. A `pending` refund older than `NOTIFY_REDRIVE_HORIZON` (the same 15min horizon governs refund re-drive) is a stuck money-back the operator owes a customer — page-worthy.
  - **`billing_outstanding_credit_liability_cents`** (gauge) — `SUM(credit_ledger.amount_cents)` across all non-expired grants minus consumed, fleet-wide. This is real platform liability (credit we owe as future discounts); it must be tracked for finance and to detect a credit-laundering anomaly (a sudden jump flags the CRITICAL-1 class).
  - **`billing_notify_send_failure_backlog`** (gauge) — count of `billing_notifications.status='pending'` rows past `NOTIFY_REDRIVE_HORIZON` (claimed but never sent). Mirrors the export backlog signal; a growing backlog means the mailer or the cron is wedged.
  - **`billing_dispute_open_count` / `billing_dispute_lost_amount_cents`** — open disputes and finalized chargeback losses, for fraud monitoring.
  - **`billing_void_reissue_count`** — corrections issued (a high rate flags a pricing bug upstream).

These are scraped from control via the existing metrics surface (same mechanism that exposes the export failure counters), not a new pipeline.

---

## PII / redaction posture for operator free-text fields <!-- Added in round 1: addressing MINOR-6 -->

The operator-supplied free-text fields — `refunds.reason`, `credit_ledger.note`, and (when added) any dispute note — are **operator-authored audit text**, not creator/end-user content. GDPR posture, consistent with the redesign's anonymize-retain model:
  - These fields are part of **financial-history records** (a refund, a credit grant) that GDPR Art. 17(3)(b)/(e) lets the platform RETAIN despite an erasure request — the invoice/refund/grant is the lawful financial artifact. So an operator note about a creator who is later erased **survives anonymize** along with the rest of the financial row (the row's `creator_id` FK target is anonymize-retained, not deleted, exactly as `invoices` are).
  - However, operators are instructed (and the endpoint validates) that these fields **MUST NOT contain end-user PII** (no end-user names/emails/addresses) — they reference internal ticket ids ("goodwill ticket #1234"), not personal data. The field is for *why the operator acted*, not *who the customer is*. A redaction lint on the write path warns on email-shaped / phone-shaped substrings.
  - The creator's own identity in these rows is already covered by the anonymize-in-place tombstone (`users.anonymized_at`); the free-text note is retained as financial audit but contains no *additional* PII beyond the lawful financial record. This is the explicit, stated posture — operator notes about an erased user survive anonymize **because they are financial audit, scrubbed of end-user PII by policy + lint**.

---

## PRODUCT DECISIONS

Twelve decisions, each now RATIFIED. (Round 1 grew the list from 7 to 11: decisions 8–11 are the money-correctness forks the round-1 critic surfaced. **Round 2 added decision 12** — the cash-collected mechanism fork: an `invoice_payments` side table vs. amending `invoices_immutable()` — and **revised decision 8**, since its round-1 "freeze a column" recommendation was mechanically impossible. **Round 3** stamps the user's calls.)

> **All decisions ratified (round 3).** The user greenlit **D1 = implement now** and made three explicit calls: **proration = FULL usage-segment** (an OVERRIDE of the recommended base-fee-only), **tax = build the seam, Native computes 0** (frozen into `tax_cents`), and **Connect = Express**. Every decision the user did not ask about keeps its recommended default, marked **ACCEPTED**: self-serve refunds = operator-only (D5), notification channel = email-only + `Mailer`→`crates/mailer` PR-0 (D4), void-reissue = partial unique index (D6), credit expiry = optional `expires_at` + per-grant consume, USD-pinned (D7), cash-collected = `invoice_payments` side table (D8/D12), idempotency = key + body fingerprint (D11). The decision the user OVERRODE (D1 proration) is the one substantive design change in round 3; its full usage-segment design is below.

1. **Proration granularity (v1). DECIDED — option (b), full usage-segment proration (user OVERRODE the recommended (a)).** Options: (a) base-fee-only [was REC]; (b) **full usage-segment proration [DECIDED]**; (c) defer all proration. — *The architect recommended (a)* (record the `plan_change_events` timeline from day one, but prorate only the base fee in v1, deferring usage-segment proration). **The user overrode this: v1 ships FULL usage-segment proration.** Each plan-segment within a period is priced under its OWN plan's weights/FX/included_units and frozen as a separate invoice line, so a mid-period plan change is money-correct on usage, not just the base fee. This requires (i) snapshotting the app's cumulative `usage_aggregates` totals onto each `plan_change_events` row at the change instant (`usage_at_change` JSONB), and (ii) a line-grain reshape of `invoice_lines` to allow multiple lines per app per invoice (`segment_no` in the PK, changeset `0054`). The full mechanism is the **"Full usage-segment proration"** section below; the no-change case degenerates to exactly one line per app, identical to today. *Rationale for accepting the override:* the snapshot-onto-line discipline already in the redesign extends cleanly to a per-segment snapshot, and the monotonic-cumulative shape of `usage_aggregates` makes the segment split deterministic and re-runnable — so the money-correct version is reachable without a per-event metering store.

2. **Tax. DECIDED — option (a), build the `TaxProvider` seam, Native computes `0`.** Options: (a) **build the `TaxProvider` seam, Native computes `0` [DECIDED]**; (b) build the seam AND enable Stripe Tax now; (c) no seam (hard-wire `0` forever). — *Decided (a), as recommended (the user ratified the rec):* a USD launch owes no tax, but baking `tax_cents = 0` into the reconciler forever means enabling tax later is a schema+reconciler surgery. The seam costs one trait + a no-op impl now; `NativeTaxProvider::compute` returns `0` and that `0` is **frozen into `tax_cents`** in the one-statement finalize UPDATE (per segment, in the usage-segment design). Enabling Stripe Tax later becomes a provider swap. `tax_cents` already exists, so (a) is schema-free.

3. **Refund destination + Stripe object. DECIDED — option (c), per-refund choice, default `credit`.** Options: (a) cash only; (b) credit only; (c) **per-refund choice, default `credit` [DECIDED]**. — *Decided (c), as recommended:* operators sometimes must refund to the card (chargeback avoidance) and sometimes to balance (goodwill that should stay on-platform). Per-refund `destination` with a `credit` default keeps money on-platform by default while allowing cash when needed. *Round 1 correction:* the *mechanism* per destination is pinned to the correct Stripe object — `cash` → a `Refund` (`re_…`) on the charge (a credit note `cn_…` is at most a bookkeeping wrapper), `credit` → a platform-native `credit_ledger('refund_to_credit')` grant (no Stripe call). See decision 10.

   **Connect account type. DECIDED — Express (user call).** The platform onboards creators as Stripe **Connect Express** accounts (not Standard, not Custom). Express gives the platform a Stripe-hosted onboarding + dashboard for creators while the platform retains control of the charge/payout flow and the 15% platform fee — the right balance for a Shopify-for-AI-apps model where creators want minimal billing setup but the platform owns the money rail. This bounds the refund/dispute mechanics above: refunds (`re_…`) and disputes are issued against charges on the **platform** account with the creator as the connected account, and the Stripe `Idempotency-Key` / `Stripe-Account` header posture is the Express-connected-account one. (Connect onboarding itself is Stream-2 work, outside this proposal's scope; recorded here so the refund/payout assumptions are pinned.)

4. **Notification channel + Mailer location. ACCEPTED — option (a), email-only v1 + `Mailer`→`crates/mailer` (PR-0).** Channel options: (a) **email-only v1 [ACCEPTED]**; (b) + creator-webhook; (c) + in-app. Mailer location: the existing `Mailer` lives in `crates/auth/src/mailer`; billing needs it from `crates/control`. — *Rec (a) + relocate `Mailer` to a NEW `crates/mailer`, as its own PR-0 prerequisite (round 1, MAJOR-4 — firm).* Email-only is the launch surface. **Not `crates/core`:** per AGENTS.md `crates/core` is strictly inter-service *wire types* (`RouteEntry`, `UsageReport`, etc.) — a `Mailer` trait + SMTP/Resend/stdout drivers + the suppression contract are an I/O capability, not a wire type, so they belong in a dedicated `crates/mailer`. The relocation drags `check_suppression`, the `email_suppressions` table access, the templates, the ~8 driver files, AND `crates/auth` still consumes it (so it is a cross-crate move, not a copy) — therefore it is scoped as **PR-0, landed BEFORE PR-1**, not bundled into the notify PR (PR-6). PR-6 then just adds the `BillingNotifier` over the already-relocated `Mailer`. The decision to ratify is now just "yes, `crates/mailer`, as PR-0".

5. **Self-serve refunds. ACCEPTED — option (a), operator-only v1.** Options: (a) **operator-only v1 [ACCEPTED]**; (b) creator self-serve with caps. — *Accepted (a), as recommended:* refunds touch real money and fraud surface. v1 keeps every refund `Action::BillingWrite` on `Resource::Any` (operator/master-key). Creator self-serve (with per-period caps + cooldowns) is a documented future, fail-closed off.

6. **Void-reissue mechanism. ACCEPTED — option (a), partial unique index `WHERE status <> 'void'`.** Options: (a) **partial unique index `WHERE status <> 'void'` [ACCEPTED]**; (b) credit-note-only correction (never void). — *Accepted (a), as recommended:* a credit note can adjust an amount but cannot fix a *wrong line structure* (wrong app, wrong period attribution). Void + reissue is the clean correction; the partial index is the minimal schema change that enables it. (Credit notes remain the mechanism for *refunds* of a *correct* invoice — decision 3.)

7. **Credit expiry + per-grant consume + currency. ACCEPTED — option (b), optional `expires_at` + per-grant consume, USD-pinned.** Options: (a) optional `expires_at` with a single aggregate `consumed` companion; (b) **optional `expires_at` with PER-GRANT consume (`consumed_from_grant_id`) [ACCEPTED]**; (c) forbid `expires_at` in v1. — *Round 1 forced this fork (MAJOR-2):* you cannot have both "optional expiry" AND "a single aggregate `consumed` companion is exact" — once a grant can expire, a single aggregate companion cannot attribute *which* grants were consumed (needed to know whether a still-unconsumed grant has expired). *Rec (b):* keep optional `expires_at` (promo credits expire) but consume **per-grant** — one `consumed` entry per drawn grant, naming `consumed_from_grant_id`, oldest-first. This makes expiry attribution exact at the cost of ~1–2 extra ledger rows per finalize. The earlier draft's option (a) is withdrawn as incorrect. Currency: v1 pins `currency='usd'` everywhere; the consume query filters `AND currency = invoice.currency` even in v1 (MINOR-5), so a stray non-USD grant is never drawn. Multi-currency stays a later policy, not a migration.

8. **Over-refund cap anchor (round 1 CRITICAL-1; round 2 CRITICAL-A — recommendation revised). ACCEPTED — option (b), cap on cash actually collected = `Σ(invoice_payments)`.** Options: (a) cap on `total_cents`; (b) **cap on cash actually collected [ACCEPTED]**. — *Accepted (b):* capping on `total_cents` enables credit-laundering (refund credit-funded value as cash/fresh-credit). The cap must be anchored on the cash actually charged to the PM (≠ `total_cents` in a dunning/partial-pay world). **Round 2 correction:** round 1 said "freeze a `cash_collected_cents` *column* on `invoices` at payment confirmation" — that is **mechanically impossible** (the payment webhook writes an already-finalized invoice, which `invoices_immutable()` rejects on every finalized→finalized UPDATE). The cash anchor is therefore `Σ(invoice_payments)` over the new side table (decision 12), not a column. The three bounds (`Σcash`, `Σcredit`, combined ≤ cash) are unchanged. Non-optional — (a) is a money defect.

9. **Void+reissue credit conservation (round 1, CRITICAL-2). ACCEPTED — option (b), append a `void_reversal` entry.** Options: (a) void does not touch the ledger (reissue re-consumes from the short balance — the bug); (b) **append a `void_reversal` entry inside the void txn restoring all credit the voided invoice consumed [ACCEPTED]**. — *Accepted (b):* (a) silently loses money and double-consumes. (b) conserves `Σ(ledger)` by construction; the reissue then re-draws from the restored balance. Non-optional.

10. **Refund Stripe object (round 1, CRITICAL-5). ACCEPTED — option (b), `Refund` (`re_…`) for cash, native ledger grant for credit.** Options: (a) credit note for everything (the wrong draft); (b) **`Refund` (`re_…`) on the charge for `cash`, platform-native ledger grant for `credit`, credit note only as optional bookkeeping [ACCEPTED]**. — *Accepted (b), verified against the Stripe API:* a credit note alone does not return cash on a paid invoice; the `Refund` object is the money movement. (a) would mean operators think they refunded a card when no money moved. Non-optional.

11. **Operator-endpoint idempotency (round 1, MISSING-CONCEPT 7; round 2, MINOR-A — body fingerprint added). ACCEPTED — option (c), `Idempotency-Key` + stored request-body fingerprint.** Options: (a) rely on claim-then-call only; (b) require an `Idempotency-Key` deduped by a `UNIQUE` key; (c) **(b) PLUS a stored request-body fingerprint, rejecting key-reuse-with-different-body [ACCEPTED]**. — *Accepted (c):* claim-then-call dedups a re-DRIVE of an already-recorded intent, but a double-CLICKED/retried operator call with no stored intent creates two refunds / two grants — the credit grant had NO guard at all in the original draft. A required idempotency key (UNIQUE on `refunds.idempotency_key` and on the credit-grant table) makes a same-body retry a no-op returning the first result. **Round 2:** a bare UNIQUE key silently returns the first result even when the key is reused with a *different* amount/destination — masking a real bug. Stripe 400s on that case (verified at `docs.stripe.com/api/idempotent_requests`). We mirror it: store a SHA-256 `request_fingerprint` and return 409 on key-reuse-with-different-body. Non-optional given the doc's own claim-then-call emphasis.

12. **Cash-collected mechanism (round 2, CRITICAL-A). ACCEPTED — option (b), append-only `invoice_payments` side table.** Options: (a) keep `cash_collected_cents` on `invoices` and amend `invoices_immutable()` to permit a finalized→finalized UPDATE iff the only changed column is `cash_collected_cents` (monotonic, others held equal); (b) **move payment tracking to an append-only `invoice_payments` side table; cash-collected = `Σ(invoice_payments)` [ACCEPTED]**. — *Accepted (b):* (a) works mechanically but opens a second legal finalized→finalized transition, forces the void path to reason about the new column, can't express a *decrease* (dispute clawback) under a monotonic CHECK, and erodes "a finalized invoice is immutable." (b) keeps the invoice byte-for-byte frozen, leaves `invoices_immutable()` untouched, expresses partial pays AND dispute clawbacks as natural rows, and is the same append-only-side-fact shape as `credit_ledger`/`refunds`/`billing_disputes` — i.e. the doc's own core principle applied to payments. (b) also makes the dispute-aware cap fall out for free (a negative `dispute_debit` row lowers the sum). Non-optional — (a) is the choice the round-1 doc *implied* but never wrote, and it is the weaker of the two.

---

## Build now vs. defer

**Build now (this proposal):**
- **PR-0** `crates/mailer` relocation (`Mailer` + drivers + suppression contract out of `crates/auth`; `crates/auth` re-points to it) — prerequisite for PR-6 (decision 4, MAJOR-4).
- `0042` partial-unique-index reshape (void releases the period claim). <!-- Round 2, CRITICAL-A: no longer adds cash_collected_cents — that column write was impossible; see 0053. invoices_immutable() left untouched. -->
- `0053` `invoice_payments` side table (append-only payment/partial-pay/dispute-clawback rows; cash-collected = `Σ(invoice_payments)`) **+ payment-webhook appends the `charge` row** (CRITICAL-A). Lands BEFORE `0049` in the changelog master (its trigger reads this table).
- `0048` `credit_ledger` + `credit_entry_kind` domain + kind↔sign CHECK + immutability trigger + grant idempotency key **+ `request_fingerprint`** (MINOR-A) + **per-grant** credit-apply-at-finalize (credit before tax, one `consumed` per drawn grant, balance = SUM, `void_reversal`).
- `0049` `refunds` + `refund_provider_refs` + **`Σ(invoice_payments)`-anchored over-refund trigger** + tax split + idempotency key **+ `request_fingerprint`** (MINOR-A) + `RefundProvider` seam → **Stripe `Refund` (re_…) for cash / native ledger grant for credit** (claim-then-call), per-refund cash/credit destination.
- `0050` `plan_change_events` (recorded on every `set_plan`, **server-derived money + cumulative `usage_at_change` snapshot + per-period cap**) + `0054` `invoice_lines` line-grain reshape (`segment_no` in the PK, **`plan_id NOT NULL`**, retargeted **named** line-provider-ref FK) + **FULL usage-segment proration** (round 3, decision 1 OVERRIDE; round 4 hardening): N+1 segments by **half-open day-partition**, each priced under its own plan (per-plan FX override) over the **floored** cumulative-snapshot usage delta, day-weighted base fee (remainder cent on last segment) + day-pro-rated included_units, zero-day segments merged, frozen as separate segment lines — **plus the segment-aware reconcile-loop rewrite** (segment-keyed Stripe idempotency key + `posted`/`line_exists` guards + line/provider-ref UPSERTs, CRITICAL-1) so N segments post N distinct items, and the cap-collapse tail prices under the running plan (MAJOR-4).
- `0051` `billing_notifications` (**two-phase claim-before-send** + status, **`NOTIFY_REDRIVE_HORIZON = 15min`**, MAJOR-B) + the `BillingNotifier` seam over the relocated `Mailer` **with a provider `Idempotency-Key`** (MAJOR-A) + the notify cron (advisory-locked, driven off the already-written spend/account history) + the surrogate-`id` add on the two history tables.
- `0052` `billing_disputes` + `dispute_status` domain + the `charge.dispute.*` webhook branch (MISSING-1) **+ the `dispute_debit`/`dispute_reversal` `invoice_payments` rows** (CRITICAL-A).
- PR-5 `TaxProvider` seam (Native = `0`, frozen into `tax_cents`) + tax-on-refund split.
- PR-7 `BillingRead` creator read APIs (invoice history, line detail, **cached/non-authoritative** projected charge, credit balance, PM status, plan/spend-state).

**Defer (documented, not built):**
- ~~**`invoice_lines` line-grain reshape** for full usage-segment proration~~ — **NOW BUILD-NOW (round 3, decision 1 OVERRIDE):** the line-grain reshape (`0054`) and full usage-segment proration ship in PR-4. (Was deferred under the recommended base-fee-only proration; the user overrode that.)
- **Stripe Tax (`automatic_tax`)** — a provider swap behind the PR-5 seam; built when the platform crosses a tax nexus (decision 2).
- **Creator self-serve refunds** (with caps + cooldowns) — fail-closed off; built post-launch (decision 5).
- **Creator-webhook + in-app notification channels** — email-only at launch (decision 4).
- **Multi-currency credits/refunds** (FX-at-consume policy) — USD-pinned v1 (decision 7).
- **Stripe-side credit mirroring** (`credit_amount` customer-balance transaction for `destination='credit'`) — v1 keeps credit-back platform-native; the `customer_balance_txn` ref_kind is reserved for when finance wants the credit liability mirrored on Stripe.

---

## Implementation plan (PR-0 … PR-8)

Each PR lands schema + code + a regression test that fails pre-fix and runs the REAL reconciler against a cyper/mock-Stripe (no shims). Money writes are operator-only; idempotency is claim-then-call with durable real-FK guards **plus operator idempotency keys**. All PRs are commit-only — **do NOT push**.

**PR-0 — `crates/mailer` relocation (prerequisite, MAJOR-4).** Move the `Mailer` trait + drivers (stdout/smtp/resend) + `check_suppression` + `email_suppressions` access + templates from `crates/auth/src/mailer` to a new `crates/mailer`; re-point `crates/auth` at it (it still consumes it). NO behaviour change.
  *Regression:* the existing `crates/auth` mailer tests pass unchanged against the relocated crate; a suppressed recipient is still skipped.

**PR-1 — Void + reissue + payments (`0042` reshape + `0053`).** Replace `UNIQUE(creator_id, period)` (DROP the name-pinned `invoices_creator_id_period_key`, verify vs live `\d`) with the partial unique index. **Round 2, CRITICAL-A: do NOT add `cash_collected_cents` and do NOT touch `invoices_immutable()`** — instead land `0053 invoice_payments` (table + `invoice_payment_kind` domain + immutability trigger + grants) and have the payment-confirmation webhook append a `charge` row; cash-collected is `Σ(invoice_payments)`. Rewrite the reconciler claim to `ON CONFLICT (creator_id, period) WHERE status <> 'void' DO NOTHING`; add operator `POST /invoices/{id}/void` and the reissue path **under the per-creator reconcile advisory lock** (CRITICAL-2 void_reversal append + re-claim → re-price → finalize), and the negative-invoice true-up auto-refund **subtracting cash refunds already issued** (round 2, CRITICAL-B: `over = Σpayments(old) − Σcash-refunds-issued(old) − total(new)`, floored at 0).
  *Regression:* voiding releases the period so a reissue mints a NEW `inv_…`; a second non-void invoice is still rejected; the payment webhook appends a `charge` row WITHOUT touching the finalized invoice (and a direct UPDATE of a finalized invoice still RAISEs — proving the side table sidesteps the trigger, CRITICAL-A); **CRITICAL-2:** consume→void→reissue conserves `Σ(credit_ledger)` (the worked-example sequence); **CRITICAL-B:** a refund-then-void-then-reissue where the corrected total is below cash collected auto-issues a true-up refund of exactly `Σpayments(old) − Σcash-refunds-issued(old) − total(new)` and the over-refund trigger ACCEPTS it (the round-1 formula would have over-refunded and been rejected — this test fails against the round-1 bridge). (All fail against the pre-reshape schema / a void that skips `void_reversal` / a true-up that ignores prior refunds.)

**PR-2 — Credits (`0048`).** `credit_ledger` + domain + kind↔sign CHECK + grant-ref CHECK + immutability trigger + grant idempotency index **+ `request_fingerprint`** (MINOR-A); **per-grant** credit-apply step in `bill_creator` (oldest-first, same-currency, one `consumed` per drawn grant with `consumed_from_grant_id`, `credit_cents` into the one-statement finalize UPDATE); operator `POST /billing/credit` grant endpoint **with required `Idempotency-Key` + body-fingerprint check**.
  *Regression:* a creator with a $5 grant billed $8 finalizes with `credit_cents=500`, balance CHECK holds, a `consumed −500` entry naming the grant exists; re-running `charge_cents` over the frozen snapshot reproduces the *pre-credit* line amount; a grant past `expires_at` is NOT consumed; a non-USD grant is NOT drawn against a USD bill (MINOR-5); a kind↔sign violation is rejected by the CHECK (MAJOR-1); a double-clicked grant with the same key + same body creates ONE entry; **MINOR-A:** the same key with a DIFFERENT amount returns 409, not the first grant.

**PR-3 — Refunds (`0049`).** `refunds` (with `subtotal/tax` split + idempotency key **+ `request_fingerprint`**, MINOR-A) + `refund_provider_refs` (`refund`/`credit_note` ref_kinds) + **`Σ(invoice_payments)`-anchored over-refund trigger** (reads the `0053` side table — which PR-1 landed first); `RefundProvider` seam (Native + Stripe) mirroring `MeteringProvider`, **`destination='cash'` → Stripe `Refund` (re_…), `destination='credit'` → native `refund_to_credit` grant**; operator `POST /invoices/{id}/refunds` (idempotency-keyed + fingerprint-checked, claim-then-call).
  *Regression (CRITICAL-1 + CRITICAL-5 + CRITICAL-A):* a `cash` refund creates a Stripe `Refund` (re_…), leaves the invoice `finalized`, writes a `'refund'` ref; the **credit-laundering scenario** (consume $40 + pay $60 → one `charge +6000` payment row → attempt $60 refund-to-credit) is REJECTED by the `Σ(invoice_payments)`-anchored trigger; a refund whose POST errors stays `pending` and the re-drive issues exactly one `re_…` (idempotent on the Stripe `Idempotency-Key`); a `destination='credit'` refund appends a `refund_to_credit` grant with NO Stripe charge reversal; a double-clicked refund (same key + body) creates ONE refund; **MINOR-A:** the same key with a DIFFERENT amount/destination returns 409, not the first refund; a taxed-invoice refund splits proportional tax (MISSING-6).

**PR-4 — Full usage-segment proration (`0050` + `0054` + the reconcile-loop rewrite).** round 3, decision 1 OVERRIDE; round 4 hardening. This PR lands the line-grain reshape, the cumulative-snapshot mechanism, **AND the segment-aware reconcile-loop rewrite (the missing 60%, CRITICAL-1).** `0050` `plan_change_events` with `usage_at_change` JSONB; `0054` `invoice_lines` line-grain reshape (add `segment_no` + **`plan_id NOT NULL`**, MAJOR-5; re-key the PK to `(invoice_id, app_id, segment_no)`; retarget the `billing_line_provider_refs` composite FK — DROP the **named** `billing_line_provider_refs_line_fk` (MINOR-7) and re-add it to the 3-col key). In `api.rs::set_plan` (after the existing authz probe at api.rs:602): append a **server-derived** row whose `from/to_base_fee_cents` come from the catalog AND whose `usage_at_change` is read from `usage_aggregates` **in the same txn as the `apps.plan_id` flip** (and under the **per-creator advisory lock**, MISSING-4), subject to the per-period change cap — **past the cap, flip `apps.plan_id` but record no snapshot; the tail prices under the running plan** (MAJOR-4). In `bill_creator`, the **reconcile-loop rewrite** (CRITICAL-1): rekey `posted`/`line_exists` to `HashSet<(Uuid, i16)>` on `(app_id, segment_no)`; add `segment_no` to `invoice_item_idempotency_key` (segment-aware — one Stripe item per segment, else N collide to one and under-bill); widen the line UPSERT `ON CONFLICT` to `(invoice_id, app_id, segment_no)` with `segment_no, plan_id` in the column list; widen the provider-ref INSERT `ON CONFLICT` to include `segment_no`. Build one `BilledSegment` per segment: split each app's period into **N+1 segments** by `effective_at` (half-open day-spans), compute each segment's usage-DELTA `max(0, end−start)` from consecutive cumulative snapshots (last segment ends at the period-end `usage_aggregates` totals; MAJOR-3 floor), **merge any zero-day segment into its neighbour** (MINOR-6), price each segment via `charge_cents(price{pro-rated base, day-pro-rated included_units, segment-plan FX}, segment usage-delta, GLOBAL weights)` with the per-metric floor applied per segment, day-weight each segment's base fee with the **remainder cent on the last segment** (CRITICAL-2), post one Stripe item per segment with a per-segment description (MISSING-3), and emit a SEPARATE frozen line per segment.
  *Regression:* **(degenerate)** an app with NO plan change bills exactly ONE line (`segment_no=0`, `plan_id=current`, full usage, full base fee, full quota) byte-for-byte identical to the pre-reshape single-line bill, posting ONE Stripe item with key `billitem:…:0`; **(2-segment posts 2 DISTINCT items + 2 lines, CRITICAL-1)** a one-change app posts exactly 2 distinct Stripe `invoice_item`s (2 distinct `Idempotency-Key`s) and 2 `invoice_lines` rows (`segment_no` 0,1) + 2 provider-refs — FAILS against the segment-blind key (which posts 1 item, under-billing segment 1); **(two-segment money correctness)** the worked sub-trace — Free `[1st,11th)` 10 days (quota pro-rated 333) then Pro `[11th,period_end)` 20 days (quota pro-rated 6667) with cumulative `requests` 0→4000→30000 — produces segment-0 `amount=3667` and segment-1 `amount=21333` (subtotal 25000), the two `usage_snapshot` deltas sum to the period-end total (4000+26000=30000), and the day-weighted base fees `0+2000` sum to ≤ one full Pro base fee; **(day-partition)** `Σ segment_days = 10+20 = 30 = days_in_period`, the change day (11th) is owned by segment 1 only (no shared boundary, CRITICAL-2); **(delta floor)** an END-missing metric yields delta 0, not a negative credit (MAJOR-3); **(zero-day merge)** two same-day `set_plan`s produce a merged line, not a quota-less spike line (MINOR-6); **(cap-collapse)** past the cap, a flip to an expensive higher-FX plan prices the tail under the RUNNING plan, not the last recorded cheap one (MAJOR-4); **(replay)** re-running `charge_cents` over each frozen segment line reproduces both amounts bit-for-bit, and a void+reissue re-derives the same two segments from the immutable `usage_at_change` snapshot; **(server-derived)** `usage_at_change` is read from `usage_aggregates`, never from the request body; **CRITICAL-4:** the body cannot supply a base fee and the (N+1)th change in a period records no new row (cap holds). (All fail against the pre-reshape single-line schema / the segment-blind idempotency key / a reconciler that reprices the whole period under the new plan / a snapshot read OUTSIDE the plan-flip txn.)

**PR-5 — Tax seam (code-only, no changeset).** `TaxProvider` trait (`#[async_trait(?Send)]`); `NativeTaxProvider::compute → 0`; `build_tax_provider` at boot; call after credit; write into the finalize UPDATE's `tax_cents`; tax-on-refund split in the refund endpoint.
  *Regression:* Native ⇒ `tax_cents=0`, `total=subtotal−credit`; a stub provider's `tax_cents` flows into `total` in ONE statement (balance CHECK holds); a refund of a (stub-)taxed invoice returns proportional tax.

**PR-6 — Notifications (`0051`).** (Mailer already relocated in PR-0.) Add the surrogate `id` to `spend_state_history` + `creator_billing_status_history`; `billing_notifications` (status, claimed_at) + domains; `BillingNotifier` seam **passing a provider `Idempotency-Key = (creator_id, kind, transition_id)` to `Mailer::send`** (round 2, MAJOR-A); the `cron/billing_notify` cron (**advisory-locked**, scan → **claim-before-send** → send → flip to `sent`, with **`NOTIFY_REDRIVE_HORIZON = 15min`** as the named re-drive horizon, round 2 MAJOR-B).
  *Regression (CRITICAL-3 + MAJOR-A/B):* an `active→past_due` row triggers exactly ONE `past_due` email; **two concurrent cron instances send the email exactly once** (the claim INSERT, not the send, arbitrates — fails against claim-after-success under simulated dual-flight); **MAJOR-A:** a crash in the send→flip window re-drives past `NOTIFY_REDRIVE_HORIZON` and re-sends, but `Mailer::send` carries the same provider `Idempotency-Key` so a dedup-honouring mock provider delivers ONCE (proving at-least-once delivery / exactly-once claim / idempotent effect); a send that fails leaves a `pending` row that the next tick (past `NOTIFY_REDRIVE_HORIZON`) re-drives for the SAME row; a suppressed recipient is skipped.

**PR-7 — Creator read APIs.** The six `BillingRead` endpoints in `api.rs`, each authz'd `Resource::App{id}` for owned apps with the operator `Resource::Any` probe-first fallback; the projected-charge endpoint re-runs `charge_cents` over the open period's live aggregates, **cached (60s TTL) + labelled non-authoritative** (MAJOR-5).
  *Regression:* a creator reading their OWN app's invoices succeeds; a NON-owned app is denied; an operator reads any; the credit-balance endpoint returns `SUM(credit_ledger)` for the caller's creator only; the projected-charge response carries `"authoritative": false` and a second call within the TTL does NOT re-price (cache hit).

**PR-8 — Disputes (`0052`).** `billing_disputes` + `dispute_status` domain; the `charge.dispute.created`/`.closed` webhook branch (claim-after-success on `stripe_events_seen`) **appending a negative `dispute_debit` / positive `dispute_reversal` `invoice_payments` row** (round 2, CRITICAL-A — so the dispute lowers `Σ(invoice_payments)` automatically); the belt-and-braces dispute-aware refund-cap double-check; the `disputed` notification.
  *Regression (MISSING-1 + CRITICAL-A):* a `charge.dispute.created` for an invoice records an `open` dispute, appends a `dispute_debit −amount` payment row, and fires one `disputed` email; a subsequent `destination='cash'` refund that would exceed the now-reduced `Σ(invoice_payments)` is rejected **by the trigger alone** (proving the negative payment row tightens the cap without a cross-table trigger); a `won` dispute appends a `dispute_reversal +amount` row restoring the budget; a re-delivered dispute webhook is deduped by `stripe_events_seen`.
