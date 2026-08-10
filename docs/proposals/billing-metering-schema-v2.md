# Billing & Metering Schema v2 — Redesign

Status: proposal (pre-launch clean rewrite of changesets `0037`–`0043`)
Branch: `feat/billing-metering`
Philosophy: **billing-platform-grade** — fully effective-dated rate cards, an append-only money ledger with DB-enforced immutability + a balance invariant, and provider-agnostic invoices / lines / credits. The most correct and auditable design, accepting more tables.

This document specifies the **full Postgres DDL** for every table (columns, types, PK/FK, CHECK, RLS, grants, indexes, partitioning), a **table-by-table rationale**, an **OLD→NEW mapping**, and **flow walkthroughs** proving the load-bearing invariants survive: idempotent ingest dedup, spend enforcement reading the *local* current-period aggregate, reproducible cross-provider month-end invoicing, and crash/retry/>24h idempotency.

Every choice is grounded in (a) the real query paths in `crates/control/src/{pricing,pricing_store,spend,metering,internal}.rs` + `cron/{billing_reconcile,metering_export,spend_reconcile}.rs`, (b) the four research briefs (rate cards, metering ledgers, invoice/money model, spend/hygiene), and (c) the hard constraints (PG16 via compio-postgres, zero exotic extensions, RLS fail-closed + least-priv, CU+FX neutral unit, pluggable providers, local spend coupling, idempotent ingest + invoicing).

---

## 0. Design pillars (the four invariants the shapes encode)

