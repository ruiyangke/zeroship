"use server";

// Primary API
export { createDb } from "./db.js";
export { t } from "./types.js";
export { ValidationError } from "./errors.js";

// Types
export type { Db, TxCollection } from "./db.js";
export type { FieldDef, PlainObject, Result, Document, CreateInput, UpdateInput, InferSchema, InferFieldDef, InferType } from "./types.js";
export type { NormalizedSchema } from "./schema.js";
