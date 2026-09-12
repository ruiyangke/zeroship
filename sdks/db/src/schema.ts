/** Normalized collection descriptors and entity identity validation. */
import type { FieldDef } from "./types";

/** A normalized schema mapping field names to their FieldDef. */
export type NormalizedSchema = Record<string, FieldDef>;

/** Validate entity identity before exposing a collection. */
export function validateCollectionIdentity(schema: NormalizedSchema): void {
  const id = schema.id;
  let message: string | undefined;
  if (!id || id.primaryKey !== true) {
    message = "collection requires 'id' declared as its primary key";
  } else if (id.required !== true) {
    message = "collection 'id' must be required and non-null";
  } else if (!["string", "text", "id", "integer", "int", "bigint", "bigInt"].includes(id.type)) {
    message = "collection 'id' must use text or integer storage";
  } else if (id.encrypted === true || (id.mask !== undefined && id.mask.kind !== "none")) {
    message = "collection 'id' cannot be encrypted or masked";
  } else if (id.assign !== undefined && id.assign?.on !== "insert") {
    message = "collection 'id' can only be assigned on insertion";
  } else if (Object.entries(schema).some(([name, field]) => name !== "id" && field.primaryKey === true)) {
    message = "collection 'id' must be its sole primary key";
  }
  if (message) throw Object.assign(new Error(message), { code: "INVALID_COLLECTION_IDENTITY" });
}
