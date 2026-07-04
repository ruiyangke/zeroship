# DSL redesign — Phase 0 implementation plan (the generation substrate)

**Spec:** `docs/proposals/2026-07-04-dsl-surface-redesign-design.md` §6 Phase 0 + P1/P7.
**Branch:** `feat/migrate-dsl-redesign` (off main 96ba0aa7). Commit-only, never push.
**Gate for EVERY slice:** builds + tests green in `nix develop` on **all three dialects** — PG (:5440), SQLite (in-process), MySQL (:3307, `mysql://root:zeroship@127.0.0.1:3307/zeroship_e2e`). Render-level 3-dialect assertions are the fast gate; live-apply for apply-path changes. Full per-crate suite `--no-fail-fast` (never `--lib`-only), + `pnpm --filter @zeroship/migrate build && test`, + downstream (`zeroship-migrated`, `zeroship-schema-authority-e2e`). Regression test per behavior change.

## Current state (survey 2026-07-04)
- `op-ir.schema.json` is generated FROM Rust (`schemars::JsonSchema` derives); `emit_op_ir_schema` test emits it (`UPDATE_SCHEMA=1`).
- `sdks/migrate/scripts/gen-ir-types.mjs` generates TS enum types from the schema (closed string-enums only); recursive structural types hand-authored in `src/generated/ir.ts` with `ir-types-drift.test.ts`.
- Recorder twin: `sdks/migrate/src/ops.ts` (3045 L) ≡ `crates/zeroship-migrate/src/frontend/migrate_ops.js` (3045 L, `include_str!`'d by the engine).
- Dialect handling: hand-written match arms in `model/support.rs` + `render/{sql_preview,renderer,dml,fold,lower,declarative}.rs`.

## The inversion Phase 0 establishes
Today Rust `support.rs` is the source of dialect truth. The redesign makes a **generated dialect table** the single source that BOTH the Rust validator AND the TS surface + S10 walk consume. And ONE compiled recorder replaces the hand-kept twin, its producers minted through a single `defineOp` chokepoint from which the census is *derived* (never self-reported).

## Slices (dependency-ordered, each independently landable + 3-dialect-gated)

### S0.1 — Dialect-support metadata in the schema + generated dialect table (data only)
- Add per-op / per-node / per-option dialect-support + tier metadata to the schema source (extend the `schemars`-derived shape OR a sidecar `dialect-support.toml` that the generator reads — DECISION: sidecar keyed by op/node/option token, so it composes with the existing schemars flow without contorting the derives).
- Extend the generator to emit: (a) a Rust `dialect_table.rs` (const table: token → {PG,SQLite,MySQL} disposition ∈ {portable, transparent-degradable, vendor, unsupported}), (b) the TS `dialect-table.ts`.
- **No consumer switch yet** — pure additive generation. Gate: generated output diffs clean; the table round-trips the current hand-written support decisions (a test asserts generated-table == current `Support::decision()` for every op × dialect, proving faithfulness before any switch).

### S0.2 — `Support::decision()` + validate PG-only checks consume the generated table
- Switch `model/support.rs` `decision()` and the `check_pg_only_expr` family to read the generated `dialect_table`, deleting the hand-written match arms.
- Gate: `op_support_matrix` still green (decision()==validate invariant holds by construction now); full suite 3-dialect. This closes the "hand-written arms" debt for the op/expr dimension.

### S0.3 — `defineOp` chokepoint in the recorder (single source, twin still mirrored)
- Introduce `defineOp(kind, spec)` in a shared recorder module; every op producer in `ops.ts` is minted through it (records payload-slot writers for the tier-2 inventory). Mirror into `migrate_ops.js` byte-faithfully (twin not yet collapsed — that's S0.5).
- Export the tier-1 producer registry + tier-2 writer inventory (derived from the mint calls).
- Gate: `pnpm build && test`; recorder output byte-identical to pre-change for the golden corpus (proves the refactor is behaviour-preserving).

### S0.4 — Census + `.d.ts` lint + S10 walk + surface registry (the instrument)
- Build the census asserters (tier-1: one producer per op kind; tier-2: one writer per payload slot, registry-pairs excepted), the `.d.ts` declaration lint, the S10 core-export walk (registry-backed classification of value factories `t`/`lit`/`decimal`/`byteValue`/`minValue`/`maxValue`/`dialect`/`fromDb`), the surface-registry file, the ratchet script + baseline.
- **Exit-gate proof (the design's Phase-0 gate):** run the census against the CURRENT shipped surface — its known duplications (`addCheck`/`check().add`, `addForeignKey`/`foreignKey().add`, `t.int`/`t.integer`, index chain+bag, two module shapes) MUST show up as census FAILURES. That proves the instrument works before any surgery. (These failures are expected/red until Phase 2 deletes the duplications.)

### S0.5 — Collapse the recorder twin into one compiled artifact
- Make the build emit ONE recorder artifact both the SDK and the engine consume (engine `include_str!`s the compiled output instead of the hand-kept `migrate_ops.js`); delete `migrate_ops.js`.
- Gate: the one-release parity tripwire (compiled artifact == prior twin behaviour on the golden corpus) → then artifact-identity assertion; full 3-dialect suite + engine build.

## Exit gate for Phase 0
Generated output diffs clean; the generator emits Rust + TS dialect tables consumed by both sides; ONE recorder artifact (twin deleted); the census/lint/S10/ratchet gates stand and correctly flag the shipped surface's duplications as failures. Then Phase 1 (wire reshape-in-place) begins.

## Notes
- S0.1/S0.2 are the safest first slices (additive generation + a faithfulness-proven consumer switch) and unblock the dialect dimension; S0.3–S0.5 are the recorder surgery.
- Each slice = a bounded, well-briefed implementation task (opus agent), verified by me in nix 3-dialect, committed explicitly (never `git add -A`; never stage `db/migrations-ts` or `dist`).
