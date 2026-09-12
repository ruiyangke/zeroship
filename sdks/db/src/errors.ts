/**
 * Error types and native-error mapping for @zeroship/db.
 * Provides ValidationError for schema violations and mapNativeError for
 * translating raw native driver errors into typed JS errors.
 */

const CANONICAL_CODE_OVERRIDES = Object.freeze({
  fk_violation: "FOREIGN_KEY_VIOLATION",
  concurrency_mismatch: "OPTIMISTIC_CONCURRENCY",
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

export interface ConcurrencyExpectation {
  column: string;
  expected: number;
}

/** Raised when a descriptor-declared compare-and-swap guard no longer matches. */
export class OptimisticLockError extends Error {
  name = "OptimisticLockError";
  code = "OPTIMISTIC_CONCURRENCY" as const;
  readonly [OPTIMISTIC_LOCK_ERROR_BRAND] = true;
  concurrencyColumn: string;
  expectedValue: number;
  /** Advisory flag for retry helpers. */
  retryable = true as const;

  static [Symbol.hasInstance](value: unknown): boolean {
    return hasErrorBrand(value, OPTIMISTIC_LOCK_ERROR_BRAND, "OptimisticLockError");
  }

  constructor(expectation: ConcurrencyExpectation, collection?: string) {
    super(
      `Optimistic concurrency failure on ${collection ?? "collection"}: ` +
      `expected \`${expectation.column}\` value ${expectation.expected}, ` +
      "row was modified by another writer",
    );
    this.concurrencyColumn = expectation.column;
    this.expectedValue = expectation.expected;
  }
}

/** Translate a native compare-and-swap failure into the SDK error type. */
export function mapOptimisticConcurrencyError(
  e: unknown,
  collection: string,
  expectation: ConcurrencyExpectation,
): Error {
  if (readCanonicalErrorCode(e) === "OPTIMISTIC_CONCURRENCY") {
    return new OptimisticLockError(expectation, collection);
  }
  return e instanceof Error ? e : new Error(String(e));
}

/** `Query.unique()` found no matching row. */
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

/** `Query.unique()` found multiple matching rows. */
export class NotUniqueError extends Error {
  name = "NotUniqueError";
  code = "NOT_UNIQUE" as const;
  readonly [NOT_UNIQUE_ERROR_BRAND] = true;
  collection?: string;
  /** Number of rows observed before the ambiguity was established. */
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

/** A query terminal was called without the state it requires. */
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
