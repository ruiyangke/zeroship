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
 *
 * **P7 PR 4** — the platform's UPDATE auto-bump path now surfaces the
 * same condition with the typed code `"version_mismatch"` from the
 * Rust runtime (`DbError::version_mismatch`). The runtime-typed error
 * carries `retryable: true` semantically (the hint advises re-read +
 * retry). The SDK's `update()` / `updateMany()` catch the native
 * `version_mismatch` code and rethrow as `OptimisticLockError` so
 * existing app code that `instanceof OptimisticLockError`-checks
 * keeps working — see [`mapVersionMismatchError`].
 */
export class OptimisticLockError extends Error {
  name = "OptimisticLockError";
  code = "optimistic_lock_failure" as const;
  expectedVersion: number;
  /** **P7 PR 4** — always `true` for this error class; advisory flag
   *  the SDK consumer can branch on (`if (e.retryable) retry()`).
   *  Mirrors the `retryable: true` semantics the Rust-side
   *  `version_mismatch` carries in its `hint`. */
  retryable = true as const;

  constructor(expectedVersion: number, collection?: string) {
    super(
      `Optimistic concurrency failure on ${collection ?? "collection"}: ` +
      `expected version ${expectedVersion}, row was modified by another writer`,
    );
    this.expectedVersion = expectedVersion;
  }
}

/**
 * **P7 PR 4** — translate a caught error from the native UPDATE
 * dispatcher into an [`OptimisticLockError`] when it carries the
 * `version_mismatch` code. Used by `Collection.update()` /
 * `Collection.updateMany()` so the SDK contract surfaces a single
 * typed error class regardless of whether the failure came from the
 * SDK's pre-PR-4 null-result inference or the runtime's typed reject.
 *
 * Returns the original error unchanged for any code other than
 * `version_mismatch`; the caller then handles it via the standard
 * `mapNativeError` rail. The `expectedVersion` defaults to `NaN`
 * when the SDK doesn't have the original CAS value in scope (the
 * runtime's message body carries it but parsing free-text would
 * be fragile — callers that need the value have it in their own
 * filter object).
 */
export function mapVersionMismatchError(
  e: unknown,
  collection: string,
  expectedVersion: number,
): Error {
  if (
    e instanceof Error &&
    (e as { code?: unknown }).code === "version_mismatch"
  ) {
    return new OptimisticLockError(expectedVersion, collection);
  }
  return e instanceof Error ? e : new Error(String(e));
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
