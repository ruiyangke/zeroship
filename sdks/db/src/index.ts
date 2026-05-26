"use server";

// Primary API — schema declarators and their public builder value types.
export { t, naming, schema, SchemaBuilder, TypeBuilder } from "./types.js";
export { Collection } from "./collection.js";
export { Query } from "./query.js";
export {
  ValidationError,
  OptimisticLockError,
  NotFoundError,
  NotUniqueError,
  InvalidOperationError,
} from "./errors.js";
export { withRetry, isOptimisticLockError } from "./with-retry.js";
export type { WithRetryOptions } from "./with-retry.js";

// P5.5 PR 5 — defineMaskPolicy(): per-app actor-role → classifications map.
export { defineMaskPolicy } from "./policy.js";
export type { MaskPolicy } from "./policy.js";

// C1 / P8a — reactive queries (in-process broker)
export type { LiveQuery, LiveOptions } from "./live.js";

// Types — `Db`, `TxCollection`, `TxQuery`, `Collections`, `DbExtensions`,
// `TransactionOptions`, `SchemaInput` are user-facing (the shape of
// `env.db` users see in autocomplete; the parameter type of
// `db.transaction(tx => ...)`; etc). Stage 7 moved the runtime helpers
// (installSchema, model, normalizeSchema, ...) into @zeroship/bootstrap;
// the public type surface stayed here so user code keeps importing from
// @zeroship/db.
export type { Db, Collections, DbExtensions, TxCollection, TxQuery, TransactionOptions, SchemaInput, SchemaShape, RowOf, RowInputOf } from "./db-types.js";
export type { PaginationResult } from "./query.js";
export type { FieldDef, FieldDefaultValue, PlainObject, Result, Row, RowInput, SystemFields, UpdateExpression, Filter, NamingStrategy, SchemaOptions, InferSchema, InferUnion, InferFieldDef, IsolationLevel, Id, FkAction, RefOptions, InferRow, InferRowInput, InferId, MaskKind, Classification, MaskOpts, MaskedValueRepr, MaskedValue, Actor, NamedIndexSpec, WithRelations, WithSpec } from "./types.js";
export type { NormalizedSchema } from "./schema.js";
