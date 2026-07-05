// The engine-embedded recorder bundle entry (DSL redesign S0.5).
//
// This module is compiled by `tsup` into ONE self-contained ESM artifact
// (`dist/embedded-recorder.js`) that the `zeroship-migrate` crate `include_str!`s
// as the `@zeroship/migrate` module inside its V8 recorder isolate
// (`crates/zeroship-migrate/src/frontend/embedding.rs`). It replaces the former
// hand-kept twin `crates/zeroship-migrate/src/frontend/migrate_ops.js`: the SDK
// recorder (`src/ops.ts` + `src/pg.ts`) and the engine-embedded recorder are now
// the SAME build output (design P7 — "one compiled recorder artifact").
//
// Why a dedicated entry (not the package `.` entry `index.ts`): the engine needs
// the FULL recorder surface — the internal recorder seam (`__begin`/`__drain`),
// the derived producer census (`opProducers`/`opProducerRegistry`), the
// value-position `cCase`/`cFn`/`cPg` namespaces, the `__pgDomain`/`__pgSequence` handles
// the `/pg` subpath shim re-aliases, AND the whole `pg.ts` vendor surface — all
// in ONE module (the `@zeroship/migrate/pg` shim in the engine re-exports from
// `@zeroship/migrate`). `index.ts` is the narrower npm public API. The export set
// below is exactly the set the deleted `migrate_ops.js` exposed.
//
// The bundle is self-contained except for `@zeroship/db` (kept EXTERNAL in
// tsup.config.ts): the engine module graph registers `@zeroship/db` as its own
// module (`ZEROSHIP_DB_DIST_JS`), so `import { TypeBuilder } from "@zeroship/db"`
// resolves there exactly as it does for the npm package.

export {
  // recorder seam (build-evaluator internal)
  __begin,
  __drain,
  // derived producer census (S0.3/S0.4)
  opProducers,
  opProducerRegistry,
  // core op producers + value factories
  table,
  view,
  partition,
  dropPartition,
  enumType,
  comment,
  check,
  and,
  or,
  not,
  membership,
  notMembership,
  lit,
  interval,
  nextval,
  p,
  minValue,
  maxValue,
  t,
  // value-position function namespaces
  cCase,
  cFn,
  cPg,
  // PG-only handles the `/pg` shim re-aliases to `domain`/`sequence`
  __pgDomain,
  __pgSequence,
  // the determinism lint (best-effort source scan)
  lintDeterminism,
} from "./ops.js";

export {
  schema,
  dropSchema,
  extension,
  dropExtension,
  role,
  alterRole,
  dropRole,
  dropOwnedBy,
  grant,
  revoke,
  createPolicy,
  dropPolicy,
  createFunction,
  dropFunction,
  raw,
} from "./pg.js";
