import type { NormalizedSchema } from "../schema.js";
import type { PlainObject } from "../types.js";

/**
 * **P5 PR 2** — operators allowed on a `deterministic`-encrypted column.
 * Only equality (`$eq`, `$in`) is sound on the ciphertext; range /
 * regex / LIKE require ordering or substring matching that
 * deterministic mode cannot provide. A query that mentions any other
 * operator on a deterministic-encrypted field is rejected at the SDK
 * boundary with `deterministic_encrypted_op_not_supported` so the
 * call never reaches Rust.
 *
 * Bare values (`{ ssn: "X" }`) are treated as `$eq` and accepted.
 */
const DETERMINISTIC_ENCRYPTED_OPS_ALLOWED: ReadonlySet<string> = new Set([
  "$eq",
  "$in",
]);

/**
 * **P5 PR 2** — walk a filter looking for keys that the schema marks
 * as `encrypted`. Refuse:
 *   - ANY use of a randomised-encrypted field
 *     (`randomised_encrypted_field_not_filterable`) — the ciphertext
 *     differs per write so no equality lookup can match.
 *   - Range / regex / LIKE on a deterministic-encrypted field
 *     (`deterministic_encrypted_op_not_supported`) — only `$eq`/`$in`
 *     are sound on the ciphertext.
 *
 * Recurses into `$and` / `$or` arms. The walker is intentionally
 * conservative — anything not on the allowed list is refused — so
 * future operator additions stay fail-closed for encrypted columns.
 */
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
    if (!def || def.encrypted === undefined) {
      continue;
    }
    const mode = def.encrypted.mode;
    if (mode === "randomised") {
      throw Object.assign(
        new Error(
          `filter on "${key}": randomised-encrypted columns cannot be filtered — ` +
            `the ciphertext differs per write so no equality lookup can match. ` +
            `Switch the column to { mode: "deterministic" } if you need lookup, ` +
            `or drop the filter clause.`,
        ),
        { code: "randomised_encrypted_field_not_filterable" as const },
      );
    }
    if (value !== null && typeof value === "object" && !Array.isArray(value)) {
      for (const op of Object.keys(value as PlainObject)) {
        if (
          op.startsWith("$") &&
          !DETERMINISTIC_ENCRYPTED_OPS_ALLOWED.has(op)
        ) {
          throw Object.assign(
            new Error(
              `filter on "${key}": deterministic-encrypted columns support only $eq and $in (got "${op}"). ` +
                `Range / regex / LIKE require ordering or substring matching that deterministic mode cannot provide.`,
            ),
            { code: "deterministic_encrypted_op_not_supported" as const },
          );
        }
      }
    }
  }
}
