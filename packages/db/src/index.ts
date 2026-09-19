"use server";

// Primary API — schema declarators and their public builder value types.
export { decimal, t, naming, schema, SchemaBuilder, TypeBuilder } from "./types";
export type { Collection } from "./db-types";
export type { Query } from "./db-types";
export { eq, ne, gt, gte, lt, lte, and, or, not, isNull, count, sum, avg, min, max } from "./read";
export type { ReadColumn, ReadCondition, ReadAggregate, ReadRow } from "./read";
export type { AliasedCollection, ReadBuilder, ReadFrom, CollectionOptions } from "./db-types";
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
export { subscribe } from "./subscribe";
export type { Subscription, SubscriptionEvent } from "./subscribe";

// Reactive queries.
export type { LiveQuery, LiveOptions } from "./db-types";
export type { NativeCollection } from "./native";

// Types — `Db`, `TxCollection`, `TxQuery`, `Collections`, `DbExtensions`,
// `TransactionOptions`, `SchemaInput` are user-facing. Host installation
// belongs to zeroship-data-v8 and is absent from this package surface.
export type { Db, Collections, TransactionDb, DbExtensions, TxCollection, TxQuery, TransactionOptions, SchemaInput, SchemaShape, RowOf, RowInputOf } from "./db-types";
export type { PaginationResult } from "./db-types";
export type { Decimal, FieldDef, FieldStorage, FieldDefaultValue, PlainObject, Result, Row, RowInput, UpsertOptions, ColumnAssignment, UpdateExpression, Filter, SortableField, DistinctField, VectorField, GeoField, SortSpec, SortInput, SelectableField, SelectSpec, SelectInput, NamingStrategy, SchemaOptions, InferSchema, InferUnion, InferFieldDef, IsolationLevel, Id, IdValue, RowId, FkAction, RefOptions, InferRow, InferRowInput, InferId, MaskKind, Classification, MaskOpts, MaskedValueRepr, MaskedValue, Actor, NamedIndexSpec, RelationField, RelationName, ExactWithSpec, WithRelations, WithSpec } from "./types";
export type { NormalizedSchema } from "./schema";
export type { JsonValue } from "./types";
