/**
 * Internal DB SDK adapter used by runtime initialization and framework tests.
 * Creator code imports the public package and uses the installed env.db facade.
 */
export { Collection } from "./collection";
export { TRANSACTION_READ } from "./collection/crud";
export { captureNativeTransaction } from "./native";
export type {
  NativeDb,
  NativeCollection,
  NativeSubscriptionLike,
  NativeTransactionFn,
} from "./native";
export { Query } from "./query";
export type { PaginationResult } from "./query";
export type { TransactionDb, TxCollection, TxQuery, TransactionOptions } from "./db-types";
export { createLive } from "./live";
export type { LiveOptions, LiveQuery } from "./live";
export { subscribe } from "./subscribe";
export type { Subscription, SubscriptionEvent } from "./subscribe";
export {
  drainCollectionLoaders,
} from "./tx-state";
export type { TransactionStateCarrier } from "./tx-state";
export { naming, SchemaBuilder, TypeBuilder, ok, err } from "./types";
export { readFrom, scopeAliasedCollection } from "./read";
export type { ReadFrom, AliasedCollection } from "./read";
export type {
  NamingStrategy,
  Actor,
  NamedIndexSpec,
  PlainObject,
  Result,
  Row,
  RowId,
  RowInput,
  UpsertOptions,
  UpdateExpression,
  Filter,
  DistinctField,
  SelectInput,
  SortInput,
  SortSpec,
  IsolationLevel,
  WithSpec,
  WithRelations,
  FieldDef,
} from "./types";
export type { NormalizedSchema } from "./schema";
export { validateCollectionIdentity } from "./schema";

// Test-only hooks — exposed here (subpath, not public ./) so tests
// reaching into Collection's warning state hit the SAME module
// instance as the runtime CRUD path. Without the subpath, tests
// importing from `../src/test-hooks.js` would compile through tsx
// and observe a separate set of warning maps.
export {
  __zeroshipDbResetIndexWarnings,
  __zeroshipDbWarnedShapesSize,
} from "./collection";
export {
  __zeroshipDbResetAccShapeWarnings,
  __zeroshipDbWarnedAccShapesSize,
} from "./utils";

// Internal validation entry points — used by the c2-union tests which
// exercise the validator against synthetic schemas without going
// through Collection.{insert,update}. Same module-identity logic as
// the warning hooks above.
export { validateDoc, checkPartial } from "./validate";

// Aggregate-pipeline translator — used by warned-acc-shapes-cap to
// drive the dedup state the matching internal getter inspects.
export { translateAggregatePipeline } from "./utils";

// Mask-policy startup handoff consumed by the bootstrap package.
export { _flushPendingMaskPolicy, _peekPendingMaskPolicy } from "./policy";
export type { MaskPolicy } from "./policy";

export {
  installSchema, model, normalizeSchema, expandUnionToFlatColumns, validateRefTargets,
} from "./install-schema";
export type {
  RuntimeSchemaDescriptor, InstallSchemaOptions, SchemaInput, ValidateSchemaShape,
  Collections, DbExtensions, Db,
} from "./install-schema";
