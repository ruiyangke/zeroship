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
 * Translates a raw native driver error message into a typed JS Error.
 * Unique/duplicate constraint violations are given `code: 11000` (MongoDB
 * convention) so callers can branch on error type without string matching.
 */
export function mapNativeError(msg: string): Error {
  const lower = msg.toLowerCase();
  if (lower.includes("unique") || lower.includes("duplicate")) {
    const err = new Error(msg, { cause: msg }) as Error & { code: number };
    err.code = 11000;
    return err;
  }
  return new Error(msg, { cause: msg });
}
