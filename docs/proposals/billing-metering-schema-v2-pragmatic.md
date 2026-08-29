# Billing & Metering Schema v2 — pragmatic-postgres redesign

Status: proposal (pre-launch clean rewrite of changesets 0037–0043, **no live migration**).
Branch: `feat/billing-metering`.
Philosophy: **pragmatic-postgres** — the *minimal* clean redesign that fixes **every** critique
item with the fewest tables. We do NOT build the full effective-dated rate-card / price-version
machinery; instead we get reproducibility the cheap way industry also uses — **snapshot the
resolved price onto the invoice line at finalize time** (Stripe/Metronome/Lago all freeze the
applied rate onto the finalized line). Versioned catalogs, double-entry GL, credits, proration,
tax-line modeling, multi-currency, slotted-counter aggregate sharding, and partitioning are all
explicitly **deferred** — they earn their place post-launch / when Stream-2 Connect lands.

> A sibling proposal, `billing-metering-schema-v2.md`, specifies the *billing-platform-grade*
> variant (fully versioned rate cards + append-only money ledger). This document is the leaner
> alternative; it closes the same six critique items with far fewer tables.

The whole redesign is **13 tables** (was 11), grounded in the real query paths in
`crates/zeroship-control/src/{pricing,pricing_store,plan_catalog,spend,metering,internal}.rs` +
`cron/{billing_reconcile,metering_export,spend_reconcile}.rs` + `registry.rs`. It closes all six
known-critique items:

| # | Critique | Fix in v2 |
|---|---|---|
| 1 | Bills not reproducible (reconciler re-reads weights/FX/plan live → a mid-period edit retroactively reprices the closed month) | **`invoice_lines` snapshots** the resolved `fx_pico_cents_per_unit` + the `ChargeBreakdown` (`total_units`, `billable_units`, `base_fee_cents`, `amount_cents`) + JSONB `usage_snapshot`/`weights_snapshot`. The finalized line is self-contained; a later catalog edit can't restate it. |
| 2 | `period_start TIMESTAMPTZ` bound via f64 | **`period DATE`** (first-of-month domain) everywhere. Exact key, no f64 round-trip, clean `WHERE period = $1`. |
| 3 | Free-text metric (silent $0); free-text spend states | **`billing_metrics` catalog** (FK spine) + **`metric_kind`/`spend_state`/`invoice_status` domains**. Emitted-but-unweighted becomes a queryable left-join gap. |
| 4 | Stripe-shaped core (`stripe_invoice_id`/`draft_invoice_id`/`stripe_item_id`/`stripe_customer_id`) | **`billing_provider_refs` side table**; core carries zero provider ids. The C1/C2 crash-window proof relocates onto the side table faithfully. |
| 5 | `plans.runtime_limits_json` couples billing↔sandbox | `runtime_limits_json` **moves out of the priced model**. `plans` is priced identity only. |
| 6 | No FK on `billing_run_items.app_id`; config+derived mixed in one spend row; money-unit mixing; naming | **FK `app_id`** on lines; **`app_spend_limit` (config) split from `app_spend_state` (derived)**; `*_cents` = integer cents, `fx_pico_cents_per_unit` the only sub-cent column (never `_cents`-named); consistent `period`/`*_at` suffixes; `ON DELETE RESTRICT` on money/audit FKs. |

Hard constraints honored throughout: PG16 via compio-postgres (**standard SQL only, no
extensions**); RLS fail-closed (`ENABLE`+`FORCE`, `current_setting('zeroship.tenant_app',
true)::uuid`, control BYPASSRLS); least-priv grants to `zeroship_control` guarded on role
existence; CU+FX neutral-unit pricing with **no stored CU column** (CU re-derived from raw totals so
a re-weight reprices *future* periods cleanly); pluggable providers (no provider baked into core);
the local current-period aggregate stays the enforcement fact (metering + the aggregate are NOT
outsourced).

---

## OLD → NEW mapping

