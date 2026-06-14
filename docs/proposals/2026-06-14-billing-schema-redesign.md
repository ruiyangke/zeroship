# Billing & Metering Schema Redesign (v3.4)

**Status: HARDENED (critic→reviser 3 rounds, score 93/100) — ready for implementation review.** Pre-launch clean rewrite of the billing changesets. Not a live migration. No back-compat. Every table was verified against the live code that reads/writes it (control `pricing`/`spend`/`metering`/`account_status`/`stripe_*`, the `billing_reconcile`/`spend_reconcile`/`metering_export`/`dunning` crons, the 3 metering providers, the `account_reaper`, and changesets `0037`–`0043`/`0045`/`0046`); all idempotency, spend-enforcement, snapshot-replay, G2 order-safe suspension, and G6 webhook-dedup invariants confirmed preserved.
**Date:** 2026-06-14
**Branch / worktree:** `feat/billing-metering` · `/home/ruiyang/Projects/appbase-billing`
**Scope:** replaces the current **14-table** billing surface (changesets `0037`,`0038`,`0039`,`0040`,`0041`,`0042`,`0043`,`0045`,`0046` — note there is **no billing `0044`**; `0044` is reserved for the unrelated G1 work) with a reproducible, provider-agnostic, FK-anchored model. Net: **19 tables + 2 immutability triggers + 5 domains.**

