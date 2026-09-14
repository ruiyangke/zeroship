/**
 * Test-only access to the DB SDK implementation and crate-owned facade adapter.
 * This file is not part of the production adapter bundle or an npm export.
 */
export { Collection } from "../../../packages/db/src/collection";
export { TRANSACTION_READ } from "../../../packages/db/src/collection/crud";
export { captureNativeTransaction } from "../../../packages/db/src/native";
export type {
  NativeDb,
  NativeCollection,
  NativeSubscriptionLike,
  NativeTransactionFn,
} from "../../../packages/db/src/native";
export { Query } from "../../../packages/db/src/query";
export type { PaginationResult } from "../../../packages/db/src/query";
export type { TransactionDb, TxCollection, TxQuery, TransactionOptions } from "../../../packages/db/src/db-types";
export { createLive } from "../../../packages/db/src/live";
export type { LiveOptions, LiveQuery } from "../../../packages/db/src/live";
export { subscribe } from "../../../packages/db/src/subscribe";
export type { Subscription, SubscriptionEvent } from "../../../packages/db/src/subscribe";
export {
  drainCollectionLoaders,
} from "../../../packages/db/src/tx-state";
export type { TransactionStateCarrier } from "../../../packages/db/src/tx-state";
export { naming, SchemaBuilder, TypeBuilder, ok, err } from "../../../packages/db/src/types";
export { readFrom, scopeAliasedCollection } from "../../../packages/db/src/read";
export type { ReadFrom, AliasedCollection } from "../../../packages/db/src/read";
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
} from "../../../packages/db/src/types";
export type { NormalizedSchema } from "../../../packages/db/src/schema";
export { validateCollectionIdentity } from "../../../packages/db/src/schema";

// Test-only hooks — collected here so tests
// reaching into Collection's warning state hit the SAME module
// instance as the runtime CRUD path.
export {
  __zeroshipDbResetIndexWarnings,
  __zeroshipDbWarnedShapesSize,
} from "../../../packages/db/src/collection";
export {
  __zeroshipDbResetAccShapeWarnings,
  __zeroshipDbWarnedAccShapesSize,
} from "../../../packages/db/src/utils";

// Internal validation entry points — used by the c2-union tests which
// exercise the validator against synthetic schemas without going
// through Collection.{insert,update}. Same module-identity logic as
// the warning hooks above.
export { validateDoc, checkPartial } from "../../../packages/db/src/validate";

// Aggregate-pipeline translator — used by warned-acc-shapes-cap to
// drive the dedup state the matching test hook inspects.
export { translateAggregatePipeline } from "../../../packages/db/src/utils";

export type { MaskPolicy } from "../../../packages/db/src/policy";

export {
  installSchema, model, normalizeSchema, expandUnionToFlatColumns, validateRefTargets,
} from "./install-schema";
export type {
  RuntimeSchemaDescriptor, InstallSchemaOptions, SchemaInput, ValidateSchemaShape,
  Collections, DbExtensions, Db,
} from "./install-schema";
