/**
 * Top-level `model()` factory for @appbase/db.
 * Creates a Collection bound to the current app's native database driver.
 */
import { normalizeSchema } from "./schema.js";
import { Collection, NativeDb } from "./collection.js";

/** Returns the native appbase.db driver from the global scope, or throws if unavailable. */
function getNativeDb(): NativeDb {
  if (typeof globalThis !== "undefined" && (globalThis as any).appbase?.db) {
    return (globalThis as any).appbase.db as NativeDb;
  }
  throw new Error("@appbase/db: native appbase.db.* not available — are you running inside appbase?");
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
export function model(
  name: string,
  schema: Record<string, unknown>,
  nativeOverride?: NativeDb
): Collection {
  if (typeof name !== "string" || name.trim().length === 0) {
    throw new Error("model name must be a non-empty string");
  }
  if (schema === null || schema === undefined || typeof schema !== "object") {
    throw new Error("model schema must be an object");
  }
  const normalized = normalizeSchema(schema);
  const native = nativeOverride ?? getNativeDb();
  return new Collection(name, normalized, native);
}