> **Changeset numbering.** The current billing tables live across `0037`–`0043`, `0045` (G2 account-status), and `0046` (G6 webhook-dedup). This redesign consolidates them into **`0037`–`0043`** (re-using those exact slots for a clean re-author) plus **`0047`** for the account-status + webhook-dedup tables (the next free slot after the live `0046`; the `0044`/`0045`/`0046` numbers are NOT re-used — `0044` is G1's, and `0045`/`0046` are superseded-in-place by the `0037`–`0043` + `0047` set on a clean dev/test re-migrate). The redesign accounts for the **NOW-current** schema, i.e. it must carry forward G2's `creator_billing_status` (incl. its order-safety columns) **and** G6's `stripe_events_seen`.

---

## Executive summary

The current billing schema bills correctly only by luck: it mutates `plans`, `metric_weights`, and `pricing_config` in place, so the month-end reconciler reprices the whole period at *current* rates — a mid-period price change retroactively rewrites history and the price-at-usage-time is unrecoverable. Metrics are free-text with no catalog, so an emitted metric with no weight is silently billed `$0`. The invoice model is Stripe-shaped (`stripe_invoice_id` / `stripe_item_id` baked into core tables) despite a pluggable-provider mandate. Money mixes cents and pico-cents, periods are `TIMESTAMPTZ` round-tripped through `f64`, several FKs are missing, and unbounded ledgers have no retention story.

This redesign fixes all of that without versioned rate cards or a general ledger (both deferred). The single reproducibility mechanism is **snapshot-onto-line**: at finalize, each invoice line freezes the *inputs* to the charge function — the raw usage map, the applied weights, the resolved FX, the applied included-units quota, and the base fee — so any finalized bill replays bit-for-bit by re-running `charge_cents` over the frozen snapshot. Weights, FX, and plans therefore stay un-versioned and simple.

Around that core: a **`billing_metrics` catalog** is the FK spine for every metric reference (no silent `$0`; custom SDK metrics auto-register, capped + GC'd + app-attributed); a migration-time **seed-parity assertion** fails loudly if a billable metric lacks a weight; `period` becomes a **`DATE` domain** pinned to first-of-month (killing the `f64` round-trip); spend state splits into **config (`app_spend_limit`) vs derived (`app_spend_state`)** to end a three-writer clobber dance; the invoice model becomes **provider-agnostic** (`invoices` + `invoice_lines` with a balance CHECK and immutability triggers) with all provider ids relocated to **real-FK side tables** (`billing_customer_refs`, `billing_provider_refs`, `billing_line_provider_refs`); account/dunning state (`creator_billing_status`, **carrying forward G2's `last_event_at`/`last_recovered_at` order-safety high-water columns verbatim**) is pulled fully into scope and re-keyed onto `creator_billing` so the FK and erase policy are uniform end-to-end; and G6's **`stripe_events_seen`** webhook replay-dedup ledger is carried forward unchanged (control-internal, no RLS, append-only) so each verified webhook still processes at-most-once.

The local spend-enforcement aggregate (`usage_aggregates`) stays local, fast, single-row-per-`(app, period, metric)`, and provider-independent — the gateway's Warn/Degrade/Block path can never be outsourced to a billing backend. Idempotent at-least-once ingest (dedup on `(worker_id, sequence)`, never on period) and idempotent invoicing (finalize-in-one-statement, durable real-FK provider-ref guards) are both preserved exactly, including the two prior holistic-review fixes (crash-window and spend↔invoice divergence). Account erase **conforms to the existing reaper's anonymize-on-financial-history model** rather than inventing a parallel RESTRICT path: all billing tables stay `ON DELETE CASCADE` from `users`, and `user_has_financial_history` is widened to recognize invoiced creators.

---

## Design principles (industrial best practice applied)

1. **Reproducible bills via snapshot-onto-line (Stripe / Metronome / Orb / Lago).** A finalized invoice line is an immutable record of the *inputs* to its charge: usage, weights, FX, quota, base fee. Re-pricing reads the snapshot, never the live rate tables. This buys reproducibility without the operational weight of versioned rate cards — those are deferred precisely because snapshotting already makes finalized bills auditable and replayable.

2. **A metric catalog is the FK spine (Amberflo / OpenMeter meter registry).** Every `metric` reference (`usage_aggregates`, `metric_weights`, line snapshots) FKs `billing_metrics`. No free-text metric can enter the system unregistered; no billable metric can silently price at `$0` (enforced both by FK and by a migration-time parity assertion).

3. **Neutral compute-unit (CU) intermediate + FX (telco/cloud rating).** `raw metrics × weights = CU`, `CU × FX = cents`. CU is the provider-neutral unit. FX is a single global pico-cents rate (with a per-plan override), floored to avoid rounding-to-zero. CU is *derived* at pricing time, never stored as a column, so it cannot drift from its inputs.

4. **Provider-agnostic invoice/line model + provider-ref seam (Lago / OpenMeter pluggability).** Core `invoices` / `invoice_lines` carry no provider ids. Stripe/OpenMeter/native ids live in narrow side tables joined by real FKs. Swapping the billing backend touches only the seam, never the core money tables.

5. **Money correctness via CHECK + immutability (double-entry discipline, lightweight).** A balance CHECK (`total = subtotal − credit + tax`) holds on every invoice row; finalize writes all money columns in one statement so the CHECK never sees a half-written row. Triggers make finalized invoices and their lines append-only (only `finalized → void`). Single money unit per surface: cents for charges, pico-cents for FX, no mixing.

6. **Idempotency + watermarks (at-least-once metering ledgers).** Ingest dedups on `(worker_id, sequence)` — never on period, so a report straddling a month boundary cannot double-apply. Invoicing claims a period with `ON CONFLICT (creator_id, period) DO NOTHING` and short-circuits on `status='finalized'`; per-line provider-ref rows (real FKs) are the durable replay guards.

7. **Effective-dated periods as `DATE` (billing-period hygiene).** `period` is a `DATE` domain pinned to the first of the month — no `TIMESTAMPTZ`/`f64` round-trip, drop-in retention pruning, range-partition-ready.

8. **Entitlements / quotas as frozen line state.** `included_units` (the free CU quota) is applied per line and frozen onto the snapshot, so quota changes never retroactively reprice a finalized period.

9. **Retention & partitioning pre-staged, not prematurely built.** Unbounded growers (`usage_reports_seen`, `spend_state_history`, `usage_aggregates`) get a `period DATE` and a documented Rust retention stub (disabled), with range-partitioning as the at-scale answer. Built only on a measured signal.

10. **Fail-closed RLS + least-privilege grants (existing platform pattern).** App-keyed tables FORCE RLS on `current_setting('zeroship.tenant_app')`; creator-keyed tables are RLS-exempt (control runs BYPASSRLS; the key is not an `app_id`). Grants are explicit per-table, role-guarded.

---

## Full schema — consolidated Liquibase changesets

These replace the live billing changesets (`0037`–`0043`, `0045`, `0046`) on a clean dev/test re-migrate. File order preserves FK precedence (domains → catalog → cost model → metering → plans → spend → invoicing → exports → account-status + webhook-dedup). Numbering: `0037`–`0043` are re-authored in place; the account-status and webhook-dedup tables move to **`0047`** (the next free slot after the live `0046`, leaving G1's `0044` untouched).

### `0037` — `billing_domains_and_metrics.sql`

```sql
--liquibase formatted sql

--changeset zeroship:billing-domains splitStatements:true
-- period DATE pinned to first-of-month (kills the f64 round-trip in metering/mod.rs).
-- DOMAIN-over-CHECK keeps the wire types the bespoke compio-postgres driver already
-- round-trips (DATE / TEXT) — a native ENUM would force an enum codec for zero added safety.
CREATE DOMAIN zeroship.billing_period AS DATE
    CHECK (EXTRACT(DAY FROM VALUE) = 1);
-- spend states: membership only. Ordering (allow<warn<degrade<block) + transition
-- legality live in Rust (spend.rs severity()); the DB does NOT encode rank.
CREATE DOMAIN zeroship.spend_state AS TEXT
    CHECK (VALUE IN ('allow','warn','degrade','block'));
-- account (dunning) states: membership only; lifecycle lives in account_status.rs.
CREATE DOMAIN zeroship.account_state AS TEXT
    CHECK (VALUE IN ('active','past_due','suspended'));
-- metric provenance.
CREATE DOMAIN zeroship.metric_kind AS TEXT
    CHECK (VALUE IN ('platform','primitive','custom'));
-- invoice lifecycle. Period-claim short-circuit + immutability triggers key off this.
CREATE DOMAIN zeroship.invoice_status AS TEXT
    CHECK (VALUE IN ('draft','finalized','void'));
--rollback DROP DOMAIN IF EXISTS zeroship.invoice_status;
--rollback DROP DOMAIN IF EXISTS zeroship.metric_kind;
--rollback DROP DOMAIN IF EXISTS zeroship.account_state;
--rollback DROP DOMAIN IF EXISTS zeroship.spend_state;
--rollback DROP DOMAIN IF EXISTS zeroship.billing_period;

--changeset zeroship:billing-metrics splitStatements:true
-- GLOBAL operator catalog (mirrors plans): NOT tenant-scoped, NO RLS (control is
-- BYPASSRLS). Platform/primitive rows seeded; custom SDK metrics auto-register on
-- first ingest (capped + GC'd) so usage_aggregates.metric FK always resolves.
CREATE TABLE zeroship.billing_metrics (
    metric       TEXT PRIMARY KEY,            -- 'requests','cpu_us',…, custom names
    kind         zeroship.metric_kind NOT NULL,
    unit         TEXT NOT NULL,               -- 'request','microsecond','byte' (display/audit)
    archived     BOOLEAN NOT NULL DEFAULT false,
    last_seen_at TIMESTAMPTZ,                 -- bumped on custom-metric ingest; drives GC
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
--rollback DROP TABLE zeroship.billing_metrics;

--changeset zeroship:billing-metrics-custom-owner splitStatements:true
-- Custom metrics must be attributable + cappable + GC-able. Platform/primitive rows
-- have NULL owner_app (global, never GC'd).
ALTER TABLE zeroship.billing_metrics
    ADD COLUMN owner_app UUID REFERENCES zeroship.apps(id) ON DELETE CASCADE;
CREATE INDEX billing_metrics_owner_app_idx ON zeroship.billing_metrics (owner_app)
    WHERE owner_app IS NOT NULL;
--rollback DROP INDEX IF EXISTS zeroship.billing_metrics_owner_app_idx;
--rollback ALTER TABLE zeroship.billing_metrics DROP COLUMN owner_app;

--changeset zeroship:billing-metrics-seed splitStatements:true
-- Mirrors the 0038 metric_weights seed names EXACTLY (parity ENFORCED by 0038's
-- seed-weight-parity-assert): every billable weight has a catalog parent; every
-- cataloged billable metric has a weight (no silent $0). Seeded here so the FKs in
-- 0038/0039 resolve at first boot regardless of seed_plans timing.
INSERT INTO zeroship.billing_metrics (metric, kind, unit) VALUES
    ('requests',            'platform',  'request'),
    ('cpu_us',              'platform',  'microsecond'),
    ('wall_us',             'platform',  'microsecond'),
    ('ingress_bytes',       'platform',  'byte'),
    ('egress_bytes',        'platform',  'byte'),
    ('db_reads',            'primitive', 'op'),
    ('db_writes',           'primitive', 'op'),
    ('db_rows_written',     'primitive', 'row'),
    ('kv_reads',            'primitive', 'op'),
    ('kv_writes',           'primitive', 'op'),
    ('storage_ops',         'primitive', 'op'),
    ('storage_bytes',       'primitive', 'byte'),
    ('storage_egress_bytes','primitive', 'byte')
ON CONFLICT (metric) DO NOTHING;
--rollback DELETE FROM zeroship.billing_metrics;

--changeset zeroship:billing-metrics-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.billing_metrics TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.billing_metrics FROM zeroship_control'; END IF; END $rb$;
```

### `0038` — `cost_model.sql`

```sql
--liquibase formatted sql

--changeset zeroship:metric-weights splitStatements:true
-- CU stays DERIVED from raw totals at pricing time (no CU column). Reproducibility
-- for FINALIZED periods comes from snapshot-onto-line, so weights/FX need NOT be
-- versioned.
CREATE TABLE zeroship.metric_weights (
    metric       TEXT        PRIMARY KEY REFERENCES zeroship.billing_metrics(metric) ON DELETE RESTRICT,
    units_per_op BIGINT      NOT NULL CHECK (units_per_op >= 0),
    per_units    BIGINT      NOT NULL CHECK (per_units > 0),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE TABLE zeroship.pricing_config (
    id                     TEXT        PRIMARY KEY DEFAULT 'global' CHECK (id = 'global'),
    fx_pico_cents_per_unit BIGINT      NOT NULL CHECK (fx_pico_cents_per_unit >= 1000),  -- floor = MIN_FX_PICO_CENTS_PER_UNIT
    updated_at             TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
--rollback DROP TABLE zeroship.pricing_config;
--rollback DROP TABLE zeroship.metric_weights;

--changeset zeroship:metric-weights-seed splitStatements:true
INSERT INTO zeroship.metric_weights (metric, units_per_op, per_units) VALUES
    ('requests',            1, 1),
    ('cpu_us',              1, 1000),
    ('wall_us',             1, 10000),
    ('ingress_bytes',       1, 10000),
    ('egress_bytes',        1, 1000),
    ('db_reads',            1, 1),
    ('db_writes',           2, 1),
    ('db_rows_written',     1, 50),
    ('kv_reads',            1, 1),
    ('kv_writes',           2, 1),
    ('storage_ops',         2, 1),
    ('storage_bytes',       1, 1000),
    ('storage_egress_bytes',1, 1000)
ON CONFLICT (metric) DO NOTHING;
INSERT INTO zeroship.pricing_config (id, fx_pico_cents_per_unit) VALUES ('global', 30000000)
ON CONFLICT (id) DO NOTHING;
--rollback DELETE FROM zeroship.pricing_config WHERE id = 'global';
--rollback DELETE FROM zeroship.metric_weights;

--changeset zeroship:seed-weight-parity-assert splitStatements:false
-- ENFORCE "no silent $0" at migration time: every non-custom cataloged metric MUST
-- have a weight (the FK gives the other direction). Fails the migration loudly if the
-- two hand-maintained seed lists diverge.
DO $assert$
DECLARE missing TEXT;
BEGIN
    SELECT string_agg(m.metric, ', ') INTO missing
    FROM zeroship.billing_metrics m
    LEFT JOIN zeroship.metric_weights w ON w.metric = m.metric
    WHERE m.kind IN ('platform','primitive') AND m.archived = false AND w.metric IS NULL;
    IF missing IS NOT NULL THEN
        RAISE EXCEPTION 'billing seed parity: cataloged billable metric(s) without a weight (silent $0): %', missing;
    END IF;
END
$assert$;
--rollback SELECT 1;

--changeset zeroship:compute-unit-pricing-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.metric_weights TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.pricing_config TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.metric_weights FROM zeroship_control'; EXECUTE 'REVOKE ALL ON zeroship.pricing_config FROM zeroship_control'; END IF; END $rb$;
```

### `0039` — `metering_usage.sql`

```sql
--liquibase formatted sql

--changeset zeroship:metering-usage-aggregates splitStatements:true
-- The LOCAL, fast, provider-independent enforcement fact. Single-row-per-
-- (app,period,metric) KEPT — slot=worker_id sharding is the future answer to write
-- contention but there is no measured contention pre-launch.
CREATE TABLE zeroship.usage_aggregates (
    app_id     UUID                    NOT NULL REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    period     zeroship.billing_period NOT NULL,
    metric     TEXT                    NOT NULL REFERENCES zeroship.billing_metrics(metric) ON DELETE RESTRICT,
    total      BIGINT                  NOT NULL DEFAULT 0 CHECK (total >= 0),
    updated_at TIMESTAMPTZ             NOT NULL DEFAULT NOW(),
    PRIMARY KEY (app_id, period, metric)
);
-- The spend/export sweeps read the whole fleet's CURRENT period (WHERE period=$1).
-- PK leads with app_id, so add the period-leading index.
CREATE INDEX usage_aggregates_period_idx ON zeroship.usage_aggregates (period, app_id);
--rollback DROP INDEX IF EXISTS zeroship.usage_aggregates_period_idx;
--rollback DROP TABLE zeroship.usage_aggregates;

--changeset zeroship:metering-usage-aggregates-rls splitStatements:true
ALTER TABLE zeroship.usage_aggregates ENABLE ROW LEVEL SECURITY;
ALTER TABLE zeroship.usage_aggregates FORCE  ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON zeroship.usage_aggregates
    USING      (app_id = current_setting('zeroship.tenant_app', true)::uuid)
    WITH CHECK (app_id = current_setting('zeroship.tenant_app', true)::uuid);
--rollback DROP POLICY IF EXISTS tenant_isolation ON zeroship.usage_aggregates;
--rollback ALTER TABLE zeroship.usage_aggregates NO FORCE ROW LEVEL SECURITY;
--rollback ALTER TABLE zeroship.usage_aggregates DISABLE ROW LEVEL SECURITY;

--changeset zeroship:metering-usage-reports-seen splitStatements:true
-- IDEMPOTENCY INVARIANT: the dedup gate is `INSERT … (worker_id, sequence) ON CONFLICT
-- DO NOTHING`; the period is derived AT INGEST TIME, not carried on the report.
-- `period` MUST NOT be in the dedup PRIMARY KEY: a retried report straddling the UTC
-- month boundary would hash to a new key, pass the gate, and DOUBLE-APPLY.
-- POSTURE: `period` is present + NULLable but is NOT WRITTEN at ingest today. The hot
-- ingest INSERT stays EXACTLY `(worker_id, sequence)` — NO third bind, ZERO write-
-- amplification. The column + its index are introduced for retention; the retention-
-- activation PR (the ONLY consumer) is what starts populating it and adds the period
-- index in the SAME PR. Until then the column is inert.
CREATE TABLE zeroship.usage_reports_seen (
    worker_id TEXT                    NOT NULL,
    sequence  BIGINT                  NOT NULL,
    period    zeroship.billing_period,           -- NULLable; retention-only; NOT written at ingest
    seen_at   TIMESTAMPTZ             NOT NULL DEFAULT NOW(),
    PRIMARY KEY (worker_id, sequence)
);
-- (NO period index here. Added by the retention-activation PR alongside the cron.)
--rollback DROP TABLE zeroship.usage_reports_seen;

--changeset zeroship:metering-usage-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.usage_aggregates   TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, DELETE         ON zeroship.usage_reports_seen TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.usage_aggregates FROM zeroship_control'; EXECUTE 'REVOKE ALL ON zeroship.usage_reports_seen FROM zeroship_control'; END IF; END $rb$;
```

### `0040` — `plans.sql`

```sql
--liquibase formatted sql

--changeset zeroship:plan-catalog splitStatements:true
-- Priced identity. runtime_limits_json STAYS: the 5s route-pull LEFT JOINs it. The
-- billing/sandbox coupling critique is resolved by keeping it OFF the price semantics
-- (plan identity, not a priced field), NOT by removing it. The poison-tolerance fix
-- lives in runtime_limits_from_catalog().
CREATE TABLE zeroship.plans (
    id                        TEXT        PRIMARY KEY,           -- pln_<base62>
    name                      TEXT        NOT NULL,
    base_fee_cents            BIGINT      NOT NULL DEFAULT 0 CHECK (base_fee_cents >= 0),
    included_units            BIGINT      NOT NULL DEFAULT 0 CHECK (included_units >= 0),  -- CU free before overage
    fx_pico_cents_per_unit    BIGINT      CHECK (fx_pico_cents_per_unit IS NULL OR fx_pico_cents_per_unit >= 1000),  -- NULL ⇒ global default
    spend_limit_default_cents BIGINT      NOT NULL DEFAULT 0 CHECK (spend_limit_default_cents >= 0),
    assignable_by_creator     BOOLEAN     NOT NULL DEFAULT false,
    runtime_limits_json       JSONB       NOT NULL,              -- AppRuntimeLimits; identity, NOT a priced field
    archived                  BOOLEAN     NOT NULL DEFAULT false,
    created_at                TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at                TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
ALTER TABLE zeroship.apps
    ADD CONSTRAINT apps_plan_fk FOREIGN KEY (plan_id) REFERENCES zeroship.plans(id) ON DELETE RESTRICT;
CREATE INDEX apps_plan_id_idx ON zeroship.apps(plan_id);
--rollback DROP INDEX IF EXISTS zeroship.apps_plan_id_idx;
--rollback ALTER TABLE zeroship.apps DROP CONSTRAINT apps_plan_fk;
--rollback DROP TABLE zeroship.plans;

--changeset zeroship:plan-catalog-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.plans TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.plans FROM zeroship_control'; END IF; END $rb$;
```

### `0041` — `spend.sql`

```sql
--liquibase formatted sql

--changeset zeroship:app-spend-limit splitStatements:true
-- CONFIG: per-app override, written ONLY by set_limit. Absent ⇒ plan default.
CREATE TABLE zeroship.app_spend_limit (
    app_id            UUID PRIMARY KEY REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    spend_limit_cents BIGINT CHECK (spend_limit_cents IS NULL OR spend_limit_cents >= 0),  -- override; NULL clears
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
-- DERIVED: the hot enforcement state, UPSERTed every ~60s; the route-pull LEFT JOINs
-- s.state AS spend_state.
CREATE TABLE zeroship.app_spend_state (
    app_id           UUID PRIMARY KEY REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    state            zeroship.spend_state    NOT NULL DEFAULT 'allow',
    spend_cents      BIGINT                  NOT NULL DEFAULT 0 CHECK (spend_cents >= 0),
    eval_limit_cents BIGINT                  NOT NULL DEFAULT 0 CHECK (eval_limit_cents >= 0),
    period           zeroship.billing_period NOT NULL,
    evaluated_at     TIMESTAMPTZ             NOT NULL DEFAULT NOW()
);
-- Append-only transition audit; typed states; period bound in code.
CREATE TABLE zeroship.spend_state_history (
    app_id      UUID                    NOT NULL REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    period      zeroship.billing_period NOT NULL,
    from_state  zeroship.spend_state    NOT NULL,
    to_state    zeroship.spend_state    NOT NULL,
    spend_cents BIGINT                  NOT NULL CHECK (spend_cents >= 0),
    limit_cents BIGINT                  CHECK (limit_cents IS NULL OR limit_cents >= 0),
    at          TIMESTAMPTZ             NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_spend_state_history_app_at ON zeroship.spend_state_history (app_id, at DESC);
CREATE INDEX idx_spend_state_history_period ON zeroship.spend_state_history (period);
--rollback DROP INDEX IF EXISTS zeroship.idx_spend_state_history_period;
--rollback DROP INDEX IF EXISTS zeroship.idx_spend_state_history_app_at;
--rollback DROP TABLE zeroship.spend_state_history;
--rollback DROP TABLE zeroship.app_spend_state;
--rollback DROP TABLE zeroship.app_spend_limit;

--changeset zeroship:app-spend-rls splitStatements:true
ALTER TABLE zeroship.app_spend_limit     ENABLE ROW LEVEL SECURITY;
ALTER TABLE zeroship.app_spend_limit     FORCE  ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON zeroship.app_spend_limit
    USING      (app_id = current_setting('zeroship.tenant_app', true)::uuid)
    WITH CHECK (app_id = current_setting('zeroship.tenant_app', true)::uuid);
ALTER TABLE zeroship.app_spend_state     ENABLE ROW LEVEL SECURITY;
ALTER TABLE zeroship.app_spend_state     FORCE  ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON zeroship.app_spend_state
    USING (app_id = current_setting('zeroship.tenant_app', true)::uuid);
ALTER TABLE zeroship.spend_state_history ENABLE ROW LEVEL SECURITY;
ALTER TABLE zeroship.spend_state_history FORCE  ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON zeroship.spend_state_history
    USING (app_id = current_setting('zeroship.tenant_app', true)::uuid);
--rollback DROP POLICY IF EXISTS tenant_isolation ON zeroship.spend_state_history;
--rollback ALTER TABLE zeroship.spend_state_history NO FORCE ROW LEVEL SECURITY;
--rollback ALTER TABLE zeroship.spend_state_history DISABLE ROW LEVEL SECURITY;
--rollback DROP POLICY IF EXISTS tenant_isolation ON zeroship.app_spend_state;
--rollback ALTER TABLE zeroship.app_spend_state NO FORCE ROW LEVEL SECURITY;
--rollback ALTER TABLE zeroship.app_spend_state DISABLE ROW LEVEL SECURITY;
--rollback DROP POLICY IF EXISTS tenant_isolation ON zeroship.app_spend_limit;
--rollback ALTER TABLE zeroship.app_spend_limit NO FORCE ROW LEVEL SECURITY;
--rollback ALTER TABLE zeroship.app_spend_limit DISABLE ROW LEVEL SECURITY;

--changeset zeroship:app-spend-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.app_spend_limit     TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.app_spend_state     TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, DELETE ON zeroship.spend_state_history TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.app_spend_limit FROM zeroship_control'; EXECUTE 'REVOKE ALL ON zeroship.app_spend_state FROM zeroship_control'; EXECUTE 'REVOKE ALL ON zeroship.spend_state_history FROM zeroship_control'; END IF; END $rb$;
```

### `0042` — `invoicing.sql`

```sql
--liquibase formatted sql

--changeset zeroship:creator-billing splitStatements:true
-- Identity only; stripe_customer_id MOVES to billing_customer_refs (REAL FK).
-- Creator(user)-keyed control bookkeeping ⇒ no app RLS.
-- creator_id STAYS ON DELETE CASCADE from users (uniform with the existing erase
-- model). account_reaper.rs retains the users row (anonymize-in-place) whenever the
-- creator has financial history, so the FK target survives and CASCADE never fires for
-- an ever-billed creator; a never-billed creator is hard-deleted and CASCADE cleanly
-- reaps this empty shell. The erase tombstone lives on users.anonymized_at — NOT here.
CREATE TABLE zeroship.creator_billing (
    creator_id     UUID PRIMARY KEY REFERENCES zeroship.users(id) ON DELETE CASCADE,
    default_pm_set BOOLEAN NOT NULL DEFAULT false,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
--rollback DROP TABLE zeroship.creator_billing;

--changeset zeroship:billing-customer-refs splitStatements:true
-- Customer↔provider mapping: a NARROW table with a REAL FK to creator_billing. A
-- stray/buggy writer CANNOT orphan a customer ref. One creator holds one ref PER
-- provider ⇒ PK (creator_id, provider); external_id globally unique per provider ⇒
-- UNIQUE (provider, external_id) backs the reverse lookup.
CREATE TABLE zeroship.billing_customer_refs (
    creator_id  UUID NOT NULL REFERENCES zeroship.creator_billing(creator_id) ON DELETE CASCADE,
    -- 'stripe' (the platform Customer — shared by the Native invoice rail AND
    -- Stripe Billing Meters, which posts meter events against the same cus_…) |
    -- 'openmeter' (a distinct external customer handle). Stripe-Meters does NOT
    -- get its own ref; see Key flow E.
    provider    TEXT NOT NULL,
    external_id TEXT NOT NULL,                -- cus_… (stripe) / external handle (openmeter)
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (creator_id, provider),
    -- Reverse lookup: the webhook resolves creator FROM a customer id with NO
    -- provider in hand (stripe_handlers.rs:874 get_creator_by_customer(customer)).
    -- A provider customer id (cus_…) is globally unique, so the reverse lookup is
    -- `WHERE external_id = $1` and a STANDALONE UNIQUE(external_id) makes it
    -- constraint-guaranteed-singular — NOT merely UNIQUE(provider, external_id),
    -- which would not back a providerless probe. UNIQUE(provider, external_id)
    -- additionally documents per-provider scoping but is subsumed by the stricter
    -- global unique below.
    UNIQUE (external_id)
);
--rollback DROP TABLE zeroship.billing_customer_refs;

--changeset zeroship:invoices splitStatements:true
-- Provider-agnostic per-(creator, period) claim. UNIQUE(creator_id, period) == old
-- billing_runs PK (the no-double-bill claim). status carries the short-circuit
-- (finalized ⇒ skip), replacing "stripe_invoice_id NOT NULL".
CREATE TABLE zeroship.invoices (
    id             TEXT                    PRIMARY KEY,           -- inv_<base62>
    creator_id     UUID                    NOT NULL REFERENCES zeroship.creator_billing(creator_id) ON DELETE CASCADE,
    period         zeroship.billing_period NOT NULL,
    status         zeroship.invoice_status NOT NULL DEFAULT 'draft',
    currency       CHAR(3)                 NOT NULL DEFAULT 'usd' CHECK (currency ~ '^[a-z]{3}$'),
    subtotal_cents BIGINT                  NOT NULL DEFAULT 0 CHECK (subtotal_cents >= 0),
    credit_cents   BIGINT                  NOT NULL DEFAULT 0 CHECK (credit_cents   >= 0),  -- 0 at launch; shape ready
    tax_cents      BIGINT                  NOT NULL DEFAULT 0 CHECK (tax_cents       >= 0),  -- 0 at launch
    total_cents    BIGINT                  NOT NULL DEFAULT 0 CHECK (total_cents     >= 0),
    finalized_at   TIMESTAMPTZ,
    voided_at      TIMESTAMPTZ,
    created_at     TIMESTAMPTZ             NOT NULL DEFAULT NOW(),
    updated_at     TIMESTAMPTZ             NOT NULL DEFAULT NOW(),
    CONSTRAINT invoice_total_balances CHECK (total_cents = subtotal_cents - credit_cents + tax_cents),
    UNIQUE (creator_id, period)
);
--rollback DROP TABLE zeroship.invoices;

--changeset zeroship:invoice-lines splitStatements:true
-- Per-app line. PK (invoice_id, app_id) == billing_run_items grain. The SNAPSHOT block
-- is the reproducibility fix. app_id gets the FK it lacked, RESTRICT.
-- total_units/billable_units are DERIVED at read (single source of truth), NOT stored —
-- pricing.rs floors PER METRIC before summing, so a stored copy could silently disagree.
CREATE TABLE zeroship.invoice_lines (
    invoice_id             TEXT   NOT NULL REFERENCES zeroship.invoices(id) ON DELETE RESTRICT,
    app_id                 UUID   NOT NULL REFERENCES zeroship.apps(id)     ON DELETE RESTRICT,
    included_units         BIGINT NOT NULL CHECK (included_units >= 0),  -- APPLIED quota (frozen)
    fx_pico_cents_per_unit BIGINT NOT NULL CHECK (fx_pico_cents_per_unit >= 1000),  -- RESOLVED FX charged
    base_fee_cents         BIGINT NOT NULL DEFAULT 0 CHECK (base_fee_cents >= 0),
    amount_cents           BIGINT NOT NULL CHECK (amount_cents   >= 0),  -- ChargeBreakdown.total_cents (authoritative)
    usage_snapshot         JSONB  NOT NULL,                              -- {metric: raw_total} (SOURCE OF TRUTH)
    weights_snapshot       JSONB  NOT NULL,                              -- {metric:{units_per_op,per_units}} applied
    created_at             TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (invoice_id, app_id)
);
--rollback DROP TABLE zeroship.invoice_lines;

--changeset zeroship:billing-provider-refs splitStatements:true
-- INVOICE-level provider ids. Real FK → invoices(id): no dangling ref. Core invoices
-- carry no provider ids.
CREATE TABLE zeroship.billing_provider_refs (
    invoice_id  TEXT NOT NULL REFERENCES zeroship.invoices(id) ON DELETE CASCADE,
    provider    TEXT NOT NULL,                -- 'stripe' | 'stripe_meters' | 'openmeter'
    ref_kind    TEXT NOT NULL,                -- 'invoice' | 'draft_invoice'
    external_id TEXT NOT NULL,                -- in_…
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (invoice_id, provider, ref_kind),
    UNIQUE (provider, ref_kind, external_id)
);
--rollback DROP TABLE zeroship.billing_provider_refs;

--changeset zeroship:billing-line-provider-refs splitStatements:true
-- LINE-level provider ids: a REAL COMPOSITE FK → invoice_lines(invoice_id, app_id),
-- replacing an asserted-not-enforced '<inv>:<app>' TEXT object_id. A writer CANNOT
-- insert a line ref for a non-existent (invoice, app) line — the DB rejects it, so the
-- per-app double-bill guard ("an invoice_item ref EXISTS for this line") can never
-- silently fail-to-match a malformed key. Composite PK ⇒ no cross-period collision.
CREATE TABLE zeroship.billing_line_provider_refs (
    invoice_id  TEXT NOT NULL,
    app_id      UUID NOT NULL,
    provider    TEXT NOT NULL,                -- 'stripe' | 'stripe_meters' | 'openmeter'
    ref_kind    TEXT NOT NULL,                -- 'invoice_item'
    external_id TEXT NOT NULL,                -- ii_…
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (invoice_id, app_id, provider, ref_kind),
    UNIQUE (provider, ref_kind, external_id),
    FOREIGN KEY (invoice_id, app_id)
        REFERENCES zeroship.invoice_lines(invoice_id, app_id) ON DELETE CASCADE
);
--rollback DROP TABLE zeroship.billing_line_provider_refs;

--changeset zeroship:invoices-immutable splitStatements:false
-- Once finalized, the only legal change is finalized → void. draft→finalized is
-- IMPLICITLY allowed. The finalize UPDATE writes subtotal/credit/tax/total in ONE
-- statement, so the balance CHECK never sees a half-written row.
CREATE FUNCTION zeroship.invoices_immutable() RETURNS trigger AS $fn$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'invoices are append-only (no DELETE)';
    END IF;
    IF OLD.status = 'finalized' THEN
        IF NEW.status = 'void'
           AND NEW.id = OLD.id AND NEW.creator_id = OLD.creator_id
           AND NEW.period = OLD.period
           AND NEW.subtotal_cents = OLD.subtotal_cents
           AND NEW.credit_cents = OLD.credit_cents
           AND NEW.tax_cents = OLD.tax_cents
           AND NEW.total_cents = OLD.total_cents THEN
            RETURN NEW;
        END IF;
        RAISE EXCEPTION 'invoice % is finalized — only the void transition is permitted', OLD.id;
    END IF;
    RETURN NEW;
END;
$fn$ LANGUAGE plpgsql;
CREATE TRIGGER invoices_immutable_trg
    BEFORE UPDATE OR DELETE ON zeroship.invoices
    FOR EACH ROW EXECUTE FUNCTION zeroship.invoices_immutable();
--rollback DROP TRIGGER IF EXISTS invoices_immutable_trg ON zeroship.invoices;
--rollback DROP FUNCTION IF EXISTS zeroship.invoices_immutable();

--changeset zeroship:invoice-lines-immutable splitStatements:false
-- The reproducibility record lives on the LINES. Freeze a line whenever its PARENT
-- invoice is finalized: reject UPDATE/DELETE. While the parent is draft, lines stay
-- mutable (the reconciler builds them).
-- CONCURRENCY: line mutability is SAFE because the per-creator reconcile is SINGLE-
-- FLIGHTED by an advisory lock — the same creator's lines and finalize never run
-- concurrently. A future NON-cron line writer MUST take the same per-creator advisory
-- lock or SELECT … FOR UPDATE the parent invoice first. The trigger is the backstop;
-- the advisory lock is the ordering guarantee.
CREATE FUNCTION zeroship.invoice_lines_immutable() RETURNS trigger AS $fn$
DECLARE parent_status zeroship.invoice_status;
BEGIN
    SELECT status INTO parent_status FROM zeroship.invoices
        WHERE id = COALESCE(OLD.invoice_id, NEW.invoice_id);
    IF parent_status = 'finalized' THEN
        RAISE EXCEPTION 'invoice_line for invoice % is frozen — parent invoice is finalized',
            COALESCE(OLD.invoice_id, NEW.invoice_id);
    END IF;
    RETURN COALESCE(NEW, OLD);
END;
$fn$ LANGUAGE plpgsql;
CREATE TRIGGER invoice_lines_immutable_trg
    BEFORE UPDATE OR DELETE ON zeroship.invoice_lines
    FOR EACH ROW EXECUTE FUNCTION zeroship.invoice_lines_immutable();
--rollback DROP TRIGGER IF EXISTS invoice_lines_immutable_trg ON zeroship.invoice_lines;
--rollback DROP FUNCTION IF EXISTS zeroship.invoice_lines_immutable();

--changeset zeroship:invoice-and-creator-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.creator_billing            TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.billing_customer_refs      TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE         ON zeroship.invoices                   TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE         ON zeroship.invoice_lines              TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT                 ON zeroship.billing_provider_refs      TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT                 ON zeroship.billing_line_provider_refs TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.creator_billing FROM zeroship_control'; EXECUTE 'REVOKE ALL ON zeroship.billing_customer_refs FROM zeroship_control'; EXECUTE 'REVOKE ALL ON zeroship.invoices FROM zeroship_control'; EXECUTE 'REVOKE ALL ON zeroship.invoice_lines FROM zeroship_control'; EXECUTE 'REVOKE ALL ON zeroship.billing_provider_refs FROM zeroship_control'; EXECUTE 'REVOKE ALL ON zeroship.billing_line_provider_refs FROM zeroship_control'; END IF; END $rb$;
```

### `0043` — `metering_exports.sql`

```sql
--liquibase formatted sql

--changeset zeroship:metering-exports splitStatements:true
-- Export-only; NEVER feeds enforcement (the spend cap reads usage_aggregates directly —
-- keeping providers pluggable). Cumulative high-water + failure surface kept verbatim.
CREATE TABLE zeroship.metering_exports (
    app_id               UUID                    NOT NULL REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    period               zeroship.billing_period NOT NULL,
    exported_units       BIGINT                  NOT NULL DEFAULT 0 CHECK (exported_units >= 0),  -- cumulative CU (monotonic)
    consecutive_failures INTEGER                 NOT NULL DEFAULT 0 CHECK (consecutive_failures >= 0),
    last_error           TEXT,                                                                    -- redacted at source
    last_attempt_at      TIMESTAMPTZ,
    updated_at           TIMESTAMPTZ             NOT NULL DEFAULT NOW(),
    PRIMARY KEY (app_id, period)
);
--rollback DROP TABLE zeroship.metering_exports;

--changeset zeroship:metering-exports-rls splitStatements:true
ALTER TABLE zeroship.metering_exports ENABLE ROW LEVEL SECURITY;
ALTER TABLE zeroship.metering_exports FORCE  ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON zeroship.metering_exports
    USING (app_id = current_setting('zeroship.tenant_app', true)::uuid);
--rollback DROP POLICY IF EXISTS tenant_isolation ON zeroship.metering_exports;
--rollback ALTER TABLE zeroship.metering_exports NO FORCE ROW LEVEL SECURITY;
--rollback ALTER TABLE zeroship.metering_exports DISABLE ROW LEVEL SECURITY;

--changeset zeroship:metering-exports-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.metering_exports TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.metering_exports FROM zeroship_control'; END IF; END $rb$;
```

> **Retention cron (pre-staged, DISABLED, wired in Rust — no inline DELETE SQL here):**
> The `cron/billing_retention` stub (mirroring `audit_retention`) enforces, when activated:
> - **`usage_reports_seen`** — the activation PR FIRST starts writing `period` at ingest + adds the period index, THEN prunes `WHERE period < (now()::date - INTERVAL 'N months')`; pre-activation NULL-period rows are swept by a one-time `seen_at` backfill.
> - **`spend_state_history`** — `WHERE period < cutoff`.
> - **`usage_aggregates`** — prune a `(app_id, period)` row when it is old, not the current period, outside every export reconcile window, AND one of: **(A) billed apps** — the owning-creator invoice for that *exact* period is finalized (join `app → owner → invoices`, so one creator's finalized period can't prune another's still-draft period); **(B) system / owner-less apps** (`apps.system=true`, never billed, highest-volume always-on) — pruned on a pure age gate (`NOT EXISTS owner`), so the busiest apps don't grow unbounded.
> - **`billing_metrics`** — GC `kind='custom'` rows past `last_seen_at` cutoff with no referencing aggregate / line snapshot / weight.
> Range-partition `DROP` is the preferred mechanism at scale.

### `0047` — `account_status_and_webhook_dedup.sql`

```sql
--liquibase formatted sql

--changeset zeroship:creator-billing-status splitStatements:true
-- Account/dunning state. The gateway route-pull LEFT JOINs this onto each app via
-- app_members(owner) to populate RouteEntry.account_state; account_status.rs UPSERTs the
-- payment-failure→past_due→suspension lifecycle; dunning.rs scans it.
-- creator_id FK targets creator_billing(creator_id), NOT users directly, so the FK +
-- erase policy is UNIFORM across the billing cluster. ON DELETE CASCADE — transitively
-- through creator_billing → users — so an anonymize-retained creator keeps this row (FK
-- target alive); a hard-deleted (never-billed) creator's row CASCADEs away cleanly.
-- Creator-keyed ⇒ NO per-app RLS; the gateway projects it per-app via app_members(owner).
CREATE TABLE zeroship.creator_billing_status (
    creator_id              UUID PRIMARY KEY REFERENCES zeroship.creator_billing(creator_id) ON DELETE CASCADE,
    state                   zeroship.account_state NOT NULL DEFAULT 'active',
    past_due_since          TIMESTAMPTZ,                -- set on active→past_due; cleared on recovery
    suspended_at            TIMESTAMPTZ,                -- NULL unless suspended
    last_payment_failure_at TIMESTAMPTZ,                -- audit: most recent failed-payment signal
    failed_invoice_id       TEXT,                       -- audit: WHICH invoice last failed
    -- G2 ORDER-SAFETY (account_status.rs critic #1) — CARRIED FORWARD VERBATIM
    -- from the live 0045. Stripe webhooks reorder/redeliver, so a stale
    -- `payment_failed` can land AFTER an `invoice.paid` recovery. These two
    -- high-water columns are the false-suspend guard and are READ + WRITTEN by
    -- account_status.rs (record_payment_failed / record_payment_recovered):
    --   * last_recovered_at — the Stripe `event.created` of the most recent
    --     RECOVERY (advanced monotonically via GREATEST). A `payment_failed`
    --     whose `event.created <= last_recovered_at` is STALE and is IGNORED —
    --     it MUST NOT re-arm past_due on an already-paying creator.
    --   * last_event_at — the `event.created` of the most recent event applied
    --     (monotonic audit bookkeeping).
    -- DROPPING EITHER COLUMN regresses the G2 order-safe suspension (the live
    -- code would fail with "column does not exist"). Both default NULL.
    last_event_at           TIMESTAMPTZ,
    last_recovered_at       TIMESTAMPTZ,
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE TABLE zeroship.creator_billing_status_history (
    creator_id UUID NOT NULL REFERENCES zeroship.creator_billing(creator_id) ON DELETE CASCADE,
    from_state zeroship.account_state NOT NULL,
    to_state   zeroship.account_state NOT NULL,
    reason     TEXT,                      -- 'payment_failed' | 'dunning_exhausted' | 'payment_recovered'
    at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_creator_billing_status_history_creator_at
    ON zeroship.creator_billing_status_history (creator_id, at DESC);
-- Partial index: the dunning cron scans past_due rows by past_due_since.
CREATE INDEX idx_creator_billing_status_past_due
    ON zeroship.creator_billing_status (past_due_since)
    WHERE state = 'past_due';
--rollback DROP INDEX IF EXISTS zeroship.idx_creator_billing_status_past_due;
--rollback DROP INDEX IF EXISTS zeroship.idx_creator_billing_status_history_creator_at;
--rollback DROP TABLE zeroship.creator_billing_status_history;
--rollback DROP TABLE zeroship.creator_billing_status;

--changeset zeroship:creator-billing-status-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.creator_billing_status         TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT         ON zeroship.creator_billing_status_history TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.creator_billing_status FROM zeroship_control'; EXECUTE 'REVOKE ALL ON zeroship.creator_billing_status_history FROM zeroship_control'; END IF; END $rb$;

--changeset zeroship:stripe-events-seen splitStatements:true
-- G6 WEBHOOK REPLAY-DEDUP — CARRIED FORWARD VERBATIM from the live 0046.
-- Stripe delivers webhooks AT-LEAST-ONCE; this is the GENERAL dedup ledger so
-- each verified event processes AT-MOST-ONCE. Written CLAIM-AFTER-SUCCESS by the
-- webhook dispatcher (stripe_handlers.rs:582 event_processed / :603
-- mark_event_processed → stripe_store.rs:453/472), so a handler that errored is
-- NOT recorded and Stripe's retry re-processes it (exactly-once EFFECTIVE).
-- Mirrors usage_reports_seen: control-internal, NOT app-keyed (keyed by the
-- Stripe evt_… id) ⇒ NO RLS; append-only ⇒ no DELETE/UPDATE grant.
CREATE TABLE zeroship.stripe_events_seen (
    event_id   TEXT        PRIMARY KEY,       -- evt_… ; the dedup key
    event_type TEXT        NOT NULL,
    seen_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
--rollback DROP TABLE zeroship.stripe_events_seen;

--changeset zeroship:stripe-events-seen-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT ON zeroship.stripe_events_seen TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.stripe_events_seen FROM zeroship_control'; END IF; END $rb$;
```

---

## Table-by-table rationale

- **`billing_metrics` (new).** The FK spine that ends silent-`$0` billing (critique #3). Every `metric` reference FKs it. Platform/primitive rows are seeded; custom SDK metrics auto-register on first ingest but are **capped per-app** (refuse-with-warn past the cap — at-least-once ingest must never wedge), **GC'd** by `last_seen_at`, and **app-attributed** (`owner_app`, CASCADE) so attacker-influenced `usage.custom` keys can't cardinality-DoS the global catalog or leak rows.
- **`metric_weights` + `pricing_config`.** CU cost model + single global FX default. Un-versioned because snapshot-onto-line already makes finalized bills reproducible — versioning here would be cost without benefit. FX floored at `>= 1000` pico-cents (`MIN_FX_PICO_CENTS_PER_UNIT`) to avoid rounding-to-zero. A **migration-time parity assertion** fails the build if a billable metric lacks a weight.
- **`usage_aggregates`.** The local, fast, provider-independent enforcement fact (critique constraint: spend enforcement can't be outsourced). `period DATE` (critique #2), `metric` FK'd (critique #3), single-row-per-`(app, period, metric)` kept (no measured contention pre-launch), plus a `(period, app_id)` index because the fleet sweep scans the current period.
- **`usage_reports_seen`.** Idempotent dedup ledger. PK stays `(worker_id, sequence)` — **never** includes `period`, or a month-boundary retry would double-apply. `period` column is present-but-inert until retention activates.
- **`plans`.** Priced identity; `runtime_limits_json` kept (the route-pull needs it) but kept off price semantics (critique #5). `assignable_by_creator` inlined; sole `apps_plan_fk` added.
- **`app_spend_limit` + `app_spend_state` + `spend_state_history` (split).** Config override split from derived hot state (critique #6) — ends the three-writer clobber dance where `touch_state`/`persist_transition`/`set_limit` each tiptoed around columns they didn't own. Typed `spend_state` domain; `period DATE`.
- **`creator_billing`.** Identity only. `stripe_customer_id` relocated to `billing_customer_refs`. Stays `ON DELETE CASCADE` from `users` (uniform with the existing reaper; the erase tombstone is `users.anonymized_at`).
- **`billing_customer_refs` / `billing_provider_refs` / `billing_line_provider_refs` (new).** The provider-agnostic seam (critique #4). Customer, invoice, and line provider ids each live in a narrow real-FK side table — the line-ref table uses a **real composite FK** to `invoice_lines`, so a malformed line key is rejected at write time and the double-bill guard cannot silently fail-to-match.
- **`invoices` (replaces `billing_runs`).** Provider-agnostic; `invoice_status` domain; balance CHECK; immutability trigger. `UNIQUE (creator_id, period)` is the no-double-bill claim; `status='finalized'` is the crash-safe short-circuit.
- **`invoice_lines` (replaces `billing_run_items`).** The reproducibility record (critique #1). `app_id` FK'd at last. Freezes the charge *inputs* (`usage_snapshot`, `weights_snapshot`, `included_units`, `fx_pico_cents_per_unit`, `base_fee_cents`); `total_units`/`billable_units` are derived on read to avoid drift.
- **`metering_exports`.** Export high-water + failure surface, `period DATE`. Never feeds enforcement — the spend cap reads `usage_aggregates` directly, which is what keeps providers pluggable.
- **`creator_billing_status` + `_history` (0045/G2).** Account/dunning state, re-keyed onto `creator_billing` for a uniform FK/erase policy; typed `account_state`; partial index for the dunning scan. The G2 order-safety high-water columns `last_event_at`/`last_recovered_at` are **carried forward verbatim** — they are the stale-failure gate read+written by `account_status.rs`; dropping either regresses the G2 order-safe suspension.
- **`stripe_events_seen` (0046/G6, kept).** The Stripe webhook replay-dedup ledger — one row per verified `evt_…`, written claim-after-success so a handler that errored is re-processed (exactly-once EFFECTIVE). Carried forward unchanged: control-internal, NOT app-keyed (keyed by event-id) ⇒ no RLS; append-only ⇒ no DELETE/UPDATE grant. Without it, every redelivered webhook re-runs its side effects.

---

## OLD → NEW mapping

| Old (0037–0043, 0045, 0046) | New | Action | Key changes |
|---|---|---|---|
| `usage_aggregates` | `usage_aggregates` | reshape | `period DATE`; `metric` FK → `billing_metrics`; period-leading index. |
| `usage_reports_seen` | `usage_reports_seen` | reshape | PK stays `(worker_id, sequence)`; `period` NULLable, retention-only, NOT written at ingest. |
| `plans` | `plans` | reshape | priced identity; `runtime_limits_json` kept off price semantics; `assignable_by_creator` inlined. |
| `metric_weights` | `metric_weights` | reshape | `metric` FK → `billing_metrics`; un-versioned; seed/weight parity asserted at migration. |
| `pricing_config` | `pricing_config` | keep | single-row global FX default; floored. |
| — | `billing_metrics` | **new** | metric catalog / FK spine; custom rows capped + GC'd + app-attributed. |
| `app_spend_state` | `app_spend_limit` **+** `app_spend_state` | **split** | config override split from derived state. |
| `spend_state_history` | `spend_state_history` | reshape | typed states; `period DATE`. |
| `creator_billing` | `creator_billing` | reshape | `stripe_customer_id` → `billing_customer_refs`; stays CASCADE from users. |
| `billing_runs` | `invoices` | replace | provider-agnostic; status domain; balance CHECK; immutability trigger; Stripe ids → side table. |
| `billing_run_items` | `invoice_lines` | replace | FK `app_id`; frozen snapshot incl. `included_units`; derived `total_units`; immutability trigger. |
| `metering_exports` | `metering_exports` | reshape | `period DATE`; failure surface kept. |
| `creator_billing_status` (0045/G2) | `creator_billing_status` | reshape | FK → `creator_billing(creator_id)` CASCADE; typed `account_state`; **`last_event_at`/`last_recovered_at` order-safety columns CARRIED FORWARD verbatim** (dropping either regresses G2). |
| `creator_billing_status_history` (0045/G2) | `creator_billing_status_history` | reshape | typed states; FK → `creator_billing(creator_id)` CASCADE; append-only. |
| `stripe_events_seen` (0046/G6) | `stripe_events_seen` | **keep** | webhook replay-dedup ledger carried forward verbatim; control-internal, NO RLS, append-only (event_processed / mark_event_processed). |
| — | `billing_customer_refs` | **new** | customer↔provider, real FK → `creator_billing`; `UNIQUE(external_id)` backs the providerless reverse lookup. |
| — | `billing_provider_refs` | **new** | invoice provider seam, real FK → `invoices(id)`. |
| — | `billing_line_provider_refs` | **new** | line provider seam, real composite FK → `invoice_lines(invoice_id, app_id)`. |

**14 live billing tables** (`usage_aggregates`, `usage_reports_seen`, `plans`, `metric_weights`, `pricing_config`, `app_spend_state`, `spend_state_history`, `creator_billing`, `billing_runs`, `billing_run_items`, `metering_exports`, `creator_billing_status`, `creator_billing_status_history`, `stripe_events_seen`) → **19 new tables**:
- **12 reshaped/kept** (1:1): `usage_aggregates`, `usage_reports_seen`, `plans`, `metric_weights`, `pricing_config`, `app_spend_state`, `spend_state_history`, `creator_billing`, `metering_exports`, `creator_billing_status`, `creator_billing_status_history`, `stripe_events_seen`.
- **2 replacements**: `billing_runs` → `invoices`, `billing_run_items` → `invoice_lines`.
- **5 net-new**: `billing_metrics`, `app_spend_limit` (split from `app_spend_state`), `billing_customer_refs`, `billing_provider_refs`, `billing_line_provider_refs`.

Net **19 tables + 2 immutability triggers + 5 domains** (`billing_period`, `spend_state`, `account_state`, `metric_kind`, `invoice_status`).

---

## Key flows

**A. Idempotent at-least-once ingest** (`metering/mod.rs`, one tx per report). `period = first-of-month(now)::date`. The dedup gate is **unchanged**: `INSERT … usage_reports_seen (worker_id, sequence) VALUES ($1,$2) ON CONFLICT (worker_id, sequence) DO NOTHING RETURNING sequence` — 0 rows ⇒ duplicate ⇒ no-op, including across a month boundary; `period` is **not** written. On a new report, **inside the SAME report transaction and BEFORE the aggregate UPSERT** (so the `usage_aggregates.metric` → `billing_metrics(metric) ON DELETE RESTRICT` FK always resolves): one capped, set-valued custom-metric registration into `billing_metrics`, then per `(app_id, metric, delta)` an UPSERT `(app_id, period, metric) total = total + delta`.

  **Cap-refuse MUST NOT wedge ingest (FK-abort guard).** The custom-metric catalog is capped per-`owner_app` to bound attacker cardinality. When a report carries a *new* custom metric that would exceed the cap, the registration step does NOT insert the catalog row — but then the aggregate UPSERT for that metric would FK-violate and abort the WHOLE report tx (and, because the dedup row already committed-or-rolls-back with it, retries can never make progress). So the ingest path **filters the delta set to metrics that resolved in the catalog**: a metric refused at the cap is **dropped-with-warn** (`billing_event="custom_metric_cap_refused"`), its delta is NOT applied, and the rest of the report commits normally. At-least-once liveness is preserved — a refused metric never FK-aborts the report, and the platform/primitive metrics (always cataloged) always apply. The registration itself is an `INSERT … ON CONFLICT (metric) DO UPDATE SET last_seen_at = NOW()` so an already-known custom metric simply bumps its GC clock.

  `high_water_sequence` stays cross-period (`MAX(sequence) WHERE worker_id=$1`, never `AND period=$2`). The u64→i64 overflow skip-with-warn stays. No double-count regression.

**B. Local spend enforcement** (`spend.rs`, ~60s). Hoist plans/weights/FX once; run the active unweighted-metric check against the loaded `weights` map (no extra query). One batched fleet read `WHERE period=$1` (hits `usage_aggregates_period_idx`). Per app: effective limit = `app_spend_limit.spend_limit_cents` else `plan.spend_limit_default_cents` (double LEFT JOIN); `derive_state` with hysteresis; on transition, one tx UPSERTs `app_spend_state` (no override column to clobber) and appends `spend_state_history` **with `period` bound** (the column is NOT NULL). The gateway reads precomputed `app_spend_state.state` via the 5s route-pull. `GET /spend-limit` (`get_spend_limit`) is rewritten to the double LEFT JOIN reading `l.spend_limit_cents` + `s.state`.

**C. Reproducible month-end reconcile** (`billing_reconcile.rs`, per-creator advisory-locked). Claim with `INSERT … invoices (…, 'draft') ON CONFLICT (creator_id, period) DO NOTHING`; short-circuit when `status='finalized'`. Per app: compute the `ChargeBreakdown`, then **snapshot the inputs onto the line** (`usage_snapshot`, `weights_snapshot`, `included_units`, resolved FX, base fee, authoritative `amount_cents`) before the provider POST. Provider success → INSERT `billing_line_provider_refs(invoice_id, app_id, 'stripe', 'invoice_item', ii_…)`. **Finalize in ONE UPDATE** (subtotal/credit/tax/total/status/finalized_at) so the balance CHECK never sees a half-written row; the line trigger then freezes lines. Any finalized line replays bit-for-bit by re-running `charge_cents` over its snapshot — fixing the retroactive-reprice flaw (critique #1).

  **Snapshot-completeness argument (why replay is bit-for-bit).** `charge_cents(price, usage, weights)` (`pricing.rs:321`) is a pure function of exactly three inputs: `usage` (metric→raw total), `weights` (metric→`{units_per_op, per_units}`), and `price.{base_fee_cents, included_units, fx_pico_cents_per_unit}` — `price.spend_limit_default_cents` is a *cap*, not a charge input, and is correctly absent. The line freezes all five charge inputs (`usage_snapshot`, `weights_snapshot`, `base_fee_cents`, `included_units`, `fx_pico_cents_per_unit`) and the authoritative output (`amount_cents`). Since the function is deterministic and reads nothing else (the half-up `÷ FX_SCALE` rounding is a constant), re-running it over the frozen snapshot reproduces `amount_cents` exactly — no live rate-table read participates. This is the *complete* input set; that is why versioned rate cards are unnecessary. **Base fee is per-app-per-line** (each line carries the owning app's plan `base_fee_cents`, matching `bill_creator`'s per-app `charge_cents`), NOT a creator-level fee summed across lines — so N apps yield N base fees by design, never a double-count.

**D. Crash / retry / >24h idempotency.** The claim short-circuit `invoices.status='finalized'` == old `stripe_invoice_id NOT NULL`. Per-app guard: an `invoice_lines (invoice_id, app_id)` intent row exists before the POST; **presence of a `billing_line_provider_refs` row (real composite FK) == old `stripe_item_id IS NOT NULL`** ⇒ skip; intent present + ref absent ⇒ within 24h, deterministic-key replay; past 24h, `find_invoice_item_by_key` adopt-or-post then INSERT the ref. Draft-before-finalize is guarded by the `draft_invoice` `billing_provider_refs` row. The Native provider's `invoice` verb (`native.rs::lookup_invoice_id`) is rewritten to resolve the finalized id via `invoices ⋈ billing_provider_refs (provider='stripe', ref_kind='invoice', status='finalized')`, with `period_start i64 → period DATE`. The ref rows are durable real-FK guards — no malformed-key gap.

**E. Export (pluggable providers)** (`metering_export.rs`). Reads `usage_aggregates`, derives the app's *current cumulative* CU via `pricing::total_units(weights, period_totals)` (no CU column — always re-derived from the re-weightable raw totals), then pushes the **DELTA** `current_total_units − exported_units` to the configured provider (Native / Stripe Billing Meters / OpenMeter); a 0 delta is the no-op that makes a re-run safe. On a successful push it advances the durable high-water `exported_units = current_total_units` in `metering_exports` (monotonic), recording the failure surface (`consecutive_failures`/`last_error`/`last_attempt_at`) on failure. The local high-water — NOT Stripe's dedup window — is the primary double-push guard. The customer id comes from `billing_customer_refs`. **Provider→ref mapping (avoid a redundant ref):** Stripe Billing Meters (`stripe_meters`) posts meter events against the SAME platform Stripe customer the Native/invoice rail uses (`stripe_meters.rs:83` calls `get_customer` → the `cus_…`), so it reads the **`provider='stripe'`** ref — it does NOT mint a separate `'stripe_meters'` customer ref. Only a backend with a *distinct external customer handle* (OpenMeter) writes a non-`'stripe'` ref (`provider='openmeter'`). So `billing_customer_refs` holds at most one `'stripe'` row (shared by Native + Stripe-Meters) plus, if OpenMeter is configured, one `'openmeter'` row per creator. **Export never feeds enforcement** — that read stays local on `usage_aggregates` — which is exactly what lets the backend be swapped without touching the cap path.

**F. Account erase** (`auth::cron::account_reaper`). Conforms to the existing anonymize-vs-hard-delete branch. `user_has_financial_history` is widened to `creator_accounts OR invoices` (an invoice is the durable artifact GDPR 17(3)(b) lets us retain). Financial history ⇒ `anonymize_user` retains the `users` row, so `creator_billing`, `creator_billing_status`(+history), `invoices`, `invoice_lines`, `billing_customer_refs` all survive (FK targets alive). No financial history ⇒ `hard_delete_user`, and CASCADE reaps the empty billing shell. No RESTRICT flip, no bespoke billing-side anonymize, no dangling suspension state — uniform.

---

## Implementation plan

### Schema (this proposal)
Replace the live billing changesets (`0037`–`0043`, `0045`, `0046`) with the consolidated files above (`0037`–`0043` re-authored in place; account-status + webhook-dedup in `0047`). FK precedence is satisfied by file order (domains/catalog → cost model → metering → plans → spend → invoicing → exports → account-status + webhook-dedup). The platform/primitive metric seed lives in `0037`, so the `usage_aggregates.metric` / `metric_weights.metric` FKs resolve at first boot regardless of `seed_plans` timing; `0047`'s `creator_billing_status` FK targets `creator_billing(creator_id)` (created in `0042`), so it correctly follows by number, and `0047`'s `stripe_events_seen` is FK-free (control-internal), so it has no ordering constraint.

### Code changes (each lands with a regression test that fails pre-fix)
1. **`metering/mod.rs`** — bind `period DATE` directly (drop `period_ts`/the `f64` idiom); ingest UPSERT keys `(app_id, period, metric)`; dedup INSERT unchanged; add the capped set-valued custom-metric registration; keep the u64→i64 overflow skip.
2. **`spend.rs`** — `set_limit` upserts `app_spend_limit` only (drop its `period_ts` bind); `app_state_row` double LEFT JOIN (`apps ⋈ app_spend_limit ⋈ app_spend_state`); `touch_state`/`persist_transition` UPSERT `app_spend_state` with no `spend_limit_cents`; `persist_transition` history INSERT **binds `period`**. NOTE: `spend_state_history.period` is `NOT NULL` in the new schema while the live `persist_transition` (`spend.rs:444`) does not bind it — so this code change and the `0041` DDL MUST land in the **same PR**, or the first transition INSERT fails the NOT NULL. Also add the active unweighted-metric warn (a metric present in `usage_aggregates` with no `metric_weights` row prices $0 silently → warn).
3. **`api.rs::get_spend_limit`** — rewrite to the double LEFT JOIN (`l.spend_limit_cents` + `s.state`); the `override ?? plan_default` logic unchanged. Regression: `GET /spend-limit` returns the override after `set_limit`.
4. **`stripe_store.rs`** — `set_customer` becomes a 2-statement tx (identity `creator_billing` then `billing_customer_refs(creator_id, 'stripe', cus_…)`); `set_default_pm` unchanged (the 2nd lazy identity writer, FK-parent-safe — but it must now INSERT the `creator_billing` parent before/instead-of writing a customer ref, since the customer id moved); `get_customer(creator_id)` reads `billing_customer_refs WHERE creator_id=$1 AND provider='stripe'`; `get_creator_by_customer(cus)` reads `billing_customer_refs WHERE external_id=$1` (the caller `stripe_handlers.rs:874` has NO provider in hand — the `UNIQUE(external_id)` constraint makes this providerless probe guaranteed-singular). Regression: round-trip `set_customer` then `get_creator_by_customer` resolves the creator; the customer id is absent from `creator_billing` (fully relocated).
5. **`billing_reconcile.rs`** — claim/short-circuit on `invoices.status`; snapshot-onto-line before POST; line provider-ref INSERT; **finalize-in-one-statement**. Regression: a two-statement subtotal-then-total write is rejected; re-running `charge_cents` over a finalized snapshot reproduces `amount_cents`.
6. **`metering/provider/native.rs::lookup_invoice_id`** — rewrite to `invoices ⋈ billing_provider_refs`; `period_start i64 → period DATE`. Regression: the Native `invoice` verb returns the finalized id post-relocation.
7. **`account_status.rs`** — payment-failure UPSERT becomes parent-first (`creator_billing` then `creator_billing_status`, since the FK now targets `creator_billing(creator_id)` not `users`); `state` literal → `account_state` domain (same wire TEXT). The G2 order-safety columns `last_event_at`/`last_recovered_at` are READ+WRITTEN unchanged (carried forward in the schema) — the `record_payment_failed` stale-failure gate (`event.created <= last_recovered_at` ⇒ ignore) and the `record_payment_recovered` `GREATEST(...)` high-water advance keep working verbatim. Regression: (a) a payment-failure UPSERT for a creator with no prior `creator_billing` row succeeds (parent-first); (b) a stale `payment_failed` (event.created older than `last_recovered_at`) does NOT re-arm `past_due` (proves the carried-forward columns still gate).
   - **G6 dispatcher: no code change.** `stripe_events_seen` is carried forward verbatim, so `event_processed`/`mark_event_processed` (`stripe_store.rs:453/472`) and the webhook dispatcher (`stripe_handlers.rs:582/603`) compile and run unchanged. The schema-level requirement is solely that the table survives the rewrite (it does, in `0047`).
   - **Gateway route-pull: no code change.** The FK-target change (`creator_billing_status.creator_id → creator_billing` instead of `→ users`) is TRANSPARENT to the registry projection: `registry.rs:556` LATERAL-joins `cbs.creator_id = m.user_id` on the *value* (the same user id), not via the FK, so `RouteEntry.account_state` resolves unchanged. Likewise `app_spend_state` is still joined `s.app_id = a.id` (`registry.rs:552`); only the column the engine *writes* moved (the override is now in `app_spend_limit`), not the `state` column the gateway reads.
8. **`account_reaper.rs`** — widen `user_has_financial_history` to `creator_accounts OR invoices`. Regression: a previously-invoiced creator anonymize-retains (rows intact); a never-billed creator hard-deletes (CASCADE, no dangling rows).
9. **Immutability triggers** — regression: updating `invoice_lines.amount_cents` after the parent finalizes is rejected; a finalized invoice rejects any non-void UPDATE.

### Now vs pre-scale priority

**Build now:** all 19 tables + 5 domains + 2 immutability triggers + balance CHECK + `period DATE` everywhere + the metric-catalog FK spine (capped/GC-able/app-attributed custom rows + seed/weight parity assertion) + the active unweighted-metric alert + the spend config/derived split (with `get_spend_limit` reader fixed + `period` history bind) + the real-FK `billing_customer_refs` (with `UNIQUE(external_id)` for the providerless reverse lookup) + the `billing_provider_refs`/`billing_line_provider_refs` seams + snapshot-onto-line (frozen `included_units`, derived `total_units`) + the 2-statement `set_customer` rewrite + the `native.rs::lookup_invoice_id` rewrite + `creator_billing_status` in scope (incl. the G2 `last_event_at`/`last_recovered_at` order-safety columns) + `stripe_events_seen` (G6 webhook-dedup) carried forward + finalize-in-one-statement + the conformed account-erase widening.

**Pre-stage now, activate on a measured signal:**
1. Retention cron (`cron/billing_retention`, disabled) — `usage_reports_seen` (begins writing `period` + adds its index then), `spend_state_history`, `usage_aggregates` (per-owner-finalized **or** system/owner-less age gate, outside reconcile window), custom-metric GC.
2. Native range-partitioning of the growers.

**Defer (documented, not built):**
3. `usage_aggregates` slot=`worker_id` sharding — only when write contention is measured.
4. Effective-dated rate cards — snapshot-onto-line already makes finalized bills reproducible.
5. Credits / proration / tax-line / multi-currency / double-entry GL — `credit_cents` / `tax_cents` / `currency` shapes already present.