1. **Reproducibility — bills reconstruct to the price in effect when usage occurred.** Rate cards (CU weights *and* FX *and* plan price) are **append-only + effective-dated**, never mutated in place; the resolved rate is **snapshotted onto the invoice line** at finalize. A mid-period or after-the-fact catalog edit can never restate a finalized invoice. (Fixes critique #1, the biggest flaw.)
2. **Local, fast spend enforcement.** The minute-cadence spend sweep prices the **local** `usage_aggregate` against the **current-period** resolved rate — no provider round-trip. Because rates change only at **period boundaries** (a deliberate, defensible simplification), spend-pricing and invoice-pricing read the *same* version → spend and invoice cannot diverge (preserves the prior holistic-review fix structurally, not by code discipline).
3. **Money correctness + auditability without a full GL.** Invoices/lines/credits are **append-only**; an immutability trigger rejects mutation of finalized rows except the one legal `→ void` transition; `total = subtotal − credit + tax` is a stored CHECK. Ledger *discipline*, not a full double-entry account schema (deferred to Stream-2).
4. **Provider-agnostic core.** No Stripe id lives in a core money table. A single `billing_provider_ref` side table carries `cus_…`/`in_…`/`ii_…`/OpenMeter ids. The claim-then-call C1/C2 crash-window proof is relocated faithfully onto `invoice_line` (intent) + the ref row (confirmation), not redesigned.

Money-unit discipline (critique #6): every money column is integer **cents** and named `*_cents`; the only sub-cent quantity is **`fx_pico_cents_per_unit`** (10⁻¹² cent per CU), which is explicitly an FX rate, never a money column. CU columns are `*_units`. Pico-cents never appear in a `*_cents` column. The single place both meet — the invoice line — freezes the conversion.

Period discipline (critique #2): the period key is **`period DATE`** (first-of-month), exact and indexable, replacing `period_start TIMESTAMPTZ` bound through `f64`/`period_ts` round-trips everywhere.

---

## 1. Enums / domains / helper objects

The bespoke `compio-postgres` driver reads these columns as `String`/`i64` today. To keep that wire form intact (and avoid adding an ENUM codec to the driver), closed string sets are modeled as **`DOMAIN`-over-`CHECK`** (wire type stays `TEXT`, value-closure enforced by the DB). `metric_kind` is the one true native `ENUM` (internal, never crosses the driver as a bound param). Period is a `DATE` domain with a first-of-month CHECK.

```sql
-- Spend enforcement state machine (closed, engineer-owned; ordered allow<warn<degrade<block
-- matches spend.rs severity()). DOMAIN-over-CHECK keeps the TEXT wire form + the
-- existing Option<String> read path + snake_case serde contract; DB rejects bad writes.
CREATE DOMAIN zeroship.spend_state AS TEXT
    CHECK (VALUE IN ('allow','warn','degrade','block'));

-- Invoice lifecycle. The immutability trigger (§9) keys off this.
CREATE DOMAIN zeroship.invoice_status AS TEXT
    CHECK (VALUE IN ('draft','finalizing','finalized','void'));

-- Credit/adjustment reason (forward-posted corrections).
CREATE DOMAIN zeroship.credit_reason AS TEXT
    CHECK (VALUE IN ('overbill','goodwill','promo','tax_adjust','manual'));

-- Metric provenance. Closed set, internal-only ⇒ native ENUM is fine.
CREATE TYPE zeroship.metric_kind AS ENUM ('platform','primitive','custom');

-- Period key: first-of-month DATE. A domain documents + enforces the invariant
-- (no "is this exactly midnight UTC?" fragility, no f64 round-trip).
CREATE DOMAIN zeroship.billing_period AS DATE
    CHECK (EXTRACT(DAY FROM VALUE) = 1);

-- ISO-4217 currency. USD-only at launch; column present so the shape is right
-- when multi-currency arrives (the invoice carries it; FX-at-invoice deferred).
CREATE DOMAIN zeroship.currency_code AS CHAR(3)
    CHECK (VALUE ~ '^[a-z]{3}$');
```

`fx_pico_cents_per_unit` keeps its near-zero floor (`>= 1000` = `MIN_FX_PICO_CENTS_PER_UNIT`, mirrored in `pricing.rs`) wherever it appears.

---

## 2. Metric catalog — the FK spine (replaces free-text `metric`)

Fixes critique #3 (silent-$0). Every metric that can be weighted, aggregated, or billed must be a **cataloged** row. Fixed platform/primitive metrics are a closed seed set FK'd from weights; open-ended **custom** SDK metrics are **auto-registered on first ingest** (inside the ingest tx) so the FK on `usage_aggregate` always resolves while still allowing creator-defined names.

```sql
CREATE TABLE zeroship.billing_metrics (
    metric       TEXT PRIMARY KEY,                 -- 'requests','cpu_us',… stable code-level name
    kind         zeroship.metric_kind NOT NULL,    -- platform | primitive | custom
    unit         TEXT NOT NULL,                    -- 'request','microsecond','byte' (display/audit)
    archived     BOOLEAN NOT NULL DEFAULT false,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
-- GLOBAL operator catalog (mirrors plans): NOT tenant-scoped, NO RLS, control BYPASSRLS.
-- Grants include INSERT so ingest can auto-register a custom metric on first sight.
-- No DELETE: archive, never hard-delete (weights/usage/lines FK into it forever).

-- Seed the fixed metrics (kind = platform | primitive). Custom rows arrive at runtime.
INSERT INTO zeroship.billing_metrics (metric, kind, unit) VALUES
    ('requests',             'platform',  'request'),
    ('cpu_us',               'platform',  'microsecond'),
    ('wall_us',              'platform',  'microsecond'),
    ('ingress_bytes',        'platform',  'byte'),
    ('egress_bytes',         'platform',  'byte'),
    ('db_reads',             'primitive', 'operation'),
    ('db_writes',            'primitive', 'operation'),
    ('db_rows_written',      'primitive', 'row'),
    ('kv_reads',             'primitive', 'operation'),
    ('kv_writes',            'primitive', 'operation'),
    ('storage_ops',          'primitive', 'operation'),
    ('storage_bytes',        'primitive', 'byte'),
    ('storage_egress_bytes', 'primitive', 'byte')
ON CONFLICT (metric) DO NOTHING;
```

**Semantics retained:** an emitted-but-unweighted metric still prices to 0 CU (resilient default), but it is now a **queryable left-join gap** (`billing_metrics LEFT JOIN metric_weight_versions`), and the pricing loader can emit an alertable "cataloged, no current weight version" signal — never an invisible $0.

---

## 3. Effective-dated CU cost model (replaces mutated `metric_weights`)

Append-only, keyed `(metric, effective_from)`. The weight effective for period *P* is `… WHERE effective_from <= P ORDER BY effective_from DESC LIMIT 1` (no stored `effective_to` → no dual-write hazard). CU stays **derived from raw totals** (no CU column anywhere), so a re-weight reprices only *future* periods, never history.

```sql
CREATE TABLE zeroship.metric_weight_versions (
    metric         TEXT NOT NULL REFERENCES zeroship.billing_metrics(metric) ON DELETE RESTRICT,
    effective_from zeroship.billing_period NOT NULL,            -- first period this weight applies to
    units_per_op   BIGINT NOT NULL CHECK (units_per_op >= 0),   -- CU per per_units ops (MINOR-2)
    per_units      BIGINT NOT NULL CHECK (per_units > 0),       -- divisor (sub-unit weights)
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (metric, effective_from)
);
-- GLOBAL operator config: NOT tenant-scoped, NO RLS, control BYPASSRLS.
-- Append-only ⇒ grants are SELECT, INSERT only (NO UPDATE, NO DELETE): a price
-- change INSERTs a new (metric, effective_from) row, never mutates an old one.

-- Seed the launch weights at the genesis period (effective_from = first-of-launch-month;
-- shown as a parameter the bootstrap fills — the deterministic period is set in seed code,
-- mirroring how seed_plans() owns the pln_… ids).
-- INSERT INTO zeroship.metric_weight_versions (metric, effective_from, units_per_op, per_units)
-- VALUES ('requests', :genesis, 1, 1), ('cpu_us', :genesis, 1, 1000), … (same 13 weights as 0041).
```

**Resolver (drop-in for `PricingStore::weights()`):** the loader gains a `WHERE effective_from <= $period` + `DISTINCT ON`. It still returns the same `MetricWeights` map; `charge_cents` is unchanged downstream.

```sql
SELECT DISTINCT ON (metric) metric, units_per_op, per_units
FROM zeroship.metric_weight_versions
WHERE effective_from <= $1                    -- $1 = the period being priced
ORDER BY metric, effective_from DESC;
```

---

## 4. Effective-dated FX default (replaces single-row `pricing_config`)

The global default FX becomes a temporal table; the resolver picks the version effective for the priced period. The near-zero floor CHECK (`>= 1000`) is preserved as the source-level guard (with `default_fx_pico_cents_per_unit` still fail-closed in code as defense in depth, MAJOR-3).

```sql
CREATE TABLE zeroship.fx_versions (
    effective_from         zeroship.billing_period NOT NULL PRIMARY KEY,
    fx_pico_cents_per_unit BIGINT NOT NULL CHECK (fx_pico_cents_per_unit >= 1000),  -- 10^-12 cent/CU
    created_at             TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
-- GLOBAL: NO RLS, control BYPASSRLS, append-only grants SELECT, INSERT.
-- Seed: INSERT (effective_from=:genesis, fx_pico_cents_per_unit=30000000)  -- the 0041 default.
```

**Resolver (drop-in for `default_fx_pico_cents_per_unit()`):**

```sql
SELECT fx_pico_cents_per_unit FROM zeroship.fx_versions
WHERE effective_from <= $1 ORDER BY effective_from DESC LIMIT 1;
```

`None` (no row ≤ period, or below floor) still means **UnresolvedFx** → the sweep fails closed (aborts), exactly as today.

---

## 5. Plan identity + effective-dated plan price (splits `plans`)

`plans` keeps **stable identity only**; all priced fields move to an append-only `plan_price_versions` child. **`runtime_limits_json` moves off the priced model entirely** (critique #5, the poison-tolerance coupling) onto plan identity — a price version carries only money.

```sql
-- Stable plan identity (pln_<base62>). apps.plan_id FK targets THIS (unchanged).
CREATE TABLE zeroship.plans (
    id                    TEXT PRIMARY KEY,                  -- pln_<base62> typed id
    name                  TEXT NOT NULL,
    assignable_by_creator BOOLEAN NOT NULL DEFAULT false,    -- MAJOR-4 self-service guardrail (0042)
    runtime_limits_json   JSONB NOT NULL,                    -- AppRuntimeLimits — OFF the price model (critique #5)
    archived              BOOLEAN NOT NULL DEFAULT false,    -- soft-delete; never hard-deleted
    created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
-- apps.plan_id FK + index unchanged from 0038 (Registry::get_versions LEFT JOINs every 5s pull):
--   ALTER TABLE zeroship.apps ADD CONSTRAINT apps_plan_fk
--       FOREIGN KEY (plan_id) REFERENCES zeroship.plans(id) ON DELETE RESTRICT;
--   CREATE INDEX apps_plan_id_idx ON zeroship.apps(plan_id);
-- GLOBAL operator catalog: NO RLS, control BYPASSRLS, grants SELECT, INSERT, UPDATE
-- (UPDATE only for name/archived/assignable_by_creator/runtime_limits — NOT price). No DELETE.

-- Append-only effective-dated price. A price change INSERTs a new (plan_id, effective_from).
CREATE TABLE zeroship.plan_price_versions (
    plan_id                   TEXT NOT NULL REFERENCES zeroship.plans(id) ON DELETE RESTRICT,
    effective_from            zeroship.billing_period NOT NULL,
    base_fee_cents            BIGINT NOT NULL DEFAULT 0 CHECK (base_fee_cents >= 0),
    included_units            BIGINT NOT NULL DEFAULT 0 CHECK (included_units >= 0),    -- CU free before overage
    fx_pico_cents_per_unit    BIGINT CHECK (fx_pico_cents_per_unit IS NULL
                                            OR fx_pico_cents_per_unit >= 1000),         -- NULL ⇒ inherit fx_versions
    spend_limit_default_cents BIGINT NOT NULL DEFAULT 0 CHECK (spend_limit_default_cents >= 0),
    created_at                TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (plan_id, effective_from)
);
-- GLOBAL: NO RLS, control BYPASSRLS, append-only grants SELECT, INSERT.
```

**Resolver (drop-in for `PlanCatalog::list()`/`get()` — still returns a scalar `PlanPrice` per plan):**

```sql
SELECT DISTINCT ON (plan_id) plan_id, base_fee_cents, included_units,
       fx_pico_cents_per_unit, spend_limit_default_cents
FROM zeroship.plan_price_versions
WHERE effective_from <= $1 ORDER BY plan_id, effective_from DESC;
```

`PlanPrice` / `with_effective_fx(default_fx)` / `charge_cents` downstream are **unchanged** — they receive a resolved scalar exactly as today.

**Period-boundary guard (the spend↔invoice equality invariant).** Versions take effect at period boundaries only; inserting a `*_version` with `effective_from` *inside* an already-open period is forbidden (app-layer assertion; optionally a trigger comparing to the current period). This is the deliberate simplification that makes spend-pricing and reconcile-pricing read the identical resolved version — acceptable because zeroship bills monthly with a local minute-cadence enforcer (no Orb/Metronome mid-cycle split needed).

---

## 6. Usage aggregate — per-producer-sharded (slot = `worker_id`)

Replaces `usage_aggregates`. The hot-row write contention (one `(app, period, metric)` row UPSERT-incremented by every worker flush, ~10s × N workers) is removed by **sharding the slot on `worker_id`** — the producer that already owns the delta. Each worker writes only **its own** row → zero cross-worker lock contention, and the dedup tx no longer touches a shared row. The once-a-minute fleet read becomes `SUM … GROUP BY app_id, metric`.

`period` is `DATE`; the table is **native declarative range-partitioned by `period`** (PG16 core, no `pg_partman`), so the current-period fleet scan hits exactly one partition and old months drop as a metadata op.

```sql
CREATE TABLE zeroship.usage_aggregates (
    app_id     UUID NOT NULL REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    period     zeroship.billing_period NOT NULL,
    metric     TEXT NOT NULL REFERENCES zeroship.billing_metrics(metric) ON DELETE RESTRICT,
    worker_id  TEXT NOT NULL,                                    -- the shard/slot (free: worker owns its deltas)
    total      BIGINT NOT NULL DEFAULT 0 CHECK (total >= 0),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (app_id, period, metric, worker_id)
) PARTITION BY RANGE (period);
-- Note: the FK to a partitioned table from non-partitioned children is one-directional
-- here (this table references billing_metrics/apps, which are non-partitioned — supported).

-- The fleet sweep predicate leads with `period`; PK leads with app_id, so add the
-- access-path index. (Per-partition, so it's created per child or via the parent.)
CREATE INDEX usage_aggregates_period_app_idx
    ON zeroship.usage_aggregates (period, app_id);

-- RLS: verbatim fail-closed app_id-keyed pattern (GUC zeroship.tenant_app), FORCE,
-- control BYPASSRLS. WITH CHECK retained (a future non-bypass writer is confined).
ALTER TABLE zeroship.usage_aggregates ENABLE ROW LEVEL SECURITY;
ALTER TABLE zeroship.usage_aggregates FORCE  ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON zeroship.usage_aggregates
    USING      (app_id = current_setting('zeroship.tenant_app', true)::uuid)
    WITH CHECK (app_id = current_setting('zeroship.tenant_app', true)::uuid);
-- Grants: SELECT, INSERT, UPDATE to zeroship_control (UPSERT total = total + delta).
```

Partitions are pre-created ahead and dropped on expiry by a new `usage_retention` cron (same shape as the existing `audit_retention` / `orphaned_app_reaper` crons):

```sql
-- e.g. one partition per month:
CREATE TABLE zeroship.usage_aggregates_2026_06 PARTITION OF zeroship.usage_aggregates
    FOR VALUES FROM ('2026-06-01') TO ('2026-07-01');
```

**Read shapes the code runs:**
- spend fleet sweep (`spend.rs`): `SELECT app_id, metric, SUM(total) FROM zeroship.usage_aggregates WHERE period = $1 GROUP BY app_id, metric;` (one query, one partition, + a `GROUP BY`).
- per-app `period_totals` (`metering.rs`, reconcile + export): `SELECT metric, SUM(total) FROM zeroship.usage_aggregates WHERE app_id = $1 AND period = $2 GROUP BY metric;`

K = active workers for an app in a period (a handful), so the SUM is cheap; the contention elimination is the bigger win. A closed-period rollup-to-`worker_id='*'` cron is an **optional later** O(1)-read optimization — **not built pre-launch** (avoids a second source of truth).

---

## 7. Idempotent ingest ledger — period-scoped, replayable (replaces `usage_reports_seen`)

The dedup key gains **`period`** so it is prunable by partition, and the row **carries the applied delta payload** so `usage_aggregates` is rebuildable/auditable from the ledger (the cheap substitute for a per-event raw store — no Kafka/ClickHouse, honoring the zero-extra-infra constraint). This keeps the correct natural-composite-PK dedup pattern and the worker-keyed (not app-keyed) control-internal nature.

```sql
CREATE TABLE zeroship.usage_report_ledger (
    worker_id  TEXT NOT NULL,                       -- producer identity
    period     zeroship.billing_period NOT NULL,
    sequence   BIGINT NOT NULL,                     -- monotonic per-worker counter
    payload    JSONB NOT NULL,                      -- the per-app deltas this report applied (replay/audit)
    seen_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (worker_id, period, sequence)
) PARTITION BY RANGE (period);
-- NOT app-keyed (worker-keyed control-internal bookkeeping) ⇒ NO tenant RLS
-- (mirrors the old usage_reports_seen / creator_billing). Control is BYPASSRLS.
-- Grants: SELECT, INSERT (dedup gate + high-water resync). No UPDATE/DELETE.
-- A usage_retention cron drops partitions older than the dedup window (current-period
-- retry dedup only needs the current partition).
```

Adding `period` to the PK closes the "worker restarts, resets sequence, silently collapses new reports against old keys" gap noted in the metering-ledger brief.

---

## 8. Spend: config / state split + append-only history

Splits the old `app_spend_state` (which fused config + derived state and forced three UPSERTs to each preserve foreign columns) into:

- **`app_spend_limit`** — config: the creator **override** only, written solely by `set_limit`. Absent row ⇒ use plan default.
- **`app_spend_state`** — derived hot enforcement state, UPSERTed every ~60s tick; this is what the 5s route-pull LEFT JOINs onto `RouteEntry`.
- **`spend_state_history`** — append-only transition audit, range-partitioned + retained.

```sql
-- CONFIG (rarely written; set_limit only; route-pull never reads it).
CREATE TABLE zeroship.app_spend_limit (
    app_id            UUID PRIMARY KEY REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    spend_limit_cents BIGINT CHECK (spend_limit_cents IS NULL OR spend_limit_cents >= 0),  -- creator override
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- DERIVED hot state (UPSERTed per ~60s tick; LEFT JOINed onto RouteEntry every 5s).
CREATE TABLE zeroship.app_spend_state (
    app_id           UUID PRIMARY KEY REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    state            zeroship.spend_state NOT NULL DEFAULT 'allow',
    spend_cents      BIGINT NOT NULL DEFAULT 0 CHECK (spend_cents >= 0),
    eval_limit_cents BIGINT NOT NULL DEFAULT 0 CHECK (eval_limit_cents >= 0),  -- effective limit at last derive
    period           zeroship.billing_period NOT NULL,
    evaluated_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- APPEND-ONLY transition audit (partitioned + retained).
CREATE TABLE zeroship.spend_state_history (
    app_id      UUID NOT NULL REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    from_state  zeroship.spend_state NOT NULL,
    to_state    zeroship.spend_state NOT NULL,
    spend_cents BIGINT NOT NULL CHECK (spend_cents >= 0),
    limit_cents BIGINT CHECK (limit_cents IS NULL OR limit_cents >= 0),
    period      zeroship.billing_period NOT NULL,
    at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
) PARTITION BY RANGE (period);
CREATE INDEX idx_spend_state_history_app_at ON zeroship.spend_state_history (app_id, at DESC);

-- RLS: all three are app-keyed ⇒ verbatim fail-closed pattern, FORCE, control BYPASSRLS.
-- app_spend_limit + app_spend_state get USING + WITH CHECK (a non-bypass writer is confined);
-- history is append-only ⇒ USING only.
ALTER TABLE zeroship.app_spend_limit ENABLE ROW LEVEL SECURITY;
ALTER TABLE zeroship.app_spend_limit FORCE  ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON zeroship.app_spend_limit
    USING (app_id = current_setting('zeroship.tenant_app', true)::uuid)
    WITH CHECK (app_id = current_setting('zeroship.tenant_app', true)::uuid);
ALTER TABLE zeroship.app_spend_state ENABLE ROW LEVEL SECURITY;
ALTER TABLE zeroship.app_spend_state FORCE  ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON zeroship.app_spend_state
    USING (app_id = current_setting('zeroship.tenant_app', true)::uuid)
    WITH CHECK (app_id = current_setting('zeroship.tenant_app', true)::uuid);
ALTER TABLE zeroship.spend_state_history ENABLE ROW LEVEL SECURITY;
ALTER TABLE zeroship.spend_state_history FORCE  ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON zeroship.spend_state_history
    USING (app_id = current_setting('zeroship.tenant_app', true)::uuid);
-- Grants: app_spend_limit SELECT,INSERT,UPDATE; app_spend_state SELECT,INSERT,UPDATE;
-- spend_state_history SELECT,INSERT. No DELETE.
```

The three "preserve the column I don't own" UPSERTs collapse: `set_limit` writes only `app_spend_limit`; the sweep writes only `app_spend_state` (+ appends history). The route-pull join reads only `app_spend_state` (it never needed the override).

---

## 9. Provider-agnostic invoice / line / credit model (replaces `creator_billing` + `billing_runs` + `billing_run_items`)

The core money model carries **CU + cents + version snapshots only**. The four Stripe-shaped columns (`creator_billing.stripe_customer_id`, `billing_runs.{stripe_invoice_id, draft_invoice_id}`, `billing_run_items.stripe_item_id`) move to the `billing_provider_ref` side table (§10). The C1/C2 crash-window proof is relocated faithfully (§13).

```sql
-- Creator billing identity (creator_id = a USER id; owner resolved via app_members).
-- Stripe customer id REMOVED → lives in billing_provider_ref.
CREATE TABLE zeroship.creator_billing (
    creator_id     UUID PRIMARY KEY REFERENCES zeroship.users(id) ON DELETE RESTRICT,  -- billing outlives user
    default_pm_set BOOLEAN NOT NULL DEFAULT false,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
-- Control-internal, creator(user)-keyed ⇒ NO app RLS (mirrors old creator_billing).
-- Grants SELECT, INSERT, UPDATE. ON DELETE RESTRICT (was CASCADE): invoices/identity
-- must outlive the user row (audit). [critique #6]

-- INVOICE header. UNIQUE(creator_id, period) IS the idempotency claim (== old billing_runs PK).
-- status carries the per-period short-circuit (finalized ⇒ skip), replacing
-- "stripe_invoice_id IS NOT NULL".
CREATE TABLE zeroship.invoices (
    id             TEXT PRIMARY KEY,                                    -- inv_<base62> typed id
    creator_id     UUID NOT NULL REFERENCES zeroship.creator_billing(creator_id) ON DELETE RESTRICT,
    period         zeroship.billing_period NOT NULL,
    status         zeroship.invoice_status NOT NULL DEFAULT 'draft',
    currency       zeroship.currency_code  NOT NULL DEFAULT 'usd',
    subtotal_cents BIGINT NOT NULL DEFAULT 0 CHECK (subtotal_cents >= 0),
    credit_cents   BIGINT NOT NULL DEFAULT 0 CHECK (credit_cents   >= 0),
    tax_cents      BIGINT NOT NULL DEFAULT 0 CHECK (tax_cents       >= 0),  -- present; 0 at launch (USD, no tax)
    total_cents    BIGINT NOT NULL DEFAULT 0 CHECK (total_cents     >= 0),
    finalized_at   TIMESTAMPTZ,
    voided_at      TIMESTAMPTZ,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- Money invariant enforced by the DB (ledger discipline, not app code):
    CONSTRAINT invoice_total_balances CHECK (total_cents = subtotal_cents - credit_cents + tax_cents),
    UNIQUE (creator_id, period)                                          -- the idempotency claim
);

-- INVOICE LINE: one per app per invoice (== old billing_run_items grain). app_id now FK'd.
-- The SNAPSHOT block freezes the resolved rate so the bill is reproducible from the row
-- alone — belt (version ids) AND suspenders (frozen numbers).
CREATE TABLE zeroship.invoice_lines (
    invoice_id                TEXT NOT NULL REFERENCES zeroship.invoices(id) ON DELETE RESTRICT,
    app_id                    UUID NOT NULL REFERENCES zeroship.apps(id) ON DELETE RESTRICT,  -- FK now present (#6)
    plan_id                   TEXT NOT NULL REFERENCES zeroship.plans(id) ON DELETE RESTRICT,
    -- ── SNAPSHOT (rate-freeze): which versions were applied … ──
    plan_price_effective_from zeroship.billing_period NOT NULL,    -- which plan_price_version
    fx_effective_from         zeroship.billing_period,             -- which fx_version (NULL if plan had own fx)
    -- ── … and the resolved numbers actually charged ──
    total_units               BIGINT NOT NULL CHECK (total_units    >= 0),  -- derived CU at bill time
    billable_units            BIGINT NOT NULL CHECK (billable_units >= 0),
    fx_pico_cents_per_unit    BIGINT NOT NULL CHECK (fx_pico_cents_per_unit >= 1000),  -- RESOLVED rate charged
    base_fee_cents            BIGINT NOT NULL DEFAULT 0 CHECK (base_fee_cents >= 0),
    amount_cents              BIGINT NOT NULL CHECK (amount_cents   >= 0),  -- == charge_cents() for this app
    metric_breakdown          JSONB,                               -- raw metric SUMs this period (audit)
    created_at                TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (invoice_id, app_id)
);

-- CREDIT / adjustment: forward-posted, append-only. Corrections NEVER mutate a
-- finalized invoice (Metronome/Lago/Stripe pattern). Applied to a FUTURE invoice.
CREATE TABLE zeroship.credits (
    id                 TEXT PRIMARY KEY,                                  -- cr_<base62>
    creator_id         UUID NOT NULL REFERENCES zeroship.creator_billing(creator_id) ON DELETE RESTRICT,
    reason             zeroship.credit_reason NOT NULL,
    amount_cents       BIGINT NOT NULL CHECK (amount_cents > 0),
    currency           zeroship.currency_code NOT NULL DEFAULT 'usd',
    source_invoice_id  TEXT REFERENCES zeroship.invoices(id) ON DELETE RESTRICT,  -- the over-billed invoice (NULL for promo)
    applied_invoice_id TEXT REFERENCES zeroship.invoices(id) ON DELETE RESTRICT,  -- the future invoice it offsets; NULL until consumed
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX credits_unapplied_idx ON zeroship.credits (creator_id) WHERE applied_invoice_id IS NULL;

-- RLS: invoices/lines/credits are control-only bookkeeping keyed by creator_id/app_id.
-- Following the old creator_billing/billing_runs posture: control-internal, creator-keyed
-- ⇒ NO app RLS (the app_id on a line is for FK/audit, not a tenant axis here); control is
-- BYPASSRLS. (If a creator-facing read is ever added, add a zeroship.tenant_user GUC +
-- policy — the two tenant axes are documented, app_id vs creator user_id.)
-- Grants: SELECT, INSERT, UPDATE (no DELETE — immutability via trigger below).

-- IMMUTABILITY trigger (ledger discipline): once finalized, the only legal change is
-- the single transition to 'void'. No edit, no delete of a finalized invoice/line/credit.
CREATE FUNCTION zeroship.invoices_immutable() RETURNS trigger AS $fn$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'invoices are append-only (no DELETE)';
    END IF;
    IF OLD.status = 'finalized' THEN
        -- allow ONLY finalized → void (which also writes voided_at); reject all else.
        IF NEW.status = 'void'
           AND NEW.id = OLD.id AND NEW.creator_id = OLD.creator_id
           AND NEW.period = OLD.period AND NEW.total_cents = OLD.total_cents THEN
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
-- A parallel BEFORE UPDATE OR DELETE trigger on invoice_lines rejects any change once
-- its parent invoice.status = 'finalized' (lines are frozen with the header).
```

**Why ledger discipline, not a full GL:** zeroship Stream-1 is single-rail (creator owes platform, USD-only). Full double-entry (accounts/journals/postings + balanced-transaction trigger) earns its keep only with multi-party money movement — which is **Stream-2** (Connect, the 15% application fee, splits). The 80% of ledger value here is (a) append-only + immutability trigger, (b) the `total = subtotal − credit + tax` CHECK, (c) one money unit per column. A real `ledger_entry` table is the documented place to grow when Stream-2 lands.

**Deferred by design (pre-launch, no demand):** proration tables (no mid-cycle plan swaps in the money path — whole-calendar-month infra billing), multi-currency FX-at-invoice, tax-line modeling. `tax_cents`/`currency` columns are present and trivially populated so the shape is right when Stream-2 arrives.

---

## 10. Provider-ref side table (fixes critique #4)

Every external id maps to a core object here; the native provider writes **zero** rows. This is what makes the backend swappable without touching the money model.

```sql
CREATE DOMAIN zeroship.billing_object AS TEXT
    CHECK (VALUE IN ('invoice','invoice_line','customer'));

CREATE TABLE zeroship.billing_provider_ref (
    object_type   zeroship.billing_object NOT NULL,   -- 'invoice' | 'invoice_line' | 'customer'
    object_id     TEXT NOT NULL,                      -- inv_… | app_id (line) | creator_id (customer)
    provider      TEXT NOT NULL,                      -- 'stripe' | 'stripe_meters' | 'openmeter'
    ref_kind      TEXT NOT NULL,                      -- 'invoice' | 'draft_invoice' | 'invoice_item' | 'customer'
    external_id   TEXT NOT NULL,                      -- in_… | ii_… | cus_…
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (object_type, object_id, provider, ref_kind),
    UNIQUE (provider, external_id, ref_kind)          -- one external id ↔ one object
);
-- Control-internal mapping ⇒ NO tenant RLS; control BYPASSRLS.
-- Grants SELECT, INSERT, UPDATE (UPDATE fills external_id after the POST confirms). No DELETE.
```

The four old columns relocate cleanly:

| Old column | `billing_provider_ref` row |
| --- | --- |
| `creator_billing.stripe_customer_id` | `('customer', creator_id, 'stripe', 'customer', cus_…)` |
| `billing_runs.draft_invoice_id` | `('invoice', inv_…, 'stripe', 'draft_invoice', in_…)` |
| `billing_runs.stripe_invoice_id` | `('invoice', inv_…, 'stripe', 'invoice', in_…)` |
| `billing_run_items.stripe_item_id` | `('invoice_line', app_id, 'stripe', 'invoice_item', ii_…)` — scoped within the invoice |

The metering-export watermark (§11) deliberately needs **no** provider id (it pushes CU deltas keyed by `(app, period)`; the provider is selected by config), keeping enforcement provider-independent.

---

## 11. Metering export watermark (replaces `metering_exports`, retyped period)

Already best-practice (cumulative high-water + deterministic identifier + reconcile-against-provider-aggregate + durable failure surface). Kept verbatim except `period_start TIMESTAMPTZ → period DATE`. **Export-only; never feeds enforcement** (the spend cap reads `usage_aggregates` directly — the same local fact for native/stripe/openmeter).

```sql
CREATE TABLE zeroship.metering_exports (
    app_id               UUID NOT NULL REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    period               zeroship.billing_period NOT NULL,
    exported_units       BIGINT NOT NULL DEFAULT 0 CHECK (exported_units >= 0),  -- cumulative CU pushed (monotonic)
    consecutive_failures INTEGER NOT NULL DEFAULT 0 CHECK (consecutive_failures >= 0),
    last_error           TEXT,                       -- redacted at source (never a SecretString)
    last_attempt_at      TIMESTAMPTZ,
    updated_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (app_id, period)
);
-- RLS: verbatim fail-closed app_id-keyed pattern, FORCE, control BYPASSRLS.
ALTER TABLE zeroship.metering_exports ENABLE ROW LEVEL SECURITY;
ALTER TABLE zeroship.metering_exports FORCE  ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON zeroship.metering_exports
    USING (app_id = current_setting('zeroship.tenant_app', true)::uuid);
-- Grants: SELECT, INSERT, UPDATE. No DELETE (watermark is monotonic per period).
```

---

## 12. Grants block (the existing role-guarded DO pattern, all tables)

Verbatim with the established guard so a dev/test DB without the 0025 role model still migrates:

```sql
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_control') THEN
    -- catalog / append-only rate cards (SELECT, INSERT; no UPDATE/DELETE):
    EXECUTE 'GRANT SELECT, INSERT ON zeroship.metric_weight_versions TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT ON zeroship.fx_versions            TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT ON zeroship.plan_price_versions    TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT ON zeroship.usage_report_ledger    TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT ON zeroship.spend_state_history    TO zeroship_control';
    -- catalog identity / config (SELECT, INSERT, UPDATE; no DELETE):
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.billing_metrics     TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.plans               TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.app_spend_limit     TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.app_spend_state     TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.usage_aggregates    TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.metering_exports    TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.creator_billing     TO zeroship_control';
    -- money ledger (SELECT, INSERT, UPDATE; DELETE blocked by trigger, not granted):
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.invoices            TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.invoice_lines       TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.credits             TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.billing_provider_ref TO zeroship_control';
  END IF;
END $g$;
```

---

## 13. OLD → NEW mapping

| Old table (0037–0043) | New table(s) | Action | Key changes |
| --- | --- | --- | --- |
| `usage_aggregates` | `usage_aggregates` | **reshape** | `period DATE` (was TIMESTAMPTZ); + `worker_id` shard in PK (hot-row fix); `metric` FK → `billing_metrics`; range-partitioned by `period`; read becomes `SUM…GROUP BY`. |
| `usage_reports_seen` | `usage_report_ledger` | **rename + enrich** | `period` added to PK (prunable, no seq-reset collapse); `+ payload JSONB` (replayable/auditable); partitioned + retained. |
| `plans` | `plans` (identity) **+** `plan_price_versions` | **split** | priced fields (`base_fee_cents`, `included_units`, `fx`, `spend_limit_default_cents`) → effective-dated child; `runtime_limits_json` stays on identity but OFF the price model (critique #5). |
| `metric_weights` | `metric_weight_versions` **+** `billing_metrics` | **split + version** | append-only `(metric, effective_from)`; `metric` FK → new catalog (critique #3). |
| `pricing_config` (single row) | `fx_versions` | **version** | append-only effective-dated default FX; floor CHECK preserved. |
| `app_spend_state` | `app_spend_limit` (config) **+** `app_spend_state` (derived) | **split** | override config separated from derived hot state (removes the 3-way preserve-foreign-column UPSERT smell); `state` typed; `period DATE`. |
| `spend_state_history` | `spend_state_history` | **reshape** | `from_state`/`to_state` typed; `period DATE`; partitioned + retained. |
| `creator_billing` | `creator_billing` | **reshape** | `stripe_customer_id` → `billing_provider_ref`; `ON DELETE CASCADE → RESTRICT` (billing outlives user). |
| `billing_runs` | `invoices` | **replace** | provider-agnostic header; `UNIQUE(creator_id, period)` is the claim; `status` enum carries the short-circuit; `total = subtotal − credit + tax` CHECK; `draft_invoice_id`/`stripe_invoice_id` → `billing_provider_ref`. |
| `billing_run_items` | `invoice_lines` | **replace** | `app_id` FK now present; **snapshot** of resolved `(plan_price_effective_from, fx_effective_from, total_units, billable_units, fx_pico_cents_per_unit, base_fee_cents, amount_cents)` (critique #1 fix); `stripe_item_id` → `billing_provider_ref`. |
| `metering_exports` | `metering_exports` | **reshape** | `period DATE` only; otherwise verbatim (already best-practice). |
| — | `credits` | **new** | forward-posted corrections without mutating history. |
| — | `billing_provider_ref` | **new** | the provider seam: all external ids live here (critique #4). |

Net: 11 tables → **15 tables + 1 trigger + enums/domains** (3 catalog/rate-card splits, the config/state spend split, the invoice/line/credit money model, the provider-ref seam). The increase is the explicit cost of reproducibility + auditability + provider-agnosticism.

---

## 14. Flow walkthroughs (the load-bearing invariants survive)

### 14.1 Idempotent ingest dedup (at-least-once usage reports)

`metering::ingest_at`, one tx per `UsageReport { worker_id, sequence, period, counters }`:

1. `INSERT INTO zeroship.usage_report_ledger (worker_id, period, sequence, payload) VALUES (…) ON CONFLICT (worker_id, period, sequence) DO NOTHING` — 0 rows affected ⇒ **already seen** ⇒ whole report is a no-op (no double-count). Same dedup-gate pattern as the old `usage_reports_seen`, now period-scoped (no seq-reset collapse) and carrying the replay `payload`.
2. For each `(app_id, metric, delta)`: auto-register the metric if custom (`INSERT INTO billing_metrics (metric, kind, unit) … ON CONFLICT DO NOTHING`), then UPSERT the **producer-sharded** row:
   `INSERT INTO usage_aggregates (app_id, period, metric, worker_id, total) VALUES ($1,$2,$3,$worker,$delta) ON CONFLICT (app_id, period, metric, worker_id) DO UPDATE SET total = usage_aggregates.total + EXCLUDED.total, updated_at = NOW();`
   Because the slot is **this worker**, the UPSERT touches only this worker's row → no cross-worker lock contention, and a retried (deduped-away) report never reaches step 2. The aggregate is rebuildable from `usage_report_ledger.payload` if ever needed.

### 14.2 Spend enforcement reading the LOCAL current-period aggregate

`spend.rs` sweep (~60s), unchanged in shape:

1. Load the **period-resolved** rate cards once: weights (§3 resolver), default FX (§4 resolver), plan prices (§5 resolver) — all with `WHERE effective_from <= current_period`. Same `MetricWeights`/`PlanPrice` types as today.
2. One batched **local** fleet read (one partition, + a GROUP BY):
   `SELECT app_id, metric, SUM(total) FROM usage_aggregates WHERE period = $current GROUP BY app_id, metric;` — no provider round-trip.
3. Per app: `charge_cents(plan.price.with_effective_fx(default_fx), usage, weights)` → `spend_cents`; effective limit = override (`app_spend_limit`) else `plan.spend_limit_default_cents`; derive state with hysteresis; UPSERT `app_spend_state` and append `spend_state_history` on transition. `UnresolvedFx` still aborts the whole sweep (fail closed); overflow still skips the app.
4. The gateway reads the **precomputed** `app_spend_state.state` via the 5s route-pull LEFT JOIN onto `RouteEntry` — never touches `usage_aggregates`. The "fast local aggregate" requirement is satisfied by the minute sweep reading the local period total, exactly as today.

Because §5's boundary guard forbids a mid-period rate change, the version the spend sweep resolves for the current period is identical to the one month-end reconcile resolves → **spend and invoice cannot diverge**.

### 14.3 Reproducible month-end invoicing across pluggable providers

`billing_reconcile::sweep` for `period = previous_period`:

1. **Claim** the period (replaces `billing_runs` INSERT): `INSERT INTO invoices (id, creator_id, period, status) VALUES (inv_…, $c, $p, 'draft') ON CONFLICT (creator_id, period) DO NOTHING`. Short-circuit: if a row exists with `status='finalized'`, the period is fully billed ⇒ no-op (replaces "`stripe_invoice_id` IS NOT NULL").
2. Per owned app: resolve the **period-effective** plan price + FX + weights, compute `charge_cents` → `ChargeBreakdown`. **Snapshot** onto the line (claim-then-call C1):
   `INSERT INTO invoice_lines (invoice_id, app_id, plan_id, plan_price_effective_from, fx_effective_from, total_units, billable_units, fx_pico_cents_per_unit, base_fee_cents, amount_cents, metric_breakdown) VALUES (…) ON CONFLICT (invoice_id, app_id) DO NOTHING` — the durable **intent**, written BEFORE the provider POST.
3. Call the **pluggable provider** (`MeteringProvider::post_line` — native is a no-op; stripe creates the invoice item). On success, record the confirmation in the side table: `INSERT INTO billing_provider_ref ('invoice_line', app_id-scoped, provider, 'invoice_item', ii_…)`. **Presence of this ref row = definitely-posted** (replaces `stripe_item_id IS NOT NULL`).
4. Finalize: provider-agnostic `subtotal_cents = Σ amount_cents`, `total_cents = subtotal − credit + tax` (CHECK-enforced); set `status='finalized'`, `finalized_at=NOW()`. The immutability trigger now freezes the invoice + all its lines.

**Reproducibility:** the invoice is reconstructable from `invoice_lines` alone — the resolved `fx_pico_cents_per_unit`, the version `effective_from` keys, and the frozen `amount_cents`/`total_units` are all on the row. A later edit to a rate card (which is anyway append-only) cannot restate it; even deleting a version row leaves the frozen numbers intact (belt + suspenders).

### 14.4 Crash / retry / >24h idempotency (no double-bill)

The C1/C2 proof maps onto the new shapes with no behavioral change:

- **Period claim (no double-bill per period):** `invoices` `UNIQUE(creator_id, period)` + `status='finalized'` short-circuit = the old `billing_runs` PK + `stripe_invoice_id` check.
- **C1 per-app double-post guard:** `invoice_lines (invoice_id, app_id)` is the durable intent written **before** the provider POST. On re-drive, read the line + its `billing_provider_ref('invoice_item')`:
  - ref present ⇒ item **definitely posted** ⇒ skip.
  - ref absent (intent-only — crashed mid-call) ⇒ do **not** blindly re-POST. Within the provider's dedup window the deterministic key (`invoice_item_idempotency_key`) replays the same item; past 24h, look the item up by its deterministic metadata key (`find_invoice_item_by_key`) and adopt it if posted, else post fresh — then INSERT the ref. The **ref row, not the provider's 24h key**, is the durable guard.
- **C2 draft-before-finalize:** insert `billing_provider_ref('invoice','draft_invoice', in_…)` the instant the draft is created, **before** finalize. A finalize crash re-drive that finds a `draft_invoice` ref but no `invoice` ref re-finalizes **that existing draft** (carrying the real line items) — never a fresh empty $0 draft. The >24h under-bill bug stays fixed.

All idempotency anchors are preserved natural composite keys; only the Stripe-shaped columns moved to the side table, keeping the idempotency PKs clean and the provider swappable.

---

## 15. Retention / partitioning (critique #6, within the no-extension constraint)

Native PG16 declarative range partitioning by `period` (no `pg_partman`) on the three unbounded growers — `usage_aggregates`, `usage_report_ledger`, `spend_state_history`. A new `usage_retention` control cron (same shape as `audit_retention` / `orphaned_app_reaper`) pre-creates next-month partitions and `DROP`s expired ones (metadata op, no row-by-row DELETE). Safe because everything needed for reproducibility lives on the **immutable invoice-line snapshot**, so raw dedup/transition logs can age out without losing auditability. Dedup of a current-period retry only needs the current partition.

---

## 16. Open decisions for implementation

1. **Genesis `effective_from`** for the seeded weight/fx/plan-price versions: set by the control bootstrap (`seed_plans()`/`seed_pricing()`), like the `pln_…` ids — the first-of-launch-month is not known at SQL-authoring time.
2. **Boundary-guard enforcement** (no `effective_from` inside an open period): app-layer assertion vs. a trigger comparing to `date_trunc('month', now())`. App-layer is lighter and sufficient; a trigger is the belt-and-suspenders option.
3. **Custom-metric auto-register** placement: inside the ingest tx (chosen above) vs. a periodic catalog-sync. In-tx keeps the FK always-resolvable with no race.
4. **Rollup-to-`worker_id='*'`** closed-period cron: deferred (not pre-launch) — only add if the `SUM…GROUP BY` over per-worker shards is ever measured hot.
