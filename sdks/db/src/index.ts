"use server";

// Primary API
export { createDb } from "./db.js";
export { t, naming, schema } from "./types.js";
export { ValidationError, OptimisticLockError } from "./errors.js";

// C1 / P8a — reactive queries (in-process broker)
export { subscribe } from "./subscribe.js";
export type { Subscription, SubscriptionEvent } from "./subscribe.js";

// Types
export type { Db, TxCollection, TxQuery, TransactionOptions } from "./db.js";
export type { FieldDef, FieldDefaultValue, PlainObject, Result, Row, RowInput, UpdateExpression, Filter, NamingStrategy, SchemaOptions, InferSchema, InferUnion, InferFieldDef, IsolationLevel, Id, FkAction, RefOptions } from "./types.js";
export type { CreateDbOptions } from "./db.js";
export type { NormalizedSchema } from "./schema.js";

// B2 — runtime helper for cross-table ref validation. Exported for
// tests; production paths invoke it from createDb at module-init time.
export { validateRefTargets } from "./schema.js";
