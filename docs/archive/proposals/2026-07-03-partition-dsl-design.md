# Partitioned-table DSL — design (DSL v2, platform migration)

**Status:** approved for build 2026-07-03. PG-only, fail-closed. Full lifecycle in one build.
**Goal:** author the platform's partitioned `sandbox_events` table structurally, driving the raw-marker
count `91 → ~29` while keeping the platform pg_dump semantic diff at **0**.

## Decision (settled)

- **Op model:** partition is a **dedicated op** (`Op::CreatePartition`), NOT a field/overload on
  `CreateTable`. Rejected S1 (partition-as-`CreateTable`-field: overloads `create()`, revives the
  conditional decision↔validate gate). Chose the Atlas-aligned model: parent carries a partition-by
  strategy; each child is its own first-class named object referencing the parent.
- **Surface (cosmetic, engine-identical):** `partition(name).of(parent).forValues({from,to})` /
  `.asDefault()`. Below the DSL this is one `Op::CreatePartition`; `table(parent).partition(child)` would
  compile to the same op — the surface choice is pure ergonomics and reversible (pre-launch, no back-compat).
- **Dialects:** PG-only, fail-closed. Partition ops + BRIN/INCLUDE/WITH/ONLY index features are a
  **Vendor** tier (whole-op / whole-feature availability); SQLite + MySQL reject at validate. No portable form.
- **Scope:** full lifecycle — authoring + drift + detach/drop — in one build.

## Faithfulness principle (why the intent form is safe)

The baseline `.ts` was generated from `pg_dump` of the real platform DB, so it is in pg_dump's
**decomposed** representation: children as standalone `CREATE TABLE (…cols…)` + `ALTER TABLE … ATTACH
PARTITION`, and indexes as `CREATE INDEX … ON ONLY parent` + per-child `CREATE INDEX` + `ALTER INDEX …
ATTACH PARTITION`.

We author the **intent form** instead:
- child via `CREATE TABLE child PARTITION OF parent FOR VALUES …` (columns/checks/PK inherited), and
- parent indexes via `CREATE INDEX … ON parent` (no `ONLY`).

PG then **auto-creates + auto-attaches** every child index using its deterministic auto-naming
(`sandbox_events_2026_05_ts_idx`, `…_ts_sandbox_id_user_id_data_idx`, `…_pkey`, etc. — the exact names
already in the baseline, because that is how they were originally generated). The resulting **catalog is
identical**, so `pg_dump` of our applied result byte-matches the baseline. This is the entire reason the
collapse is 62 raw markers → ~13 ops.

**Load-bearing test:** the live-PG round-trip (`platform_ir_apply_pg.rs`, `RunProfile::Platform`) is
non-negotiable — it proves PG's propagation reproduces the hand-decomposed baseline. If PG's auto-index
names or the metering INCLUDE/WHERE child name differ from the baseline by even one char, the diff breaks
and we learn immediately.

## Raw-marker scope (62 entangled markers)

`db/migrations-ts/20260702000500_sandbox_tables.ts` (8):
- 1 parent `CREATE TABLE … PARTITION BY RANGE (ts)`
- 7 table `ALTER TABLE … ATTACH PARTITION … FOR VALUES/DEFAULT`
- (the 9 `'{}'::jsonb` `SET DEFAULT` raws here are the DEFERRED container-default issue — **out of scope**)

`db/migrations-ts/20260702000600_constraints_indexes_fks.ts` (54):
- 1 `sandbox_events_pkey` PRIMARY KEY on the partitioned parent
- 4 parent `CREATE INDEX … ON ONLY` (metering INCLUDE+WHERE, sandbox_ts, ts_brin WITH, user_id_ts)
- 14 raw per-child indexes (brin WITH + metering INCLUDE/WHERE, 2 × 7 children)
- 35 `ALTER INDEX … ATTACH PARTITION` (5 × 7 children)

Intent-form replacement: parent create+partitionBy (1) + parent PK (1) + 7 `partition().of().forValues()`
(+ 1 default) + 4 parent index ops = ~13 structural ops. The currently-structural per-child
`.index().add()` calls (sandbox_id_ts, user_id_ts) become **redundant** (auto-propagated) and are deleted.

## Engine surface (crates/zeroship-migrate)

### IR (`src/model/ir.rs`) — all serde-additive, NO `CURRENT_IR_VERSION` bump

1. `Op::CreateTable`: add
   ```rust
   #[serde(rename = "partitionBy", default, skip_serializing_if = "Option::is_none")]
   partition_by: Option<PartitionSpec>,
   ```
