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
  } else if (Object.entries(schema).some(([name, field]) => name !== "id" && field.primaryKey === true)) {
    message = "collection 'id' must be its sole primary key";
  }
  if (message) throw Object.assign(new Error(message), { code: "INVALID_COLLECTION_IDENTITY" });
}
