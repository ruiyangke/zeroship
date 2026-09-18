import type { NormalizedSchema } from "../../../../packages/db/src/schema";
import type { PlainObject } from "../../../../packages/db/src/types";

/** Encrypted values cannot participate in predicates, including nested filters. */
export function validateEncryptedFieldsInFilter(
  filter: PlainObject | undefined,
  schema: NormalizedSchema,
): void {
  if (filter === null || filter === undefined) return;
  if (typeof filter !== "object" || Array.isArray(filter)) return;

  for (const [key, value] of Object.entries(filter)) {
    if (key === "$and" || key === "$or") {
      if (Array.isArray(value)) {
        for (const arm of value) {
          validateEncryptedFieldsInFilter(arm as PlainObject, schema);
        }
      }
      continue;
    }
    if (key === "$not") {
      validateEncryptedFieldsInFilter(value as PlainObject, schema);
      continue;
    }
    if (key.startsWith("$")) {
      continue;
    }
    const def = schema[key];
    if (!def || def.encrypted !== true) {
      continue;
    }
    throw Object.assign(
      new Error(`filter on "${key}": encrypted fields cannot be filtered`),
      { code: "ENCRYPTED_FIELD_NOT_FILTERABLE" as const },
    );
  }
}
