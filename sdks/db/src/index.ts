"use server";

// Primary API — schema declarators
export { t, naming, schema, MaskedValue } from "./types.js";
export { ValidationError, OptimisticLockError } from "./errors.js";
export { withRetry, isOptimisticLockError } from "./with-retry.js";
export type { WithRetryOptions } from "./with-retry.js";

// C1 / P8a — reactive queries (in-process broker)
export { subscribe } from "./subscribe.js";
export type { Subscription, SubscriptionEvent } from "./subscribe.js";
export type { LiveQuery, LiveOptions } from "./live.js";

// Types — `Db`, `TxCollection`, `TxQuery`, `Collections`, `DbExtensions`,
// `TransactionOptions`, `SchemaInput` are user-facing (the shape of
// `env.db` users see in autocomplete; the parameter type of
// `db.transaction(tx => ...)`; etc). Stage 7 moved the runtime helpers
// (installSchema, model, normalizeSchema, ...) into @zeroship/bootstrap;
// the public type surface stayed here so user code keeps importing from
// @zeroship/db.
export type { Db, Collections, DbExtensions, TxCollection, TxQuery, TransactionOptions, SchemaInput } from "./db.js";
export type { FieldDef, FieldDefaultValue, PlainObject, Result, Row, RowInput, UpdateExpression, Filter, NamingStrategy, SchemaOptions, InferSchema, InferUnion, InferFieldDef, IsolationLevel, Id, FkAction, RefOptions, Infer, InferRow, InferRowInput, InferId, MaskKind, Classification, MaskOpts, MaskedValueRepr, Actor } from "./types.js";
export type { NormalizedSchema } from "./schema.js";
