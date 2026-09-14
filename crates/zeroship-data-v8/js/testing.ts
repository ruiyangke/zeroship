/**
 * Test-only access to the DB SDK implementation and crate-owned facade adapter.
 * This file is not part of the production adapter bundle or an npm export.
 */
export { Collection } from "../../../sdks/db/src/collection";
export { TRANSACTION_READ } from "../../../sdks/db/src/collection/crud";
export { captureNativeTransaction } from "../../../sdks/db/src/native";
export type {
  NativeDb,
  NativeCollection,
  NativeSubscriptionLike,
  NativeTransactionFn,
} from "../../../sdks/db/src/native";
export { Query } from "../../../sdks/db/src/query";
export type { PaginationResult } from "../../../sdks/db/src/query";
export type { TransactionDb, TxCollection, TxQuery, TransactionOptions } from "../../../sdks/db/src/db-types";
export { createLive } from "../../../sdks/db/src/live";
export type { LiveOptions, LiveQuery } from "../../../sdks/db/src/live";
export { subscribe } from "../../../sdks/db/src/subscribe";
export type { Subscription, SubscriptionEvent } from "../../../sdks/db/src/subscribe";
export {
  drainCollectionLoaders,
} from "../../../sdks/db/src/tx-state";
export type { TransactionStateCarrier } from "../../../sdks/db/src/tx-state";
export { naming, SchemaBuilder, TypeBuilder, ok, err } from "../../../sdks/db/src/types";
export { readFrom, scopeAliasedCollection } from "../../../sdks/db/src/read";
export type { ReadFrom, AliasedCollection } from "../../../sdks/db/src/read";
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
} from "../../../sdks/db/src/types";
export type { NormalizedSchema } from "../../../sdks/db/src/schema";
export { validateCollectionIdentity } from "../../../sdks/db/src/schema";

// Test-only hooks — collected here so tests
// reaching into Collection's warning state hit the SAME module
// instance as the runtime CRUD path.
export {
  __zeroshipDbResetIndexWarnings,
  __zeroshipDbWarnedShapesSize,
} from "../../../sdks/db/src/collection";
export {
  __zeroshipDbResetAccShapeWarnings,
  __zeroshipDbWarnedAccShapesSize,
} from "../../../sdks/db/src/utils";

// Internal validation entry points — used by the c2-union tests which
// exercise the validator against synthetic schemas without going
// through Collection.{insert,update}. Same module-identity logic as
// the warning hooks above.
export { validateDoc, checkPartial } from "../../../sdks/db/src/validate";

// Aggregate-pipeline translator — used by warned-acc-shapes-cap to
// drive the dedup state the matching test hook inspects.
export { translateAggregatePipeline } from "../../../sdks/db/src/utils";

export type { MaskPolicy } from "../../../sdks/db/src/policy";

export {
  installSchema, model, normalizeSchema, expandUnionToFlatColumns, validateRefTargets,
} from "./install-schema";
export type {
  RuntimeSchemaDescriptor, InstallSchemaOptions, SchemaInput, ValidateSchemaShape,
  Collections, DbExtensions, Db,
} from "./install-schema";
