/** Normalized collection descriptors and entity identity validation. */
import type { FieldDef } from "./types";

/** A normalized schema mapping field names to their FieldDef. */
export type NormalizedSchema = Record<string, FieldDef>;

/** Scalar convenience operations require a single declared key. */
export function scalarPrimaryKey(schema: NormalizedSchema): string {
  const keys = Object.keys(schema).filter(name => schema[name]?.primaryKey === true);
  if (keys.length !== 1) {
    throw Object.assign(new Error("use a filter containing the complete primary key"), {
      code: "COMPOSITE_KEY_FILTER_REQUIRED",
    });
  }
  return keys[0]!;
}

/** Validate entity identity before exposing a collection. */
export function validateCollectionIdentity(schema: NormalizedSchema): void {
  const invalid = (message: string): never => {
    throw Object.assign(new Error(message), { code: "INVALID_COLLECTION_IDENTITY" });
  };
  if (!schema || typeof schema !== "object" || Array.isArray(schema)) invalid("collection fields must be an object");
  const keys = Object.values(schema).filter(field => field?.primaryKey === true);
  if (!keys.length) invalid("collection requires a declared primary key");
  for (const key of keys) {
    if (key.required !== true) invalid("primary key columns must be required and non-null");
    if (key.assign !== undefined && key.assign?.on !== "insert") {
      invalid("primary key columns can only be assigned on insertion");
    }
    if (key.encrypted === true || (key.mask !== undefined && key.mask?.kind !== "none")) {
      invalid("primary key columns cannot be masked or encrypted");
    }
  }
}
