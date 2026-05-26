"use server";

// Primary API — schema declarators and their public builder value types.
export { t, naming, schema, SchemaBuilder, TypeBuilder } from "./types";
export { Collection } from "./collection";
export { Query } from "./query";
export {
  ValidationError,
  OptimisticLockError,
  NotFoundError,
  NotUniqueError,
  InvalidOperationError,
} from "./errors";
export { withRetry, isOptimisticLockError } from "./with-retry";
export type { WithRetryOptions } from "./with-retry";

// P5.5 PR 5 — defineMaskPolicy(): per-app actor-role → classifications map.
export { defineMaskPolicy } from "./policy";
export type { MaskPolicy } from "./policy";

// C1 / P8a — reactive queries (in-process broker)
export type { LiveQuery, LiveOptions } from "./live";

// Types — `Db`, `TxCollection`, `TxQuery`, `Collections`, `DbExtensions`,
// `TransactionOptions`, `SchemaInput` are user-facing (the shape of
// `env.db` users see in autocomplete; the parameter type of
// `db.transaction(tx => ...)`; etc). Stage 7 moved the runtime helpers
// (installSchema, model, normalizeSchema, ...) into @zeroship/bootstrap;
// the public type surface stayed here so user code keeps importing from
// @zeroship/db.
export type { Db, Collections, DbExtensions, TxCollection, TxQuery, TransactionOptions, SchemaInput, SchemaShape, RowOf, RowInputOf } from "./db-types";
export type { PaginationResult } from "./query";
export type { FieldDef, FieldDefaultValue, PlainObject, Result, Row, RowInput, SystemFields, UpdateExpression, Filter, NamingStrategy, SchemaOptions, InferSchema, InferUnion, InferFieldDef, IsolationLevel, Id, FkAction, RefOptions, InferRow, InferRowInput, InferId, MaskKind, Classification, MaskOpts, MaskedValueRepr, MaskedValue, Actor, NamedIndexSpec, WithRelations, WithSpec } from "./types";
export type { NormalizedSchema } from "./schema";
