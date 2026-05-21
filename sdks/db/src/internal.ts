/**
 * Framework-internal subpath for `@zeroship/db`.
 *
 * Re-exports the SDK innards needed by `@zeroship/bootstrap` (the
 * coordination package the runtime crate + Vite plugin consume). User
 * code MUST NOT import from this subpath — symbols here have no
 * back-compat guarantee.
 *
 * The dependency direction is bootstrap → db: bootstrap owns
 * `installSchema`, the `__zsDispatch` dispatcher, and dev/runtime
 * entries, and needs read-only access to a handful of db internals
 * (Collection class, Query class, the live-query factory, naming
 * helpers, the type-builder classes used for `instanceof` checks, the
 * NormalizedSchema type). Splitting these behind `./internal` keeps the
 * main `@zeroship/db` entry user-facing while making the coupling
 * explicit at the import site.
 */
export { Collection } from "./collection.js";
export type { NativeDb, NativeCollection } from "./collection.js";
export { Query } from "./query.js";
export { createLive } from "./live.js";
export type { LiveOptions, LiveQuery } from "./live.js";
export { naming, SchemaBuilder, TypeBuilder, ok, err } from "./types.js";
export type {
  NamingStrategy,
  NamedIndexSpec,
  PlainObject,
  Result,
  Row,
  RowInput,
  UpdateExpression,
  Filter,
  IsolationLevel,
  WithSpec,
  WithRelations,
  FieldDef,
} from "./types.js";
export type { NormalizedSchema } from "./schema.js";