| Old table (0037–0043) | New table(s) | Disposition |
|---|---|---|
| `usage_aggregates` | `usage_aggregates` | **kept**; `period_start TIMESTAMPTZ` → `period DATE`; `metric` gains FK → `billing_metrics`; add period-leading index |
| `usage_reports_seen` | `usage_reports_seen` | **kept**; `period` added to the dedup key (prunability + restart safety) |
| `plans` | `plans` | **kept**; priced identity; `runtime_limits_json` removed (#5); `assignable_by_creator` inlined |
| `metric_weights` | `metric_weights` | **kept**; `metric` gains FK → `billing_metrics` |
| `pricing_config` | `pricing_config` | **kept** unchanged (single-row global FX default) |
| (none) | `billing_metrics` | **NEW** — metric catalog / FK spine (#3) |
| `app_spend_state` | `app_spend_limit` **+** `app_spend_state` | **SPLIT** config (override) from derived (enforcement state) (#6) |
| `spend_state_history` | `spend_state_history` | **kept**; states → `spend_state` domain; `period` added |
| `creator_billing` | `creator_billing` | **kept**; `stripe_customer_id` **removed** → side table (#4); FK → RESTRICT |
| `billing_runs` | `invoices` | **renamed+reshaped** — provider-agnostic; `status` domain; `period DATE`; Stripe ids → side table |
| `billing_run_items` | `invoice_lines` | **renamed+reshaped** — FK `app_id`; **snapshot columns** (#1); Stripe id → side table |
| `metering_exports` | `metering_exports` | **kept**; `period DATE`; failure-surface columns kept |
| (none) | `billing_provider_refs` | **NEW** — provider-ref side table (#4) |

Net 11 → 13: +`billing_metrics`, +`billing_provider_refs`, +`app_spend_limit` (spend split), the two
renames net 0. No table is merged.

---

## Shared building blocks (domains)

Domains over `CHECK` (not native `ENUM`) deliberately: they keep the **TEXT wire form** the
bespoke compio-postgres driver already round-trips (spend state is read as `Option<String>` and
parsed by `parse_spend_state` in Rust; `metric_kind`/`invoice_status` likewise read as `String`),
while still rejecting an out-of-set value at the source. Native `ENUM` would force an enum codec
into the driver for zero added safety here. The Rust-side fail-closed parse stays as defense in
depth.

```sql
--liquibase formatted sql
--changeset zeroship:billing-domains splitStatements:true
-- Calendar-month period as a DATE pinned to the first of the month (critique #2).
-- The app layer always binds first-of-month-UTC; the CHECK makes a mid-month date
-- unrepresentable, killing the "is this exactly the period key?" f64 fragility.
CREATE DOMAIN zeroship.billing_period AS DATE
    CHECK (EXTRACT(DAY FROM VALUE) = 1);

-- The four spend-enforcement states; ordering allow<warn<degrade<block matches
-- the code's severity().
CREATE DOMAIN zeroship.spend_state AS TEXT
    CHECK (VALUE IN ('allow','warn','degrade','block'));

-- Metric provenance: 'platform' = fixed counters, 'primitive' = db/kv/storage
-- weights, 'custom' = SDK-defined open-ended names.
CREATE DOMAIN zeroship.metric_kind AS TEXT
    CHECK (VALUE IN ('platform','primitive','custom'));

-- Invoice lifecycle: draft → finalized → void. The period-claim short-circuit
-- reads this (status='finalized' ⇒ skip), replacing "stripe_invoice_id NOT NULL".
CREATE DOMAIN zeroship.invoice_status AS TEXT
    CHECK (VALUE IN ('draft','finalized','void'));
--rollback DROP DOMAIN IF EXISTS zeroship.invoice_status;
--rollback DROP DOMAIN IF EXISTS zeroship.metric_kind;
--rollback DROP DOMAIN IF EXISTS zeroship.spend_state;
--rollback DROP DOMAIN IF EXISTS zeroship.billing_period;
```

---

## 1. `billing_metrics` — metric catalog / FK spine (NEW; critique #3)

The closed set of platform/primitive metrics is cataloged; `metric_weights.metric` and
`usage_aggregates.metric` FK into it. An emitted metric with no catalog row (or no weight) becomes
a *queryable* left-join gap, not a silent $0. Custom SDK metrics are open-ended, so ingest
auto-registers them on first sight (`ON CONFLICT DO NOTHING` inside the ingest tx) — that keeps the
FK satisfiable without pre-registering every creator metric.

```sql
--liquibase formatted sql
-- Global operator catalog (mirror plans/metric_weights): NOT tenant-scoped, NO RLS
-- (control is BYPASSRLS). Platform/primitive rows seeded; custom auto-registered.
--changeset zeroship:billing-metrics splitStatements:true
CREATE TABLE zeroship.billing_metrics (
    metric     TEXT PRIMARY KEY,                  -- 'requests','cpu_us',…, custom names
    kind       zeroship.metric_kind NOT NULL,
    unit       TEXT NOT NULL,                     -- 'request','microsecond','byte' (display/audit)
    archived   BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
--rollback DROP TABLE zeroship.billing_metrics;

--changeset zeroship:billing-metrics-seed splitStatements:true
-- Mirrors the 0041 metric_weights seed names exactly, so every billable weight has
-- a catalog parent and every cataloged billable metric has a weight.
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
-- Control seeds + auto-registers custom metrics ⇒ SELECT, INSERT; UPDATE for
-- archive/unit edits. No DELETE — a referenced metric is archived, never deleted.
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.billing_metrics TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.billing_metrics FROM zeroship_control'; END IF; END $rb$;
```

The alertable gap query: `SELECT m.metric FROM billing_metrics m LEFT JOIN metric_weights w USING
(metric) WHERE w.metric IS NULL AND m.kind <> 'custom'`. The runtime keeps the resilient "unweighted
⇒ free" semantic, but the silent-$0 is now visible.

---

## 2. `usage_aggregates` — local current-period totals (kept; `period DATE`; FK metric)

The single-row-per-`(app, period, metric)` shape is **kept** — not slotted-counter sharded. The
research brief's slot=worker_id sharding is the correct *future* answer to write contention, but
pre-launch there's no measured contention and sharding doubles the read into `SUM…GROUP BY` and
forks the source of truth; we defer it. Changes: `period DATE` (#2), `metric` FK → catalog (#3),
and a **period-leading index** for the fleet sweep (#6). This stays the **local, fast,
provider-independent** fact the spend cron reads — the coupling the constraints demand.

```sql
--liquibase formatted sql
--changeset zeroship:metering-usage-aggregates splitStatements:true
CREATE TABLE zeroship.usage_aggregates (
    app_id     UUID                    NOT NULL REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    period     zeroship.billing_period NOT NULL,
    metric     TEXT                    NOT NULL REFERENCES zeroship.billing_metrics(metric),
    total      BIGINT                  NOT NULL DEFAULT 0 CHECK (total >= 0),
    updated_at TIMESTAMPTZ             NOT NULL DEFAULT NOW(),
    PRIMARY KEY (app_id, period, metric)
);
-- The spend/export sweeps read the whole fleet's CURRENT period (WHERE period=$1).
-- The PK leads with app_id, so that predicate can't range-scan the PK. Add the
-- period-leading index the fleet sweep actually uses (critique #6).
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

--changeset zeroship:metering-usage-aggregates-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.usage_aggregates TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.usage_aggregates FROM zeroship_control'; END IF; END $rb$;
```

---

## 3. `usage_reports_seen` — idempotent dedup ledger (kept; `period` added to key)

The `(worker_id, sequence)` dedup PK is correct. We add `period` to the key for two field-standard
reasons: (a) it makes the ledger **prunable by period** later, and (b) it closes the
worker-restart/sequence-reset collision the brief flagged. Worker-keyed control-internal
bookkeeping ⇒ **no app RLS** (as today).

```sql
--liquibase formatted sql
--changeset zeroship:metering-usage-reports-seen splitStatements:true
CREATE TABLE zeroship.usage_reports_seen (
    worker_id TEXT                    NOT NULL,
    period    zeroship.billing_period NOT NULL,
    sequence  BIGINT                  NOT NULL,
    seen_at   TIMESTAMPTZ             NOT NULL DEFAULT NOW(),
    PRIMARY KEY (worker_id, period, sequence)
);
--rollback DROP TABLE zeroship.usage_reports_seen;

--changeset zeroship:metering-usage-reports-seen-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT ON zeroship.usage_reports_seen TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.usage_reports_seen FROM zeroship_control'; END IF; END $rb$;
```

> Retention note: `usage_reports_seen` and `spend_state_history` are the unbounded growers. v2 keeps
> them as plain tables (no partitioning — deferred). The `period` key makes a later
> native-declarative-range-partition, or a simple `DELETE WHERE period < now() - interval 'N months'`
> retention cron (reusing the `audit_retention` cron pattern), a no-extension drop-in. Not built now.

---

## 4. `plans` — priced identity (kept; `runtime_limits_json` removed, critique #5)

`plans` keeps the scalar CU price model (`included_units`, `fx_pico_cents_per_unit` NULL ⇒ global
default), the `spend_limit_default_cents`, and the `assignable_by_creator` guardrail (inlined from
0042). **`runtime_limits_json` moves out** — billing must not own sandbox config (it caused the
poison-tolerance bug). The registry route-pull that needs `AppRuntimeLimits` reads it from a
sandbox/runtime config surface keyed by plan or app, not from the priced row. That table is out of
scope for the billing schema; this DDL just *drops* the coupling.

```sql
--liquibase formatted sql
--changeset zeroship:plan-catalog splitStatements:true
CREATE TABLE zeroship.plans (
    id                        TEXT        PRIMARY KEY,          -- pln_<base62>
    name                      TEXT        NOT NULL,
    base_fee_cents            BIGINT      NOT NULL DEFAULT 0 CHECK (base_fee_cents >= 0),
    included_units            BIGINT      NOT NULL DEFAULT 0 CHECK (included_units >= 0),  -- CU free before overage
    fx_pico_cents_per_unit    BIGINT      CHECK (fx_pico_cents_per_unit IS NULL OR fx_pico_cents_per_unit >= 1000),
    spend_limit_default_cents BIGINT      NOT NULL DEFAULT 0 CHECK (spend_limit_default_cents >= 0),
    assignable_by_creator     BOOLEAN     NOT NULL DEFAULT false,
    archived                  BOOLEAN     NOT NULL DEFAULT false,
    created_at                TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at                TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
-- apps.plan_id FK: archived (soft) never hard-deleted while an app references it.
ALTER TABLE zeroship.apps
    ADD CONSTRAINT apps_plan_fk FOREIGN KEY (plan_id) REFERENCES zeroship.plans(id)
        ON DELETE RESTRICT;
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

The `>= 1000` FX floor mirrors `MIN_FX_PICO_CENTS_PER_UNIT` (defense in depth with
`pricing_store::default_fx_pico_cents_per_unit`). `seed_plans()` still flips the public tiers'
`assignable_by_creator` true at boot.

---

## 5. `metric_weights` + `pricing_config` — CU cost model + global FX (kept; FK metric)

Unchanged from 0041 except `metric_weights.metric` gains the catalog FK. CU stays **derived from
raw totals at pricing time** (no CU column), so a re-weight reprices current/future periods cleanly
and the export watermark re-derives correctly. Because we snapshot the resolved price onto the
finalized invoice line (§7), a re-weight does **not** restate an already-finalized invoice — that is
the reproducibility fix, achieved without versioning the weight table.

```sql
--liquibase formatted sql
--changeset zeroship:metric-weights splitStatements:true
CREATE TABLE zeroship.metric_weights (
    metric       TEXT        PRIMARY KEY REFERENCES zeroship.billing_metrics(metric),
    units_per_op BIGINT      NOT NULL CHECK (units_per_op >= 0),  -- CU per per_units ops
    per_units    BIGINT      NOT NULL CHECK (per_units > 0),      -- divisor (sub-unit weights)
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE TABLE zeroship.pricing_config (
    id                     TEXT        PRIMARY KEY DEFAULT 'global' CHECK (id = 'global'),
    fx_pico_cents_per_unit BIGINT      NOT NULL CHECK (fx_pico_cents_per_unit >= 1000),
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
INSERT INTO zeroship.pricing_config (id, fx_pico_cents_per_unit) VALUES
    ('global', 30000000)
ON CONFLICT (id) DO NOTHING;
--rollback DELETE FROM zeroship.pricing_config WHERE id = 'global';
--rollback DELETE FROM zeroship.metric_weights;

--changeset zeroship:compute-unit-pricing-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.metric_weights TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.pricing_config TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.metric_weights FROM zeroship_control'; EXECUTE 'REVOKE ALL ON zeroship.pricing_config FROM zeroship_control'; END IF; END $rb$;
```

> Changeset ordering: the weight seed FK depends on the catalog seed (§1), so the
> `billing-metrics` changelog file must precede the `metric-weights` file in the include list
> (Liquibase runs in include order).

---

## 6. Spend split — `app_spend_limit` (config) + `app_spend_state` (derived) (critique #6)

Today one `app_spend_state` row carries both the creator override (`spend_limit_cents`, written only
by `set_limit`) **and** the derived enforcement state (UPSERTed every ~60s tick). The three writers
(`persist_transition`, `touch_state`, `set_limit`) each carefully avoid clobbering the columns they
don't own — the textbook config-vs-derived smell (verified in `spend.rs`). v2 splits them: the
rarely-written override → `app_spend_limit` (absent row ⇒ plan default); the hot derived row →
`app_spend_state`. The route-pull join (`registry.rs` LEFT JOIN `s.state AS spend_state`) reads only
the derived table — it never needed the override.

```sql
--liquibase formatted sql
-- CONFIG: per-app override, written ONLY by set_limit. Absent ⇒ plan default.
--changeset zeroship:app-spend-limit splitStatements:true
CREATE TABLE zeroship.app_spend_limit (
    app_id            UUID PRIMARY KEY REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    spend_limit_cents BIGINT CHECK (spend_limit_cents IS NULL OR spend_limit_cents >= 0),  -- override; NULL clears
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- DERIVED: the hot enforcement state, UPSERTed every ~60s tick. Read on the
-- registry route-pull (LEFT JOIN … s.state AS spend_state).
CREATE TABLE zeroship.app_spend_state (
    app_id           UUID PRIMARY KEY REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    state            zeroship.spend_state    NOT NULL DEFAULT 'allow',
    spend_cents      BIGINT                  NOT NULL DEFAULT 0 CHECK (spend_cents >= 0),
    eval_limit_cents BIGINT                  NOT NULL DEFAULT 0 CHECK (eval_limit_cents >= 0),  -- effective limit at last derive
    period           zeroship.billing_period NOT NULL,
    evaluated_at     TIMESTAMPTZ             NOT NULL DEFAULT NOW()
);

-- Append-only transition audit. period added (prunability); states are the domain.
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
--rollback DROP INDEX IF EXISTS zeroship.idx_spend_state_history_app_at;
--rollback DROP TABLE zeroship.spend_state_history;
--rollback DROP TABLE zeroship.app_spend_state;
--rollback DROP TABLE zeroship.app_spend_limit;

--changeset zeroship:app-spend-rls splitStatements:true
-- All three app_id-keyed ⇒ the verbatim fail-closed FORCE pattern. The config
-- table gets WITH CHECK too (a future creator-scoped writer); the derived/history
-- tables are control-BYPASSRLS writes, so USING suffices.
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
    EXECUTE 'GRANT SELECT, INSERT         ON zeroship.spend_state_history TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.app_spend_limit FROM zeroship_control'; EXECUTE 'REVOKE ALL ON zeroship.app_spend_state FROM zeroship_control'; EXECUTE 'REVOKE ALL ON zeroship.spend_state_history FROM zeroship_control'; END IF; END $rb$;
```

Code impact: `set_limit` writes only `app_spend_limit`. `app_state_row` reads `apps LEFT JOIN
app_spend_state s LEFT JOIN app_spend_limit l`. `persist_transition`/`touch_state` UPSERT
`app_spend_state` without the override column — the clobber-avoidance dance disappears. Enforcement
semantics unchanged.

---

## 7. Invoice model — `invoices` + `invoice_lines` (renames; snapshot; critiques #1, #4, #6)

`invoices` replaces `billing_runs` as the provider-agnostic per-period claim; `invoice_lines`
replaces `billing_run_items` as the per-app line. Two load-bearing changes:

- **Snapshot (critique #1):** each line freezes the *resolved* `fx_pico_cents_per_unit` + the
  `ChargeBreakdown` fields (`pricing.rs`: `total_units`, `billable_units`, `base_fee_cents`,
  `total_cents`→`amount_cents`) + two JSONB blobs (`usage_snapshot` = raw metric totals priced,
  `weights_snapshot` = weights applied). A later edit to `metric_weights`/`pricing_config`/`plans`
  can't restate a finalized line — belt (inputs frozen) and suspenders (`amount_cents` frozen).
- **Provider refs out (critique #4):** no Stripe ids in core. The claim short-circuit reads
  `invoices.status`; the per-app skip/adopt set reads the presence of the `invoice_item` provider
  ref instead of `stripe_item_id IS NOT NULL`.

`invoice_lines.app_id` gets the **real FK** it lacked (#6), `ON DELETE RESTRICT` (an invoice
outlives the app row — money/audit outlives the tenant).

```sql
--liquibase formatted sql
-- Provider-agnostic per-(creator, period) invoice. UNIQUE(creator_id, period) is
-- the idempotency claim (== billing_runs PK). status carries the short-circuit:
-- finalized ⇒ this period is billed ⇒ skip. Creator-keyed (a USER id) control
-- bookkeeping ⇒ NO app RLS (mirrors creator_billing).
--changeset zeroship:invoices splitStatements:true
CREATE TABLE zeroship.invoices (
    id             TEXT                    PRIMARY KEY,           -- inv_<base62> typed id
    creator_id     UUID                    NOT NULL REFERENCES zeroship.users(id) ON DELETE RESTRICT,
    period         zeroship.billing_period NOT NULL,
    status         zeroship.invoice_status NOT NULL DEFAULT 'draft',
    currency       CHAR(3)                 NOT NULL DEFAULT 'usd',
    subtotal_cents BIGINT                  NOT NULL DEFAULT 0 CHECK (subtotal_cents >= 0),
    total_cents    BIGINT                  NOT NULL DEFAULT 0 CHECK (total_cents >= 0),
    finalized_at   TIMESTAMPTZ,
    created_at     TIMESTAMPTZ             NOT NULL DEFAULT NOW(),
    updated_at     TIMESTAMPTZ             NOT NULL DEFAULT NOW(),
    UNIQUE (creator_id, period)                                  -- the no-double-bill claim
);

-- Per-app line. PK (invoice_id, app_id) == billing_run_items grain. Snapshot
-- columns make the bill reproducible (#1); app_id gets a real FK (#6).
CREATE TABLE zeroship.invoice_lines (
    invoice_id             TEXT   NOT NULL REFERENCES zeroship.invoices(id) ON DELETE CASCADE,
    app_id                 UUID   NOT NULL REFERENCES zeroship.apps(id) ON DELETE RESTRICT,
    total_units            BIGINT NOT NULL CHECK (total_units >= 0),     -- derived CU at bill time
    billable_units         BIGINT NOT NULL CHECK (billable_units >= 0),  -- max(0, total_units - included)
    fx_pico_cents_per_unit BIGINT NOT NULL CHECK (fx_pico_cents_per_unit >= 1000), -- the RESOLVED FX charged
    base_fee_cents         BIGINT NOT NULL DEFAULT 0 CHECK (base_fee_cents >= 0),
    amount_cents           BIGINT NOT NULL CHECK (amount_cents >= 0),    -- ChargeBreakdown.total_cents
    usage_snapshot         JSONB  NOT NULL,                              -- {metric: total} priced
    weights_snapshot       JSONB  NOT NULL,                              -- {metric: {units_per_op, per_units}} applied
    created_at             TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (invoice_id, app_id)
);
--rollback DROP TABLE zeroship.invoice_lines;
--rollback DROP TABLE zeroship.invoices;

--changeset zeroship:invoices-grants splitStatements:false
-- Claim-then-call: INSERT intent, UPDATE status/amounts after the provider call.
-- No DELETE — invoices are voided (status), never hard-deleted.
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.invoices      TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.invoice_lines TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.invoices FROM zeroship_control'; EXECUTE 'REVOKE ALL ON zeroship.invoice_lines FROM zeroship_control'; END IF; END $rb$;
```

`invoices.id` is a `inv_<base62>` typed id (the period claim is the `UNIQUE(creator_id, period)`,
not the PK — this lets a provider ref point at a stable surrogate while the natural idempotency key
stays the composite). Money discipline (#6): every `_cents` column is integer cents; the only
sub-cent value is `fx_pico_cents_per_unit`, named so it can never be mistaken for cents.
`currency`/`subtotal_cents`/`total_cents` are present so the shape is right when credits/tax/Connect
arrive; for now `total_cents = subtotal_cents`. We intentionally do **not** add the immutability
trigger or a `credit` table the brief sketches — "no DELETE" grant + the `status` domain
(draft→finalized→void) are enough pre-launch.

---

## 8. `creator_billing` — creator identity (kept; `stripe_customer_id` removed, critique #4)

Identity only. The `stripe_customer_id` moves to `billing_provider_refs` (§9). Creator-keyed
control bookkeeping ⇒ no app RLS, control BYPASSRLS. FK → RESTRICT (billing identity outlives the
user row).

```sql
--liquibase formatted sql
--changeset zeroship:creator-billing splitStatements:true
CREATE TABLE zeroship.creator_billing (
    creator_id     UUID PRIMARY KEY REFERENCES zeroship.users(id) ON DELETE RESTRICT,
    default_pm_set BOOLEAN NOT NULL DEFAULT false,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
--rollback DROP TABLE zeroship.creator_billing;

--changeset zeroship:creator-billing-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.creator_billing TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.creator_billing FROM zeroship_control'; END IF; END $rb$;
```

---

## 9. `billing_provider_refs` — provider-ref side table (NEW; critique #4)

The single home for every downstream-provider id. Core invoice/line/customer carry none. The native
provider writes `customer` + `invoice`/`draft_invoice` + `invoice_item` rows here (it IS a Stripe
rail underneath); Stripe-Meters/OpenMeter write their own kinds (or none). This makes the core model
provider-agnostic — and it is where the C1/C2 crash-window proof relocates.

```sql
--liquibase formatted sql
--changeset zeroship:billing-provider-refs splitStatements:true
CREATE TABLE zeroship.billing_provider_refs (
    object_type TEXT NOT NULL CHECK (object_type IN ('invoice','invoice_line','customer')),
    object_id   TEXT NOT NULL,                 -- inv_… | app_id::text | creator_id::text
    provider    TEXT NOT NULL,                 -- 'stripe' | 'openmeter' | …
    ref_kind    TEXT NOT NULL,                 -- 'customer'|'invoice'|'draft_invoice'|'invoice_item'
    external_id TEXT NOT NULL,                 -- cus_… | in_… | ii_…
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (object_type, object_id, provider, ref_kind),
    UNIQUE (provider, ref_kind, external_id)   -- one external id ↔ one object/kind (catch a mis-adopt)
);
--rollback DROP TABLE zeroship.billing_provider_refs;

--changeset zeroship:billing-provider-refs-grants splitStatements:false
-- Claim-then-call: the invoice_item ref is INSERTed AFTER the Stripe POST returns
-- the ii_… (the durable "definitely posted" marker). draft_invoice ref is INSERTed
-- before finalize (C2). No DELETE — refs are immutable facts.
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.billing_provider_refs TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.billing_provider_refs FROM zeroship_control'; END IF; END $rb$;
```

Mapping of the four old Stripe-shaped fields:

| Old | New `billing_provider_refs` row `(object_type, object_id, provider, ref_kind, external_id)` |
|---|---|
| `creator_billing.stripe_customer_id` | `('customer', creator_id, 'stripe', 'customer', cus_…)` |
| `billing_runs.stripe_invoice_id` | `('invoice', inv_…, 'stripe', 'invoice', in_…)` |
| `billing_runs.draft_invoice_id` | `('invoice', inv_…, 'stripe', 'draft_invoice', in_…)` |
| `billing_run_items.stripe_item_id` | `('invoice_line', inv_…\|app_id, 'stripe', 'invoice_item', ii_…)` |

Control-internal, keyed by surrogate object ids (not an app tenant key) ⇒ no app RLS, control
BYPASSRLS — same posture as `billing_runs` today.

---

## 10. `metering_exports` — export high-water (kept; `period DATE`)

Unchanged except `period DATE`. Still **export-only, never feeds enforcement** — the spend cap
reads `usage_aggregates` directly. The failure-surface columns (0043 MAJOR M2) are kept verbatim.
This preserves the provider-pluggable separation: the watermark is a provider-sync cursor, not a
billing fact.

```sql
--liquibase formatted sql
--changeset zeroship:metering-exports splitStatements:true
CREATE TABLE zeroship.metering_exports (
    app_id               UUID                    NOT NULL REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    period               zeroship.billing_period NOT NULL,
    exported_units       BIGINT                  NOT NULL DEFAULT 0 CHECK (exported_units >= 0),  -- cumulative CU pushed (monotonic)
    consecutive_failures INTEGER                 NOT NULL DEFAULT 0 CHECK (consecutive_failures >= 0),
    last_error           TEXT,
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

---

## Flow walkthroughs — the load-bearing properties still hold

### A. Idempotent at-least-once ingest dedup

Worker flushes `UsageReport { worker_id, sequence, counters }` every ~10s, at-least-once. Control,
in **one transaction** per report:

1. `period = first-of-month(now)::date`.
2. `INSERT INTO usage_reports_seen (worker_id, period, sequence) … ON CONFLICT DO NOTHING`. 0 rows ⇒
   duplicate ⇒ commit a no-op (no double-count). Key now spans `period` — strictly safer (a sequence
   reset across a month boundary can't collide).
3. Per `(app_id, metric, delta)`: if custom-and-uncataloged, `INSERT INTO billing_metrics (…,
   'custom', …) ON CONFLICT DO NOTHING` (keeps the new FK satisfiable for open-ended SDK metrics),
   then `INSERT INTO usage_aggregates (app_id, period, metric, total) … ON CONFLICT (app_id, period,
   metric) DO UPDATE SET total = usage_aggregates.total + EXCLUDED.total`.

The dedup gate precedes the aggregate write in the same tx, so a crash mid-apply can't mark a report
seen-but-not-applied or vice versa. **Preserved.**

### B. Spend enforcement reading the LOCAL current-period aggregate

The ~60s `spend_reconcile` cron is the only reader of `usage_aggregates` for enforcement (the
gateway reads the *precomputed* `app_spend_state.state` off the 5s route-pull, never the aggregate
per request). Per sweep:

1. `period = current first-of-month::date`.
2. Hoist plans, weights (`metric_weights`), default FX (`pricing_config`) once — loaders
   byte-identical to today (no `WHERE effective_from` — we did not version the catalogs).
3. **One** batched fleet read: `SELECT app_id, metric, total FROM usage_aggregates WHERE period =
   $1`, now hitting `usage_aggregates_period_idx` instead of scanning the app-leading PK. Group by
   app in Rust.
4. Per app: read `(plan_id, prev_state, override_limit, prev_eval_limit)` from `apps LEFT JOIN
   app_spend_state LEFT JOIN app_spend_limit`. Price `charge_cents(price, usage, weights)` →
   `spend_cents`. Effective limit = override else plan default. `derive_state(...)`.
5. On transition: `persist_transition` (UPSERT `app_spend_state` + INSERT `spend_state_history`) in
   one tx — without the override column. Else `touch_state` keeps spend/eval-limit fresh.

The enforcement fact stays the **local** aggregate priced by the **local** resolved rate — no
provider round-trip. Spend and the in-period invoice estimate price the *current* period from the
*same* catalog rows, so they can't diverge. **Preserved** (and the config/derived split removes the
column-clobber smell without changing enforcement semantics).

### C. Reproducible month-end invoicing across pluggable providers (critique #1)

`billing_reconcile` (~hourly) bills each creator's *closed* prior period:

1. `period = prior first-of-month::date`. Claim: `INSERT INTO invoices (id, creator_id, period,
   status) VALUES (inv_…, …, 'draft') ON CONFLICT (creator_id, period) DO NOTHING`.
2. **Short-circuit:** existing row `status='finalized'` ⇒ fully billed ⇒ skip (replaces
   "`stripe_invoice_id IS NOT NULL`").
3. Per owned app: compute `ChargeBreakdown` from the *current* `usage_aggregates` + weights + FX,
   then **snapshot it onto the line**: `INSERT INTO invoice_lines (invoice_id, app_id, total_units,
   billable_units, fx_pico_cents_per_unit, base_fee_cents, amount_cents, usage_snapshot,
   weights_snapshot) … ON CONFLICT (invoice_id, app_id) DO NOTHING` — the **intent** row (C1),
   before the provider POST.
4. Provider call (native rail): `create_invoice_item`. On success, `INSERT INTO
   billing_provider_refs ('invoice_line', inv_…, 'stripe', 'invoice_item', ii_…)` — the
   **confirmation**.
5. Draft-before-finalize (C2): insert the `('invoice', …, 'draft_invoice', in_…)` ref *before*
   finalize; finalize then inserts `('invoice', …, 'invoice', in_…)` and `UPDATE invoices SET
   status='finalized', subtotal_cents=…, total_cents=…, finalized_at=NOW()`.

**Reproducibility:** once finalized, every line carries the exact `fx_pico_cents_per_unit` charged,
the computed `total_units`/`billable_units`/`amount_cents`, and the raw `usage_snapshot` +
`weights_snapshot`. A later operator edit to `metric_weights`/`pricing_config`/`plans` reprices only
*future* periods — the finalized line is self-contained and re-derivable from its own JSONB. The
snapshot-onto-invoice cure for critique #1 **without** versioned rate cards.

**Provider-agnostic:** native writes `billing_provider_refs`; Stripe-Meters/OpenMeter
self-invoice/export and write their own kinds (or none). Core `invoices`/`invoice_lines` carry no
Stripe id — #4 closed.

### D. Crash / retry / >24h idempotency (no double-bill)

The C1/C2 proof relocates faithfully onto the side table:

- **Claim short-circuit:** `invoices.status='finalized'` ⇒ skip (was `stripe_invoice_id NOT NULL`).
  A re-drive of a `draft` row continues.
- **Per-app skip set (C1):** the **presence** of a `('invoice_line', …, 'invoice_item', …)` ref ⇒
  definitely posted ⇒ skip. **Absence** with an `invoice_lines` intent row present ⇒ intent
  recorded, outcome unknown (crash mid-call). On re-drive we do NOT blindly re-POST: within Stripe's
  24h window the deterministic Idempotency-Key replays the same item; past 24h we
  `find_invoice_item_by_key` (deterministic `metadata.zs_item_key`) and adopt-or-post, then insert
  the `invoice_item` ref. The **ref row — not Stripe's 24h key — is the durable double-bill guard**,
  exactly as `stripe_item_id` was.
- **Draft adopt (C2):** a re-drive that finds a `draft_invoice` ref but no `invoice` ref re-finalizes
  *that* draft (which carries the real line items) by id, never creating a fresh empty draft that
  would finalize a $0 invoice and under-bill past Stripe's 24h create-key expiry.

Every idempotency anchor is preserved: `invoices` UNIQUE`(creator_id, period)` (was `billing_runs`
PK), `invoice_lines` PK`(invoice_id, app_id)` (was `billing_run_items` PK grain), `metering_exports`
PK`(app_id, period)`, `usage_reports_seen` PK`(worker_id, period, sequence)`. The prior holistic
fixes for the crash-window and spend↔invoice divergence are **structurally** preserved, not
re-derived. **Preserved.**

### E. Export delta watermark (>24h safe)

`metering_export` cron (only for stripe/openmeter): derive current cumulative CU from
`usage_aggregates` (`total_units(weights, period_totals)` — no CU column), `delta = current −
exported_units`, push delta (skip when 0), on success `UPDATE metering_exports SET exported_units =
current`. Deterministic push `identifier` dedups Stripe-side too; the local high-water is the primary
guard past Stripe's window. Unchanged except `period DATE`. **Preserved.**

---

## What we deliberately did NOT build (deferred, per pragmatic-postgres)

- **Effective-dated rate cards / price versions** (`metric_weight_versions`, `fx_versions`,
  `plan_price_versions`). Snapshot-onto-line (§7) gives reproducibility for the realistic case
  (operator edits a global weight/FX) at a fraction of the complexity. Versioning earns its place
  only with *scheduled* future price changes or mid-period transitions — neither exists pre-launch.
  (The sibling `billing-metering-schema-v2.md` is the variant that builds these.)
- **Double-entry GL / ledger_entry, credits, proration, tax lines, multi-currency.** `currency` and
  `*_cents` columns are present so the shape is right; the models arrive with Stream-2 (Connect, the
  15% application fee, multi-party splits) — that is where double-entry earns its keep.
- **Slotted-counter aggregate sharding** (slot = worker_id). The correct future answer to write
  contention, but it doubles the read into `SUM…GROUP BY` and forks the source of truth; defer until
  contention is measured.
- **Native declarative range-partitioning + retention cron** for `usage_reports_seen` /
  `spend_state_history`. The `period` key is in place so this is a drop-in later (no extension); not
  built now.
- **Invoice immutability trigger.** "No DELETE grant" + the `invoice_status` domain (only
  draft→finalized→void) are sufficient pre-launch; the reject-UPDATE-on-finalized trigger is a
  Stream-2 hardening.

These are the lines the philosophy draws: fix every critique with the fewest tables, leave the shape
ready for post-launch additions, build none of them speculatively.
