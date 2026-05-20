/**
 * Top-level `model()` factory for @zeroship/db.
 * Creates a Collection bound to the current app's native database driver.
 */
import { env } from "zeroship";
import { normalizeSchema } from "./schema.js";
import { Collection, NativeDb } from "./collection.js";
import { NamingStrategy, naming, type NamedIndexSpec } from "./types.js";

/**
 * Resolve the native database driver off the runtime's composite `env`.
 *
 * Mirrors `getNativeDb()` in db.ts — see the comment there for full
 * context. Model-level callers of `model()` that pass `nativeOverride`
 * (tests, mocks) short-circuit this lookup and never invoke it.
 */
function getNativeDb(): NativeDb {
  const db = (env as { db?: NativeDb } | undefined)?.db;
  if (db) {
    return db;
  }
  throw new Error(
    "@zeroship/db: env.db not available — " +
    "is the DbPlugin registered on this runtime?"
  );
}

/**
 * Creates a Collection for the given collection name and schema.
 *
 * @param name - The collection name (e.g. `"users"`).
 * @param schema - A record of field definitions using `t.*` builders or Mongoose-style objects.
 * @param nativeOverride - Optional: inject a custom native driver (used in tests).
 * @returns A Collection instance ready for CRUD operations.
 *
 * @example
 * ```ts
 * const Users = model("users", {
 *   name: t.string().required(),
 *   email: t.string().required().unique(),
 * });
 * await Users.insert({ name: "Alice", email: "alice@example.com" });
 * ```
 */
export function model<S extends Record<string, unknown>>(
  name: string,
  schema: S,
  nativeOverride?: NativeDb,
  namingStrategy: NamingStrategy = naming.snakeCase,
  softDelete: boolean = false,
  versioning: boolean = false,
  /** @internal — `createDb` sets this so it can chain `registerModel`
   *  calls itself in dependency order. Standalone `model()` callers
   *  leave it false and get the eager fire-and-store behaviour. */
  skipRegister: boolean = false,
  /** @internal — named multi-column indexes declared via
   *  `schema(...).index(name, fields)`. The SDK maps each field through
   *  the naming strategy before passing to the native side. */
  declaredIndexes: readonly NamedIndexSpec[] = [],
): Collection<S> {
  if (typeof name !== "string" || name.trim().length === 0) {
    throw new Error("model name must be a non-empty string");
  }
  if (schema === null || schema === undefined || typeof schema !== "object") {
    throw new Error("model schema must be an object");
  }
  // C2 — a top-level `t.union(...)` is a valid schema. `normalizeSchema`
  // detects the TypeBuilder branch internally and expands the union into
  // flat columns; the call below works for both record-of-fields AND a
  // single `TypeBuilder<X>` of union type.
  const normalized = normalizeSchema(schema as Parameters<typeof normalizeSchema>[0]);
  const native = nativeOverride ?? getNativeDb();

  // When soft delete is enabled, inject the deletedAt field into the schema
  if (softDelete && !normalized.deletedAt) {
    normalized.deletedAt = { type: "date", required: false };
  }

  // D4 — when versioning is enabled, inject a `version` column into the
  // normalized schema so DDL emits an INTEGER NOT NULL DEFAULT 1 column.
  // The Collection's update path treats `version` in a filter as a CAS
  // check and auto-increments it on success.
  if (versioning && !normalized.version) {
    normalized.version = { type: "number", required: false, default: 1 };
  }

  // Register model with the runtime — creates table + columns if not exists.
  // registerModel returns a Promise. We store it so the Collection can await
  // it before the first CRUD operation, ensuring DDL completes first.
  // Convert schema keys to column names for DDL
  const dbSchema: ZeroshipDbSchema = {};
  for (const [key, def] of Object.entries(normalized)) {
    dbSchema[namingStrategy.toColumn(key)] = def as ZeroshipDbFieldDef;
  }

  const wireIndexes: ZeroshipDbNamedIndex[] = declaredIndexes.map((idx) => ({
    name: idx.name,
    fields: idx.fields.map((f) => namingStrategy.toColumn(f)),
    ...(idx.unique ? { unique: true } : {}),
  }));

  // Mirror db.ts:467-475 — call via `.call(native, ...)` so the v8_class
  // brand check sees the right receiver. The unbound-fn form drops `this`
  // and triggers "Illegal invocation"; see commit e564c010 for the sibling
  // fix in createDb. The narrow catch only swallows the synchronous
  // env-resolution path (covered by `getNativeDb`); a native rejection
  // becomes a rejected promise stored on the Collection and surfaces as
  // `result.error` on first CRUD, which is the correct signal.
  let registrationPromise: Promise<void> | null = null;
  if (!skipRegister && native.registerModel) {
    registrationPromise = (native.registerModel as unknown as (
      this: typeof native,
      collection: string,
      schema: ZeroshipDbSchema,
      indexes?: ZeroshipDbNamedIndex[],
    ) => Promise<void>).call(native, name, dbSchema, wireIndexes);
  }

  return new Collection<S>(name, normalized, native, {
    naming: namingStrategy,
    ready: registrationPromise,
    softDelete,
    versioning,
    indexes: declaredIndexes,
  });
}