2. New `PartitionSpec` (parent strategy):
   ```rust
   enum PartitionSpec { Range { columns: Vec<String> }, List { columns: Vec<String> }, Hash { columns: Vec<String> } }
   ```
   (Platform uses `Range`; List/Hash modeled for completeness, all PG-only.)
3. New `Op::CreatePartition { name, of, bounds, schema, existence_guard }` — child = PARTITION OF form:
   ```rust
   struct … { name: String, of: String, bounds: PartitionBounds,
              schema: Option<String>, existence_guard: Option<ExistenceGuard> }
   enum PartitionBounds {
       Range { from: Vec<PartitionBoundValue>, to: Vec<PartitionBoundValue> },
       List  { values: Vec<PartitionBoundValue> },
       Hash  { modulus: u32, remainder: u32 },
       Default,
   }
   ```
   `PartitionBoundValue` = a closed literal (string/int/timestamp/`MINVALUE`/`MAXVALUE`), NEVER raw SQL.
4. New `Op::DetachPartition { parent, name, schema, concurrently: Option<bool> }` →
   `ALTER TABLE parent DETACH PARTITION name [CONCURRENTLY]`.
5. New `Op::DropPartition { name, schema, existence_guard, cascade }` → `DROP TABLE name` (a partition is a
   real relation; dedicated op keeps the fail-closed gate + lifecycle symmetry crisp).
6. `CreateIndex` + `IrIndex`: add
   - `include: Vec<String>` (`INCLUDE (…)`, default empty, skip-if-empty)
   - `with: Option<IndexStorageParams>` — typed, e.g. `{ pages_per_range: Option<u32>, fillfactor: Option<u32> }`
     rendered as `WITH (key='val', …)` in catalog order. Start with `pages_per_range`.
   - `only: Option<bool>` (`ON ONLY`) — needed so drift/round-trip can represent pg_dump's decomposed form
     if ever authored directly; the intent form does NOT set it.
   - `IndexMethod::Brin`
   (`where`/partial predicate already exists — reuse.)

### Render (`src/render/declarative.rs`)
- `PARTITION BY RANGE|LIST|HASH (cols)` appended to `CREATE TABLE`.
- `CREATE TABLE child PARTITION OF parent FOR VALUES FROM (…) TO (…)` / `FOR VALUES IN (…)` /
  `FOR VALUES WITH (MODULUS m, REMAINDER r)` / `DEFAULT`. Bound literals rendered to match pg_dump
  normalization (timestamptz → `'2026-05-01 00:00:00+00'`).
- `ALTER TABLE parent DETACH PARTITION name [CONCURRENTLY]`; `DROP TABLE name`.
- Index: `USING brin`, `INCLUDE (…)`, `WITH (…)`, `ON ONLY`.

### fold/snapshot (`src/render/fold.rs`, `src/model/snapshot.rs`)
- Snapshot records the parent as partitioned (`partition_by`) + a set of child partitions keyed by name
  (flat, matching the PG catalog shape) so a later-migration `CreatePartition`/`DropPartition` validates
  against real state (parent exists + is partitioned; child name unique; no range overlap best-effort).
- Parent indexes recorded once; child propagated indexes are NOT enumerated in the snapshot (they are a PG
  runtime consequence) — the `attached_to` relationship is reconstructed at drift time (see zeroship-schema).

### validate + decision (support matrix) — MUST be symmetric
- `Op::CreatePartition`, `DetachPartition`, `DropPartition` and the BRIN/INCLUDE/WITH/ONLY index features
  are **Vendor(PG)**. `Support::decision()` (dialect-only) and `validate()` (feature-gated) must agree —
  both reject on SQLite/MySQL. Guard against the `op_support_matrix` decision↔validate drift class that bit
  the FK slice: add a matrix test row per new op.

