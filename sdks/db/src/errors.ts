/**
 * Error types and native-error mapping for @zeroship/db.
 * Provides ValidationError for schema violations and mapNativeError for
 * translating raw native driver errors into typed JS errors.
 */

/** A single field-level validation failure with path and human-readable message. */
export interface FieldError {
  message: string;
  path: string;
}

/** Thrown when one or more document fields fail schema validation. */
export class ValidationError extends Error {
  name = "ValidationError";
  errors: Record<string, FieldError>;

  constructor(errors: Record<string, FieldError>) {
    const messages = Object.values(errors)
      .map((e) => e.message)
      .join(", ");
    super(`Validation failed: ${messages}`);
    this.errors = errors;
  }
}

/**
 * D4 — optimistic-concurrency CAS update failed. Raised when an
 * `updateOne`/`updateMany` call includes `{ version: N }` in the filter
 * but the stored `version` no longer matches N (another writer won the
 * race). The error's `code` is `"optimistic_lock_failure"` matching the
 * A2 error-code inventory; `expectedVersion` carries the caller's N.
 */
export class OptimisticLockError extends Error {
  name = "OptimisticLockError";
  code = "optimistic_lock_failure" as const;
  expectedVersion: number;

  constructor(expectedVersion: number, collection?: string) {
    super(
      `Optimistic concurrency failure on ${collection ?? "collection"}: ` +
      `expected version ${expectedVersion}, row was modified by another writer`,
    );
    this.expectedVersion = expectedVersion;
  }
}

/**
 * @internal
 * Translates a caught value (native driver error or rejected promise) into a
 * typed JS Error.
 *
 * Preservation contract — the native side throws Error objects whose `.code`
 * is a structured string (e.g. `"migration_already_running"`,
 * `"unique_violation"`). Earlier code reconstructed a new Error from the
 * message alone, dropping `.code` along the way; this passes the original
 * Error through unchanged whenever it already carries a string `.code`.
 *
 * Back-compat fallback — for bare strings or Errors with no `.code`, the
 * legacy substring match on "unique"/"duplicate" still tags MongoDB code
 * 11000 so existing callers branching on numeric code keep working.
 */
export function mapNativeError(e: unknown): Error {
  // Already a coded Error from the native layer — pass through. Subtypes
  // we own (ValidationError, OptimisticLockError) also flow through this
  // branch because they carry `.code` as a string-or-number field.
  if (e instanceof Error && typeof (e as { code?: unknown }).code === "string") {
    return e;
  }
  const msg = e instanceof Error ? e.message : String(e);
  const lower = msg.toLowerCase();
  if (lower.includes("unique") || lower.includes("duplicate")) {
    const err = new Error(msg, { cause: e instanceof Error ? e : undefined }) as Error & { code: number };
    err.code = 11000;
    return err;
  }
  if (e instanceof Error) return e;
  return new Error(msg);
}
