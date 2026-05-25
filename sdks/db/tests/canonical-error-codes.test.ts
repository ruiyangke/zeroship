import { test, describe } from "node:test";
import assert from "node:assert/strict";
import {
  canonicalErrorCode,
  mapNativeError,
  mapOptimisticConcurrencyError,
  NotFoundError,
  OptimisticLockError,
  ValidationError,
} from "../src/errors.js";

function nativeError(code: string): Error {
  return Object.assign(new Error(code), { code });
}

describe("canonical SDK error codes", () => {
  test("representative failures emit canonical SCREAMING_SNAKE codes", () => {
    assert.equal(
      (mapNativeError(nativeError("unique_violation")) as Error & { code?: string }).code,
      "UNIQUE_VIOLATION",
    );

    assert.equal(new NotFoundError("users").code, "NOT_FOUND");

    const optimistic = mapOptimisticConcurrencyError(
      nativeError("version_mismatch"),
      "users",
      7,
    );
    assert.ok(optimistic instanceof OptimisticLockError);
    assert.equal(optimistic.code, "OPTIMISTIC_CONCURRENCY");
    assert.equal(optimistic.expectedVersion, 7);

    assert.equal(
      new ValidationError({
        email: { path: "email", message: "email is required" },
      }).code,
      "VALIDATION",
    );
  });

  test("native plugin-db codes map to SDK-facing canonical codes", () => {
    assert.equal(canonicalErrorCode("fk_violation"), "FOREIGN_KEY_VIOLATION");
    assert.equal(canonicalErrorCode("lock_not_available"), "LOCK_NOT_AVAILABLE");
    assert.equal(canonicalErrorCode("not_null_violation"), "NOT_NULL_VIOLATION");
    assert.equal(canonicalErrorCode("check_violation"), "CHECK_VIOLATION");
    assert.equal(canonicalErrorCode("serialization_failure"), "SERIALIZATION_FAILURE");
  });
});
