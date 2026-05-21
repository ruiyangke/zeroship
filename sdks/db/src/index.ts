"use server";

// Primary API — schema declarators
export { t, naming, schema } from "./types.js";
export { ValidationError, OptimisticLockError } from "./errors.js";
export { withRetry, isOptimisticLockError } from "./with-retry.js";
export type { WithRetryOptions } from "./with-retry.js";

// C1 / P8a — reactive queries (in-process broker)
export { subscribe } from "./subscribe.js";
export type { Subscription, SubscriptionEvent } from "./subscribe.js";
export type { LiveQuery, LiveOptions } from "./live.js";

// Types
export type { Db, TxCollection, TxQuery, TransactionOptions } from "./db.js";
export type { FieldDef, FieldDefaultValue, PlainObject, Result, Row, RowInput, UpdateExpression, Filter, NamingStrategy, SchemaOptions, InferSchema, InferUnion, InferFieldDef, IsolationLevel, Id, FkAction, RefOptions, Infer, InferRow, InferRowInput, InferId } from "./types.js";
export type { NormalizedSchema } from "./schema.js";

// B2 — runtime helper for cross-table ref validation. `_installSchema`
// calls this at module-init time; the export is also used by tests that
// validate schemas directly without going through the install path.
export { validateRefTargets } from "./schema.js";

// Framework-internal — the synthetic SSR entry (`@zeroship/vite-plugin`)
// and dev-bootstrap call this to register schemas declared via
// `export default { schema }`. User code MUST NOT call this directly;
// declare the schema once and access collections through `env.db.<name>`.
// Kept in the main entry (single underscore prefix signals "framework
// internal, not screaming") so the SSR build doesn't have to know about
// any subpath export.
export { _installSchema } from "./db.js";

// Framework-internal typed-globals helpers (`getPlatformReady`,
// `setPlatformReady`, `getSchemaInit`, `setSchemaInit`,
// `INSTALL_SCHEMA_NAME`, `PLATFORM_READY_NAME`, `SCHEMA_INIT_NAME`)
// are NOT re-exported on the main entry — surfacing platform-ready
// mutators next to `t` / `schema` in autocomplete invites user code
// to clobber the auto-tx happens-before edge. Internal consumers
// (vite-plugin dev-bootstrap, rpc-registry) import them through the
// `@zeroship/db/internal` subpath instead.