## Drift (crates/zeroship-schema/src/query.rs)
- Introspect partitioned parents (`relkind='p'`), children (`relispartition`, `pg_get_expr(relpartbound)`),
  and — critically — model the **index ATTACH relationship** (Atlas's `attached_to`): a child index that is
  attached to a parent index (via `pg_inherits` on the index relations) is **owned by the parent index**, not
  independent. Without this, drift false-positives on all 35 auto-propagated child indexes. Round-trip test:
  introspect a partitioned table with propagated indexes → **zero** spurious drift.
- Run `cargo test -p zeroship-schema` whenever `query.rs` is touched (separate crate, ~337 tests).

## JS surface (sdks/migrate + generated + frontend twin)
- `table(t).create({ …, partitionBy: p.range(["ts"]) })` — `p.range/list/hash`.
- `partition("sandbox_events_2026_05").of("sandbox_events").forValues({ from: ["'2026-05-01 00:00:00+00'"], to: ["'2026-06-01 00:00:00+00'"] })`
  ; `.asDefault()`; list/hash variants.
- `detachPartition(parent, name, { concurrently? })`; `dropPartition(name, { schema, ifExists?, cascade? })`.
- Index builder: `.using("brin")`, `.include([...])`, `.with({ pagesPerRange: 32 })`, `.only()`.
- **Lock-step twin:** every recorder change in `sdks/migrate/src/ops.ts` mirrored in
  `crates/zeroship-migrate/src/frontend/migrate_ops.js` (V8 recorder) — both edited together, both
  `include_str!`'d. Update `sdks/migrate/generated/{enums,ir}.ts` + `op-ir.schema.json`.

## Platform re-author
- `sandbox_tables.ts`: parent `create({ partitionBy })` + PK; 7 children as `partition().of().forValues()`
  + default; delete the 7 child `table().create()` column blocks + their ATTACH raws. (Leave the 9
  `'{}'::jsonb` default raws — deferred.)
- `constraints_indexes_fks.ts`: 4 parent index ops with brin/include/with/where; delete the 14 child-index
  raws, 35 ATTACH raws, and the redundant structural child `.index().add()` calls.

## Test plan (per feedback_regression_test_per_fix + feedback_faithful_e2e_tests)
1. `sdks/migrate/tests/ops.test.ts` — recorder emits the exact IR for partitionBy / CreatePartition /
   detach / drop / brin+include+with+only. Each a would-fail-pre-change assertion.
2. Render snapshot tests (DB-free) — DDL strings for every new op/feature.
3. `platform_ir_apply_pg.rs` `RunProfile::Platform` — apply intent form → `pg_dump` == baseline (0 diff).
   THE load-bearing faithfulness gate.
4. zeroship-schema drift round-trip — partitioned table + propagated indexes → 0 spurious drift.
5. `op_support_matrix` — one row per new op asserting decision↔validate agree, PG-only, SQLite/MySQL reject.

## Verify discipline
- FULL per-crate suites: `cargo test -p zeroship-migrate` (all targets, `--test-threads=1` for live-PG) +
  `cargo test -p zeroship-schema`. Never `--lib`-only (feedback_verify_full_suite_not_lib).
- Live PG = docker `appbase-migrate-postgres-1` on :5440. NEVER run concurrent live-PG suites
  (advisory-lock deadlock); `pkill -9` + `pg_terminate_backend zeroship%` before a fresh run.
- Build SDKs before cargo (`pnpm build`) — runtime `include_str!`s bootstrap/migrate dist.
- Commit-only, NEVER push. NEVER `git add -A` (seeded worktree) — `git add -u` + explicit new paths.

## Sub-partitioning (multi-level) — DEFERRED, extension point only

PG allows a partition to itself be `PARTITION BY` (an intermediate node is *both* a child, with bounds,
*and* a parent, with a strategy):
`CREATE TABLE m_2026 PARTITION OF m FOR VALUES FROM (…) TO (…) PARTITION BY RANGE (city_id)`.

The platform's `sandbox_events` is **single-level** (flat monthly children), so multi-level is **not built**
(YAGNI — no render/support path, no platform use). But the surface + IR are chosen so it's a zero-reshape
add later:
- **Surface:** `partition().of()` scales to arbitrary depth with a *stable* name for each node
  (`partition("m_2026").of("m")` to create it; `partition("child").of("m_2026")` to reference it as a
  parent). `table().partition()` would force each intermediate to be addressed two different ways
  (grandparent-anchored as a child vs own-name as a parent) — the decisive reason the surface is
  `partition().of()`.
- **IR extension point:** add one optional field `partition_by: Option<PartitionSpec>` to
  `Op::CreatePartition` (a partition that is itself partitioned). Serde-additive + skip-when-absent ⇒ addable
  later with **no breaking change**. Neither current op expresses an intermediate node (CreateTable = root
  parent only; CreatePartition = leaf only) — this field is the whole gap, recorded here so we don't reshape.

## Slice plan
- **S1 — engine IR + render + support-matrix** (partitionBy on CreateTable, CreatePartition, DetachPartition,
  DropPartition; brin/include/with/only on index). TDD render + matrix tests. No JS, no re-author.
- **S2 — JS surface + twin** (p.range, partition().of().forValues()/asDefault(), detach/drop, index
  .include/.with/.using(brin)/.only) + generated + ops.test.ts. Lock-step migrate_ops.js.
- **S3 — drift** (zeroship-schema query.rs attached_to + partition introspection) + round-trip test.
- **S4 — platform re-author + live-PG round-trip** (both .ts files → intent form; pg_dump 0-diff). The gate.
