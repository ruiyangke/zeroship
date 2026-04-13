import { normalizeSchema } from "./schema.js";
import { Collection } from "./collection.js";

function getNativeDb(): any {
  if (typeof globalThis !== "undefined" && (globalThis as any).appbase?.db) {
    return (globalThis as any).appbase.db;
  }
  throw new Error("@appbase/db: native appbase.db.* not available — are you running inside appbase?");
}

export function model(
  name: string,
  schema: Record<string, unknown>,
  nativeOverride?: any
): Collection {
  const normalized = normalizeSchema(schema);
  const native = nativeOverride ?? getNativeDb();
  return new Collection(name, normalized, native);
}
