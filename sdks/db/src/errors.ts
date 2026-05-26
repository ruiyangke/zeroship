/**
 * Error types and native-error mapping for @zeroship/db.
 * Provides ValidationError for schema violations and mapNativeError for
 * translating raw native driver errors into typed JS errors.
 */

const CANONICAL_CODE_OVERRIDES = Object.freeze({
  fk_violation: "FOREIGN_KEY_VIOLATION",
  version_mismatch: "OPTIMISTIC_CONCURRENCY",
} satisfies Record<string, string>);

const VALIDATION_ERROR_BRAND = Symbol.for("@zeroship/db/ValidationError");
const OPTIMISTIC_LOCK_ERROR_BRAND = Symbol.for("@zeroship/db/OptimisticLockError");
const NOT_FOUND_ERROR_BRAND = Symbol.for("@zeroship/db/NotFoundError");
const NOT_UNIQUE_ERROR_BRAND = Symbol.for("@zeroship/db/NotUniqueError");
const INVALID_OPERATION_ERROR_BRAND = Symbol.for("@zeroship/db/InvalidOperationError");

function hasErrorBrand(value: unknown, brand: symbol, name: string): boolean {
  return Boolean(
    value &&
      typeof value === "object" &&
      (value as Record<PropertyKey, unknown>)[brand] === true &&
      (value as { name?: unknown }).name === name,
  );
}

export function canonicalErrorCode(code: string): string {
  const overrides = CANONICAL_CODE_OVERRIDES as Readonly<Record<string, string>>;
  const overridden = overrides[code] ?? code;
  if (/^[A-Z0-9_]+$/.test(overridden)) return overridden;
  return overridden
    .replace(/([a-z0-9])([A-Z])/g, "$1_$2")
    .replace(/[^A-Za-z0-9]+/g, "_")
    .replace(/^_+|_+$/g, "")
    .toUpperCase();
}

function stampCanonicalCode<T extends Error>(error: T, code: string): T {
  try {
    Object.defineProperty(error, "code", {
      value: code,
      enumerable: true,
      configurable: true,
      writable: true,
    });
    return error;
  } catch {
    const cloned = Object.assign(new Error(error.message), error, { code });
    cloned.name = error.name;
    if ("stack" in error && typeof error.stack === "string") {
      cloned.stack = error.stack;
    }
    return cloned as T;
  }
}

export function readCanonicalErrorCode(e: unknown): string | undefined {
  if (!(e instanceof Error)) return undefined;
  const code = (e as { code?: unknown }).code;
  return typeof code === "string" ? canonicalErrorCode(code) : undefined;
}

/** A single field-level validation failure with path and human-readable message. */
export interface FieldError {
  message: string;
  path: string;
}

/** Thrown when one or more document fields fail schema validation. */
export class ValidationError extends Error {
  name = "ValidationError";
  code = "VALIDATION" as const;
  readonly [VALIDATION_ERROR_BRAND] = true;
  errors: Record<string, FieldError>;

  static [Symbol.hasInstance](value: unknown): boolean {
    return hasErrorBrand(value, VALIDATION_ERROR_BRAND, "ValidationError");
  }

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
 * race). The error's `code` is `"OPTIMISTIC_CONCURRENCY"`; `expectedVersion`
 * carries the caller's N.
 *
 * **P7 PR 4** — the platform's UPDATE auto-bump path now surfaces the
 * same condition with its native error code. The runtime-typed error carries
 * `retryable: true` semantically (the hint advises re-read + retry). The
 * SDK's `update()` / `updateMany()` catch that native code and rethrow as
 * `OptimisticLockError` so app code can branch on the error class or on
 * `OPTIMISTIC_CONCURRENCY` — see [`mapOptimisticConcurrencyError`].
 */
