# DEFERRABLE foreign keys — design (DSL v2)

**Status:** approved for build 2026-07-04 ("impl the deferrable"). Clears the platform's last
convertible raw (`usage_aggregates_metric_fkey`), raw 2→1, and gives creators deferrable FK
constraints (`DEFERRABLE INITIALLY DEFERRED` / `DEFERRABLE`).

## What's already there (so scope is small)

- The render seam already emits the clause: `fk_definition_for_dialect(...)` takes a
  `deferrable: bool` and appends `DEFERRABLE INITIALLY DEFERRED` on PG/SQLite, omits on MySQL
  (`declarative.rs`). The inline column-FK path (`FkField`) already threads `f.deferrable`.
- The gap is ONLY: (1) `IrConstraintKind::Fk` carries no deferrable fields, so the
  **constraint-level** FK (`addForeignKey` / table-level FK) can't express it; (2) the
  constraint-level snapshot builder `ir_fk_constraint_snapshot_for_columns` hardcodes `false`;
  (3) the `.ts` surface + V8 twin don't accept the options.

## Design — two additive optional bools on the Fk constraint

```rust
IrConstraintKind::Fk {
    columns, references_table, references_columns, on_delete, on_update,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    deferrable: Option<bool>,          // Some(true) => DEFERRABLE
    #[serde(default, skip_serializing_if = "Option::is_none")]
    initially_deferred: Option<bool>,  // Some(true) => INITIALLY DEFERRED (only when deferrable)
}
```

Additive (`skip_serializing_if`), so a FK WITHOUT these fields serializes byte-identically to
today and needs NO `CURRENT_IR_VERSION` bump. Mirrors the existing `Exclusion` variant, which
already carries the same two fields.

## Render — extend the existing seam

`fk_definition_for_dialect` gains an `initially_deferred: bool` param and renders (PG/SQLite; the
whole clause is omitted on MySQL, matching InnoDB's non-deferrable semantics — the same graceful
mapping the function already applies):

- `deferrable && initially_deferred` → ` DEFERRABLE INITIALLY DEFERRED` (the platform's shape,
  and what `pg_get_constraintdef` emits for a deferred FK — so the fold body matches the catalog).
- `deferrable && !initially_deferred` → ` DEFERRABLE` (== `INITIALLY IMMEDIATE`; `pg_get_constraintdef`
  renders bare `DEFERRABLE`).
- `!deferrable` → nothing.

`ir_fk_constraint_snapshot_for_columns` gains `deferrable: bool, initially_deferred: bool` params
threaded from the four `IrConstraintKind::Fk` destructure sites (fold CreateTable + fold
AddConstraint + lower CreateTable + lower AddConstraint). The name-derivation caller
(`ir_constraint_name_and_kind`) passes `false, false` — the FK NAME is deferrable-independent.

## Dialect gating — NONE new

DEFERRABLE FK is valid on PG and SQLite; on MySQL the clause is omitted (InnoDB FKs are always
effectively immediate — same class as RESTRICT≡NO ACTION folding). Because render already handles
all three dialects gracefully, NO new `Feature`/`Capability` gate is added, so `Support::decision()`
(dialect-only) stays consistent with `validate()` (the `op_support_matrix` invariant). The platform's
`usage_aggregates_metric_fkey` references a non-id column (`metric`), so it is ALREADY PG-only via
the existing NonIdForeignKey gate — deferrable rides on top with no extra gating.

## Validate — internal consistency only

`initially_deferred == Some(true)` requires `deferrable == Some(true)` (reject
`OP_INVALID: initiallyDeferred requires deferrable` otherwise). No dialect rejection.

## Drift — NONE needed

`ConstraintSnapshot` has full `PartialEq` on `definition`; the deferrable clause is part of the
`pg_get_constraintdef` `definition` string, so a deferrable FK is drift-compared correctly by the
existing constraint-snapshot machinery. The fold builds the SAME `DEFERRABLE INITIALLY DEFERRED`
body PG's catalog reports, so no phantom drift.

## Surface

```js
table("usage_aggregates", { schema: "zeroship" }).addForeignKey("usage_aggregates_metric_fkey", {
  columns: ["metric"],
  references: { table: "billing_metrics", columns: ["metric"], schema: "zeroship" },
  deferrable: true, initiallyDeferred: true,
});
```

`fkConstraintFromSpec` (in `ops.ts` + lock-step `migrate_ops.js`) emits `deferrable` /
`initiallyDeferred` into the `kind` object via `compact()` (absent when undefined). Add the two
optional fields to the FK arg interfaces in `types.ts` (`addForeignKey` + `foreignKey().add`).

## Tests (TDD)

1. Render round-trip: an `addForeignKey` with `deferrable:true, initiallyDeferred:true` renders a
   constraint `definition` ending `DEFERRABLE INITIALLY DEFERRED`; `deferrable:true` alone →
   bare `DEFERRABLE`; neither → no clause. Assert the exact `pg_get_constraintdef` golden on live PG.
2. Validate: `initiallyDeferred:true` without `deferrable` rejected `OP_INVALID`.
3. Serde: `Fk` with the fields round-trips; a FK WITHOUT them is byte-identical to today (no
   `CURRENT_IR_VERSION` bump). Regen `op-ir.schema.json` (UPDATE_SCHEMA=1).
4. Recorder (`ops.test.ts`): `.addForeignKey(..., { deferrable:true, initiallyDeferred:true })`
   emits `kind:{ ..., deferrable:true, initiallyDeferred:true }`; omitted when unset; public-vs-engine parity.

## Verify

- Full `nix develop -c cargo test -p zeroship-migrate --tests --no-fail-fast --features standalone-cli`
  + `pnpm --filter @zeroship/migrate build && test`. Downstream build (`zeroship-migrated`,
  `zeroship-schema-authority-e2e`) clean. Regen op-ir.schema.json / snapshot goldens / corpus as needed.
- Re-author `usage_aggregates_metric_fkey` → structural `addForeignKey(..., deferrable:true,
  initiallyDeferred:true)` + delete the raw → pg_dump differential = 0 → raw 2→1.

## Slices
- **A — engine + surface + tests** (Fk fields + render + validate + snapshot-builder thread + recorder twin). Verify.
- **B — re-author `usage_aggregates_metric_fkey`** + pg_dump differential. raw 2→1.
