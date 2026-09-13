"use server";

// Primary API — schema declarators and their public builder value types.
export { decimal, t, naming, schema, SchemaBuilder, TypeBuilder } from "./types";
export { Collection } from "./collection";
export { Query } from "./query";
export { eq, ne, gt, gte, lt, lte, and, or, not, isNull, count, sum, avg, min, max } from "./read";
export type { AliasedCollection, ReadBuilder, ReadColumn, ReadCondition, ReadAggregate, ReadRow } from "./read";
export {
  ValidationError,
  OptimisticLockError,
  NotFoundError,
  NotUniqueError,
  InvalidOperationError,
} from "./errors";
export { withRetry, isOptimisticLockError } from "./with-retry";
export type { WithRetryOptions } from "./with-retry";

export { defineMaskPolicy } from "./policy";
export type { MaskPolicy } from "./policy";

// Reactive queries.
export type { LiveQuery, LiveOptions } from "./live";
export type { NativeCollection } from "./native";

// Types — `Db`, `TxCollection`, `TxQuery`, `Collections`, `DbExtensions`,
// `TransactionOptions`, `SchemaInput` are user-facing. Bootstrap-only
// installation and normalization helpers live in @zeroship/bootstrap.
export type { Db, Collections, TransactionDb, DbExtensions, TxCollection, TxQuery, TransactionOptions, SchemaInput, SchemaShape, RowOf, RowInputOf } from "./db-types";
export type { PaginationResult } from "./query";
export type { Decimal, FieldDef, FieldStorage, FieldDefaultValue, PlainObject, Result, Row, RowInput, UpsertOptions, ColumnAssignment, UpdateExpression, Filter, SortableField, DistinctField, VectorField, GeoField, SortSpec, SortInput, SelectableField, SelectSpec, SelectInput, NamingStrategy, SchemaOptions, InferSchema, InferUnion, InferFieldDef, IsolationLevel, Id, IdValue, RowId, FkAction, RefOptions, InferRow, InferRowInput, InferId, MaskKind, Classification, MaskOpts, MaskedValueRepr, MaskedValue, Actor, NamedIndexSpec, RelationField, RelationName, ExactWithSpec, WithRelations, WithSpec } from "./types";
export type { NormalizedSchema } from "./schema";
export type { JsonValue } from "./types";
