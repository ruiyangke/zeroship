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
 * **P9 PR 1** — `Query.unique()` raised this when zero matches resolved
 * against the strict-exactly-one terminal. Mirrors `ValidationError` /
 * `OptimisticLockError`'s shape: a `.name` set for `instanceof` flow, a
 * stable `.code` string app code can branch on, plus the collection
 * name for log/diagnostic messages.
 *
 * Use `find(filter).first()` instead if a missing row is a normal
 * outcome — `.first()` returns `null`, never throws.
 */
export class NotFoundError extends Error {
  name = "NotFoundError";
  code = "expected_one_got_zero" as const;
  collection?: string;

  constructor(collection?: string) {
    super(
      collection
        ? `expected one row in ${collection}, found zero`
        : "expected one row, found zero",
    );
    this.collection = collection;
  }
}

/**
 * **P9 PR 1** — `Query.unique()` raised this when more than one row
 * resolved against the strict-exactly-one terminal. Includes the actual
 * count (capped at 2 — the query is `LIMIT 2`) so `e.count === 2` is
 * the canonical signal for "ambiguous match".
 *
 * Use `find(filter).first()` if the caller is happy with any single
 * matching row; `.unique()` is for "this filter must identify EXACTLY
 * one row" assertions (a unique-constraint enforced lookup).
 */
export class NotUniqueError extends Error {
  name = "NotUniqueError";
  code = "expected_one_got_many" as const;
  collection?: string;
  /** Number of rows the query observed (capped at 2 by the `LIMIT 2`
   *  the terminal applies). */
  count: number;

  constructor(count: number, collection?: string) {
    super(
      collection
        ? `expected one row in ${collection}, found ${count}+`
        : `expected one row, found ${count}+`,
    );
    this.collection = collection;
    this.count = count;
  }
}

/**
 * **P9 PR 1** — raised when a Query terminal is called in a state that
 * doesn't make sense. Today the only producer is `Query.last()` invoked
 * without a `.sort(...)` clause: "last" is meaningless without an
 * ordering, so the terminal refuses upfront rather than silently
 * returning whatever the storage layer hands back.
 */
export class InvalidOperationError extends Error {
  name = "InvalidOperationError";
  code: string;

  constructor(code: string, message: string) {
    super(message);
    this.code = code;
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
 * Fallback behaviour — for bare strings or Errors with no structured
 * `.code`, preserve the original Error when possible and otherwise wrap
 * the message in a plain `Error`. The SDK does not mint synthetic DB-
 * specific codes; native uniqueness violations already arrive as the
 * coded string `unique_violation`.
 */
export function mapNativeError(e: unknown): Error {
  // Already a coded Error from the native layer — pass through. Subtypes
  // we own (ValidationError, OptimisticLockError) also flow through this
  // branch because they carry `.code` as a string-or-number field.
  if (e instanceof Error && typeof (e as { code?: unknown }).code === "string") {
    return e;
  }
  const msg = e instanceof Error ? e.message : String(e);
  if (e instanceof Error) return e;
  return new Error(msg);
}