export class OptimisticLockError extends Error {
  name = "OptimisticLockError";
  code = "OPTIMISTIC_CONCURRENCY" as const;
  readonly [OPTIMISTIC_LOCK_ERROR_BRAND] = true;
  expectedVersion: number;
  /** **P7 PR 4** — always `true` for this error class; advisory flag
   *  the SDK consumer can branch on (`if (e.retryable) retry()`).
   *  Mirrors the `retryable: true` semantics the Rust side carries
   *  in its `hint`. */
  retryable = true as const;

  static [Symbol.hasInstance](value: unknown): boolean {
    return hasErrorBrand(value, OPTIMISTIC_LOCK_ERROR_BRAND, "OptimisticLockError");
  }

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
 * optimistic-concurrency code. Used by `Collection.update()` /
 * `Collection.updateMany()` so the SDK contract surfaces a single
 * typed error class regardless of whether the failure came from the
 * SDK's pre-PR-4 null-result inference or the runtime's typed reject.
 *
 * Returns the original error unchanged for any other code; the caller then
 * handles it via the standard `mapNativeError` rail. The `expectedVersion`
 * defaults to `NaN`
 * when the SDK doesn't have the original CAS value in scope (the
 * runtime's message body carries it but parsing free-text would
 * be fragile — callers that need the value have it in their own
 * filter object).
 */
export function mapOptimisticConcurrencyError(
  e: unknown,
  collection: string,
  expectedVersion: number,
): Error {
  if (readCanonicalErrorCode(e) === "OPTIMISTIC_CONCURRENCY") {
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
  code = "NOT_FOUND" as const;
  readonly [NOT_FOUND_ERROR_BRAND] = true;
  collection?: string;

  static [Symbol.hasInstance](value: unknown): boolean {
    return hasErrorBrand(value, NOT_FOUND_ERROR_BRAND, "NotFoundError");
  }

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
  code = "NOT_UNIQUE" as const;
  readonly [NOT_UNIQUE_ERROR_BRAND] = true;
  collection?: string;
  /** Number of rows the query observed (capped at 2 by the `LIMIT 2`
   *  the terminal applies). */
  count: number;

  static [Symbol.hasInstance](value: unknown): boolean {
    return hasErrorBrand(value, NOT_UNIQUE_ERROR_BRAND, "NotUniqueError");
  }

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
  readonly [INVALID_OPERATION_ERROR_BRAND] = true;
  code: string;

  static [Symbol.hasInstance](value: unknown): boolean {
    return hasErrorBrand(value, INVALID_OPERATION_ERROR_BRAND, "InvalidOperationError");
  }

  constructor(code: string, message: string) {
    super(message);
    this.code = canonicalErrorCode(code);
  }
}

/**
 * @internal
 * Translates a caught value (native driver error or rejected promise) into a
 * typed JS Error.
 *
 * Preservation contract — the native side throws Error objects whose `.code`
 * is a structured string (e.g. `"MIGRATION_ALREADY_RUNNING"`,
 * `"UNIQUE_VIOLATION"`). Earlier code reconstructed a new Error from the
 * message alone, dropping `.code` along the way; this passes the original
 * Error through unchanged whenever it already carries a string `.code`.
 *
 * Fallback behaviour — for bare strings or Errors with no structured
 * `.code`, preserve the original Error when possible and otherwise wrap
 * the message in a plain `Error`. The SDK does not mint synthetic DB-
 * specific codes; native uniqueness violations already arrive as the
 * coded string `UNIQUE_VIOLATION`.
 */
export function mapNativeError(e: unknown): Error {
  // Already a coded Error from the native layer — pass through. Subtypes
  // we own (ValidationError, OptimisticLockError) also flow through this
  // branch because they carry `.code` as a string-or-number field.
  const canonicalCode = readCanonicalErrorCode(e);
  if (e instanceof Error && canonicalCode !== undefined) {
    return stampCanonicalCode(e, canonicalCode);
  }
  const msg = e instanceof Error ? e.message : String(e);
  if (e instanceof Error) return e;
  return new Error(msg);
}
