/**
 * Adapter-only native-error mapping. The creator-facing error classes and
 * canonical-code helpers live in `@zeroship/db` (`packages/db/src/errors.ts`);
 * this module turns a caught native value into one of those typed errors.
 */
import {
  readCanonicalErrorCode,
  OptimisticLockError,
  type ConcurrencyExpectation,
} from "../../../../packages/db/src/errors";

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
