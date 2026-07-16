// The full-recorder bundle entry (DSL redesign S0.5).
//
// This module is compiled by `tsup` into ONE self-contained ESM artifact
// (`dist/embedded-recorder.js`) exposing the FULL recorder surface. The SDK's
// recorder-internal tests import it directly
// (`tests/{ops,sequences-exclusion,column-facets-lockstep}.test.ts`) to exercise
// producers against the recorder seam.
//
// Why a dedicated entry (not the package `.` entry `index.ts`): the tests need the
// FULL recorder surface — the internal recorder seam (`__begin`/`__drain`), the
// derived producer census (`opProducers`/`opProducerRegistry`), the value-position
// `cCase` helper, the internal `__pgDomain`/`__pgSequence` handles, AND the whole
// public Postgres vendor surface — all in ONE module. `index.ts` is the npm public
// API; this entry additionally exposes the recorder internals.
//
// The bundle is fully self-contained: the inlined db type-builder
// (`./db-types.ts`) is bundled in — there is no external db dependency.

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
  int64,
  decimal,
  byteValue,
  now,
  uuidV4,
  uuidV7,
  genRandomUuid,
  currentSetting,
  currentUser,
  interval,
  concatWs,
  countStar,
  nextval,
  minValue,
  maxValue,
  t,
  ids,
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
  // dialect() — expression legs AND op-level thunked legs
  dialect,
  // the determinism lint (best-effort source scan)
  lintDeterminism,
} from "./ops.js";
