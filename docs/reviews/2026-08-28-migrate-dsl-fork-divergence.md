# The authoring DSL is two forks with two recorders

2026-08-28. Found while moving the platform schema off `zeroship-platform-migrate`
and onto the `zero-migrate` CLI. **Out of scope for that change and not acted on**;
recorded here so the next person does not rediscover it from the symptom.

## The finding

`@zeroship/migrate` (`sdks/migrate`, 159 KB of `src/ops.ts`) and `zero-migrate`
(`packages/zero-migrate`, 189 KB of `src/ops.ts`) are two forks of one authoring
DSL. They are not a package and its re-export: neither depends on the other
(`sdks/migrate/package.json` dependencies are `@zeroship/db` alone), and each
carries its own ambient recorder with its own `__begin`/`__drain` module-level
singleton.

A migration module records into whichever recorder its import specifier resolves
to. The host that drains is fixed: `packages/zero-migrate-cli` imports
`__begin`/`__drain` from `zero-migrate`'s own `ops.js`
(`packages/zero-migrate/src/internal/recorder.ts:31`). So an ops list authored
through `@zeroship/migrate` is drained by nobody, and the CLI sees an empty
migration rather than an error.

## Measured

`db/migrations-ts` (35 files) authored through the CLI's host recorder,
`buildEnvelope(mod, { irVersion: 1 })`, varying only what the bare specifier
`@zeroship/migrate` resolves to:

| `node_modules/@zeroship/migrate` resolves to | envelopes authored |
| --- | --- |
| `sdks/migrate` (the real package of that name) | **0 of 35** |
| `packages/zero-migrate` | **35 of 35** |

The failing arm reports `op authoring called outside an active migration
recorder; the table() handle may only be used synchronously inside up()/down()`
on every file. That message is accurate and misleading at once: the handle *was*
used synchronously inside the phase, just against the other fork's recorder.

## The fork has drifted, not just duplicated

Two divergences were load-bearing enough to refuse a migration outright, both
found by applying the corpus rather than by reading:

- **Index storage parameters.** `sdks/migrate/src/ops.ts:2736-2746` accepts
  `with: { pagesPerRange, fillfactor }` as a named pair.
  `packages/zero-migrate/src/ops.ts:3034-3037` has no `with` portable key, so
  `with:` falls through to the vendor-namespace flattener and becomes the
  attribute key `with.pagesPerRange`, which the IR refuses ("an attribute key may
  use only lowercase letters, digits and underscore in each part"). The current
  spelling is `postgres: { pages_per_range: 32 }`
  (`crates/zeroship-migrate-postgres/src/attribute.rs:108`).

- **Function language.** `sdks/migrate/src/generated/enums.ts` declared
  `FuncLanguage = "plpgsql" | "sql"`. The IR renamed that variant to `Procedural`
  and the schema has said `"const": "procedural"` since
  (`crates/zeroship-migrate/ir-envelope.schema.json:7021-7035`); `plpgsql` appears
  nowhere in `crates/zeroship-migrate-ir/src/ir.rs`. The closed set belongs to the
  IR and the rendered token to the backend, which is why PostgreSQL still emits
  `LANGUAGE plpgsql` - verified on a live apply, all 16 platform functions land
  with `pg_language.lanname = 'plpgsql'`.

Both were fixed in `db/migrations-ts` on 2026-08-28.

## Why the drift went unnoticed

`sdks/migrate`'s generated enum file could not be regenerated at all.
`sdks/migrate/scripts/gen-ir-types.mjs` drives a hardcoded `ENUM_DEFS` census, and
it still named `PgExtractField`, a def the schema no longer has - so the generator
threw `enum def PgExtractField missing from schema` before writing anything, and
the committed `enums.ts` was free to sit stale. The census was also short in the
other direction: the schema holds 33 closed string enums and the list named 29,
omitting `ColumnCollation`, `IrClassification`, `IrMaskKind` and `VectorMetric`.

`sdks/migrate/tests/ir-types-drift.test.ts` exists to catch exactly this and was
itself already red: **21 passed / 7 failed** before the generator was repaired,
**22 / 6** after. The one it gained is the regenerate-and-byte-compare subtest.
The remaining six are the test's own hardcoded enum censuses, stale the same way.

## What resolution would have to become

The platform corpus imports `@zeroship/migrate`. For the CLI to drain what it
records, either

1. the corpus imports `zero-migrate` directly - a specifier change across all 35
   files in `db/migrations-ts`, no other edit, since every name the corpus uses
   (`createFunction`, `currentSetting`, `grant`, `now`, `raw`, `revoke`, `role`,
   `schema`, `t`, `table`, `uuidV4`) is exported by `packages/zero-migrate/src/index.ts`; or
2. `@zeroship/migrate` becomes a thin re-export of `zero-migrate`, deleting the
   fork, so both specifiers reach one recorder.

(2) is the end state; (1) is what unblocks the CLI today. Until one of them
lands, a bare `@zeroship/migrate` must resolve to `packages/zero-migrate` for the
platform migrate path to work at all - today that is a `node_modules` symlink
created by hand, which means **a clean checkout cannot migrate the platform
schema**. That is the immediate consequence and the reason this note exists.

## What this note does not establish

Whether creator-facing migrations are affected. They travel a different path
(`crates/zeroship-migrated`) and were not measured here. The two divergences
above are the two the platform corpus happens to exercise; the forks are ~30 KB
apart in source and nothing here bounds what else differs.
