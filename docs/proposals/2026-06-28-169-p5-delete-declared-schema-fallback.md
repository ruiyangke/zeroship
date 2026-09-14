# #169 P5 — delete the `export default { schema }` fallback (migrations the SOLE schema source of truth)

> **Historical design snapshot.** Package paths and package boundaries below
> describe the 2026-06-28 tree. The live DSL is the single
> `@zeroship/migrate` package at `packages/zero-migrate/`.

Status: design (2026-06-28). The migration-first cutover is substantially done — when
`globalThis.__zsRuntimeDescriptor` (the migration fold's wire map, from `manifest.runtime_descriptor`) is
present it IS the schema source of truth (`sdks/bootstrap/src/runtime-entry.ts:77-105`);
`user.default.schema` is the TRANSITIONAL FALLBACK that P5 deletes. BLOCKER (runtime-entry.ts:90-100): the
descriptor is FIELD-ONLY; the declared schema is still threaded as `default.__zsDeclaredSchema` to recover
COLLECTION-LEVEL options (softDelete / versioning / indexes) the field-only descriptor can't encode. P5 must
FIRST extend the descriptor to encode collection options, THEN delete the fallback. Breaking, creator-facing
— acceptable pre-launch. Land as one coordinated cutover AFTER S1-S2 make the descriptor complete.

## Descriptor v1 (target shape)
```ts
type RuntimeSchemaDescriptor = {
  version: 1;
  collections: Record<string, {
    fields: Record<string, FieldDef>;
    options: { softDelete: boolean; versioning: boolean; strictness?: Strictness };
    indexes: Array<{ name: string; fields: string[]; unique?: boolean }>;
  }>;
};
```
Do NOT infer softDelete/versioning from physical columns (system columns exist for every table) — carry them
explicitly through the fold.

## Staged plan (ordered by dependency; verify each stage before the next)

**S1 (RISKIEST) — extend migration IR with collection runtime metadata.** Files: `sdks/migrate/src/types.ts`,
`crates/zeroship-migrate/src/model/ir.rs`, op-ir.schema.json, the former JS authoring adapter's `src/migrate_ops.js`,
`ir_adapter.js`. Add table runtime options `{ softDelete, versioning, strictness? }` + a metadata/alter op so
options can change after create; preserve explicit index tracking for runtime-visible plain indexes. Touches
canonical migration identity (checksum) → regenerate goldens + op-ir.schema.json + the platform db/migrations
if their checksums shift; verify the full migrate suite. Test: IR round-trip, checksum/golden updates, JS
front-end lowering proves `schema(...).softDelete().withVersioning().index(...)` survives to IR.

**S2 — collection descriptor v1 in fold + gen-types.** Files: `crates/zeroship-migrate/src/render/fold.rs`,
the former JS authoring adapter's `src/gen_types.rs`. Fold fields + runtime options + explicit runtime-visible
indexes into the descriptor. Test: gen-types fixture (soft delete + versioning + compound index);
schema.runtime.json golden; generated env.db.ts mirrors options.

**S3 — consume descriptor options in bootstrap; delete the declared carrier.** Files:
`sdks/bootstrap/src/{runtime-entry,install-schema,dev-entry}.ts`. `installSchema` reads descriptor
collections directly; remove `declaredSchemas`, `__zsDeclaredSchema`, and the descriptor-mode fallback to the
declared first-arg. Test: install-schema.test.ts + schema_init.rs — descriptor preserves
softDelete/versioning/indexes without a declared schema; missing descriptor no longer installs default.schema.

**S4 — Vite synthetic entry + dev flow → descriptor-only.** Files:
`packages/vite-plugin/src/{rpc-registry,dev-server,build,migrations}.ts`. Synthetic default exports
`{ fetch, rpc }` (no schema / __zsDeclaredSchema); dev injects/loads generated schema.runtime.json. Test:
rpc-registry.test.ts asserts no schema carrier; dev-server boot/hot-update regenerates descriptor.

**S5 — activate generated env.db.ts types; retire `@zeroship/db/env` schema-alias path.** Files:
`packages/db/env.d.ts`, create-app template tsconfig, docs. Generated `generated/zeroship/env.db.ts` enters app
tsconfig.include; remove the `@zeroship/db/env` + `zeroship-schema` alias from templates. Strong env.db typing
preserved, now from the fold. Test: template typecheck; gen-types --check; no duplicate Env.db augmentation.

**S6 — bundle/runtime/worker fallback → hard error, not silent degrade.** Files:
`crates/zeroship-bundle/src/manifest.rs`, `crates/zeroship-runtime/src/core/{init,runtime,state}.rs`,
`crates/zeroship-worker/src/{sync,handler}.rs`, `packages/vite-plugin/src/zship.ts`. Stop "falling back to default.schema";
a corrupt descriptor fails app load (hard boot error), not silent degrade; no-descriptor allowed only for
schema-less apps. Test: runtime parse-failure; worker descriptor-fetch failure; zship-with-migrations requires
descriptor.

**S7 — contract/docs/templates/builder cleanup.** Files: `docs/reference/{zeroship-standard,db,migrate-op-dsl,
vite-plugin,zship}.md`, `packages/create-zeroship-app/template`, `apps/zeroship-builder`. Remove `default.schema`
from the deploy contract; `schema.ts` is only optional migration-authoring input; builder emits migrations +
generated types, not `export default { schema }`. Test: doc grep for `default.schema` leaves only
historical/archive refs; create-app smoke; builder grep clean.

Riskiest: S1 (migration identity/checksum). Plan derived from read-only design pass btsjvjreb.
