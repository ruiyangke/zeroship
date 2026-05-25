/**
 * Framework-internal subpath for `@zeroship/db`.
 *
 * Re-exports the SDK innards needed by `@zeroship/bootstrap` (the
 * coordination package the runtime crate + Vite plugin consume). User
 * code MUST NOT import from this subpath — symbols here have no
 * back-compat guarantee.
 *
 * The dependency direction is bootstrap → db: bootstrap owns
 * `installSchema`, the `__zsDispatch` dispatcher, and dev/runtime
 * entries, and needs read-only access to a handful of db internals
 * (Collection class, Query class, the live-query factory, naming
 * helpers, the type-builder classes used for `instanceof` checks, the
 * NormalizedSchema type). Splitting these behind `./internal` keeps the
 * main `@zeroship/db` entry user-facing while making the coupling
 * explicit at the import site.
 */
export { Collection } from "./collection.js";
export { captureNativeTransaction } from "./native.js";
export type {
  NativeDb,
  NativeCollection,
  NativeSubscriptionLike,
  NativeTransactionFn,
} from "./native.js";
export { Query } from "./query.js";
export { createLive } from "./live.js";
export type { LiveOptions, LiveQuery } from "./live.js";
export { subscribe } from "./subscribe.js";
export type { Subscription, SubscriptionEvent } from "./subscribe.js";
export {
  anyCollectionInTransaction,
  drainCollectionLoaders,
  enterTransactionScope,
  exitTransactionScope,
  readTransactionDepth,
} from "./tx-state.js";
export type { TransactionStateCarrier } from "./tx-state.js";
export { naming, SchemaBuilder, TypeBuilder, ok, err } from "./types.js";
export type {
  NamingStrategy,
  NamedIndexSpec,
  PlainObject,
  Result,
  Row,
  RowInput,
  UpdateExpression,
  Filter,
  IsolationLevel,
  WithSpec,
  WithRelations,
  FieldDef,
} from "./types.js";
export type { NormalizedSchema } from "./schema.js";

// Test-only hooks — exposed here (subpath, not public ./) so tests
// reaching into Collection's warning state hit the SAME module
// instance as the runtime CRUD path. Without the subpath, tests
// importing from `../src/test-hooks.js` would compile through tsx
// and observe a separate set of warning maps.
export {
  __zeroshipDbResetIndexWarnings,
  __zeroshipDbWarnedShapesSize,
} from "./collection.js";
export {
  __zeroshipDbResetAccShapeWarnings,
  __zeroshipDbWarnedAccShapesSize,
} from "./utils.js";

// Internal validation entry points — used by the c2-union tests which
// exercise the validator against synthetic schemas without going
// through Collection.{insert,update}. Same module-identity logic as
// the warning hooks above.
export { validateDoc, checkPartial } from "./validate.js";

// Aggregate-pipeline translator — used by warned-acc-shapes-cap to
// drive the dedup state the matching internal getter inspects.
export { translateAggregatePipeline } from "./utils.js";

// P5.5 PR 5 — defineMaskPolicy() pending-slot drain. The bootstrap
// runtime-entry calls `_flushPendingMaskPolicy()` once at app init and
// flushes the returned policy through `zeroship.db.setMaskPolicy`.
export { _flushPendingMaskPolicy, _peekPendingMaskPolicy } from "./policy.js";
export type { MaskPolicy } from "./policy.js";
