/**
 * Top-level `model()` factory for @zeroship/db.
 * Creates a Collection bound to the current app's native database driver.
 */
import { env } from "zeroship";
import { normalizeSchema } from "./schema.js";
import { Collection, NativeDb } from "./collection.js";
import { NamingStrategy, naming } from "./types.js";

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
 * await Users.create({ name: "Alice", email: "alice@example.com" });
 * ```
 */
export function model<S extends Record<string, unknown>>(
  name: string,
  schema: S,
  nativeOverride?: NativeDb,
  namingStrategy: NamingStrategy = naming.snakeCase,
  softDelete: boolean = false
): Collection<S> {
  if (typeof name !== "string" || name.trim().length === 0) {
    throw new Error("model name must be a non-empty string");
  }
  if (schema === null || schema === undefined || typeof schema !== "object") {
    throw new Error("model schema must be an object");
  }
  const normalized = normalizeSchema(schema as Parameters<typeof normalizeSchema>[0]);
  const native = nativeOverride ?? getNativeDb();

  // When soft delete is enabled, inject the deletedAt field into the schema
  if (softDelete && !normalized.deletedAt) {
    normalized.deletedAt = { type: "date", required: false };
  }

  // Register model with the runtime — creates table + columns if not exists.
  // registerModel returns a Promise. We store it so the Collection can await
  // it before the first CRUD operation, ensuring DDL completes first.
  // Convert schema keys to column names for DDL
  const dbSchema: ZeroshipDbSchema = {};
  for (const [key, def] of Object.entries(normalized)) {
    dbSchema[namingStrategy.toColumn(key)] = def as ZeroshipDbFieldDef;
  }

  let registrationPromise: Promise<void> | null = null;
  if (native.registerModel) {
    try {
      registrationPromise = native.registerModel(name, dbSchema) as Promise<void>;
    } catch {
      // Ignore in non-runtime environments (tests, SSR)
    }
  }

  return new Collection<S>(name, normalized, native, { naming: namingStrategy, ready: registrationPromise, softDelete });
}
