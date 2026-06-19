# zeroship-migrate-js

The **optional JS/TS schema front-end** for the lean `zeroship-migrate`
engine — the analog of Atlas's HCL / ORM providers (design
`docs/proposals/2026-06-18-schema-authority-drizzle-model-design.md` §5.1).

Atlas's shape is *many schema front-ends → one internal representation → one
diff/migrate engine.* `zeroship-migrate` adopts the same shape with the
**descriptor IR** (`zeroship_migrate::declarative::CollectionDescriptor`) as
the internal representation. This crate is the **JS/TS front-end**: it
evaluates a creator `schema.js` (the `@zeroship/db` `t.*` DSL) and lowers it
to that IR, then drives the engine's declarative differ to emit a versioned
migration file. `zeroship-migrate generate --schema schema.js` is the
zeroship "the tool natively speaks the app's schema" superpower.

## Responsibility

- Evaluate an (already-bundled, self-contained) creator `schema.js` **inside
  zeroship-runtime's existing V8 sandbox** — the SAME isolate + WinterCG
  polyfills that run untrusted creator app code, so the schema runs under the
  same security model (no ambient Node, fs, or process). No second JS engine.
- Lower the `t.*`-built schema map to the engine's descriptor IR
  (`CollectionDescriptor[]`) via a pure adapter (`src/ir_adapter.js`):
  `TypeBuilder.toFieldDef()` → the IR JSON the differ deserializes. It does
  NOT run `installSchema` (welded to a native `env.db`) — the lowering is pure.
- `generate --schema`: eval → `desired_snapshot` → live introspection
  (`snapshot_schema`) → `DeclarativeAuthor::diff` → render a dbmate migration
  file. One self-contained tool, no separate Node/vite step.

## Lean-core purity (the §5.1 guardrail)

Evaluating `schema.js` needs a real JS engine (V8), which is heavy. The lean
public CLI (`zeroship-migrate migrate ./db/migrations`, a SEPARATE crate)
must NOT carry it. So V8 enters the migrate family **only here**:

- `cargo tree -p zeroship-migrate` → **no `v8`** (lean core, dbmate-style).
- `cargo tree -p zeroship-migrate-js` → `v8` via `zeroship-runtime` (expected).

There is **no dependency cycle**: `zeroship-runtime` depends on neither
`zeroship-migrate` nor `zeroship-schema`; this crate sits above all three.

## Important files

- `src/eval.rs` — `eval_schema_to_ir(schema_source, owner_app)`: builds a
  bare `Runtime`, reuses the runtime's `pub` seams (`init_v8` /
  `setup_globals` / `install_*` / `load_modules`) under `with_scope`, evals
  the module graph `[ir_adapter.js, __schema__.js, @zeroship/db, zeroship]`,
  and reads the IR JSON back off `globalThis.__zsSchemaIR`.
- `src/ir_adapter.js` — the JS IR boundary (the "provider" emitting the
  internal representation). Embedded via `include_str!`.
- `src/generate.rs` — `generate_migration(...)`: the full `generate` flow +
  `render_dbmate`.
- `src/bin/zeroship-migrate-js.rs` — the full-build CLI (`generate --schema`).

## Type story (Drizzle-style — already works, no codegen)

Types are inferred from `schema.ts` by `@zeroship/db`'s TypeScript generics
(`Row<S>` / `InferSchema<S>` / `InferInsertSchema<S>`), which derive the
row/insert types directly from the `t.*` field-record. This is a pure
compile-time derivation — `env.db.<collection>.find()` stays strongly typed
with **no separate codegen step** and independent of whether `default.schema`
is consumed at runtime (its removal is P5). Encrypted/masked/vector facets
reflect in the inferred types. **No gap to build this phase.**

## Known gaps (flagged for the pilot)

1. **No Rust-side TS transpiler.** The runtime's module loader compiles raw
   source straight to a V8 module — there is NO TS→JS transpile step in Rust
   (transpile + npm resolution live in the JS build pipeline, esbuild/Vite).
   So the input to this crate is a **self-contained `schema.js`** (bundled JS
   where `@zeroship/db` is resolvable — we provide the DSL in the graph). A
   raw `schema.ts` importing npm packages must be bundled upstream first. The
   spec's "reuses zeroship-runtime's V8 + TS toolchain" is half-true: the V8
   half is real and reused; the TS-toolchain half does not exist in Rust.
2. **zsenc/zsmask sentinels not emitted by the differ.** The IR faithfully
   carries the `encrypted`/`mask` facets, and the engine renders the column
   TYPE (`bytea`) + the `<col>_masked` sibling, but the snapshot-based
   `DeclarativeAuthor::diff` does NOT emit the inline `/* zsenc:... */` or
   `COMMENT ... __zsmask:...` sentinels (spec §4). The codec lives in
   `zeroship_schema::query`; carrying the sentinel through the differ's
   snapshot is an engine (P2) follow-up, not a front-end change.
