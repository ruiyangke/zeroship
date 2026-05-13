"use server";

// Primary API
export { createDb } from "./db.js";
export { t, naming, schema } from "./types.js";
export { ValidationError } from "./errors.js";

// Types
export type { Db, TxCollection, TxQuery, TransactionOptions } from "./db.js";
export type { FieldDef, FieldDefaultValue, PlainObject, Result, Document, CreateInput, UpdateExpression, Filter, NamingStrategy, SchemaOptions, InferSchema, InferFieldDef, InferType, IsolationLevel, Id, FkAction, RefOptions } from "./types.js";
export type { CreateDbOptions } from "./db.js";
export type { NormalizedSchema } from "./schema.js";

// B2 — runtime helper for cross-table ref validation. Exported for
// tests; production paths invoke it from createDb at module-init time.
export { validateRefTargets } from "./schema.js";
