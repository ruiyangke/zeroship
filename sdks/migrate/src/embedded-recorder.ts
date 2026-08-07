// The engine-embedded recorder bundle entry (DSL redesign S0.5).
//
// This module is compiled by `tsup` into ONE self-contained ESM artifact
// (`dist/embedded-recorder.js`) — the bundle a V8 recorder isolate maps as the
// `@zeroship/migrate` module when it authors an IR envelope from a creator
// migration. It replaces a hand-kept twin that used to be maintained separately
// inside the engine's Rust recorder front-end: the SDK recorder (`src/ops.ts`)
// and the engine-embedded recorder are now the SAME build output (design P7 —
// "one compiled recorder artifact"), so there is no second copy to drift.
//
// Why a dedicated entry (not the package `.` entry `index.ts`): the engine needs
// the FULL recorder surface — the internal recorder seam (`__begin`/`__drain`),
// the derived producer census (`opProducers`/`opProducerRegistry`), the
// value-position `cCase` helper, the legacy internal `__pgDomain`/`__pgSequence`
// handles, AND the whole public Postgres vendor surface — all in ONE module.
// `index.ts` is the npm public API; this entry also exposes recorder internals
// required by the Rust build evaluator. The export set below is exactly the set
// the deleted `migrate_ops.js` exposed, plus the now-rooted vendor names.
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
  enumType,
  comment,
  check,
  lit,
  decimal,
  byteValue,
  now,
  uuidV4,
  uuidV7,
  currentSetting,
  currentUser,
  interval,
  concatWs,
  countStar,
  nextval,
  minValue,
  maxValue,
  t,
  // value-position case helper
  cCase,
  // internal PG handles retained for recorder artifact tests
  __pgDomain,
  __pgSequence,
  domain,
  schema,
  extension,
  role,
  sequence,
  dropOwnedBy,
  grant,
  revoke,
  createFunction,
  dropFunction,
  raw,
  // dialect() — expression legs AND op-level thunked legs (J1)
  dialect,
  // the determinism lint (best-effort source scan)
  lintDeterminism,
} from "./ops.js";
