import { test, describe } from "node:test";
import assert from "node:assert/strict";
import {
  canonicalErrorCode,
  NotFoundError,
  OptimisticLockError,
  ValidationError,
} from "../src/errors.js";
import {
  mapNativeError,
  mapOptimisticConcurrencyError,
} from "../../../crates/zeroship-data-v8/js/runtime/errors.js";

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
      nativeError("concurrency_mismatch"),
      "users",
      { column: "revision", expected: 7 },
    );
    assert.ok(optimistic instanceof OptimisticLockError);
    assert.equal(optimistic.code, "OPTIMISTIC_CONCURRENCY");
    assert.equal(optimistic.concurrencyColumn, "revision");
    assert.equal(optimistic.expectedValue, 7);

    assert.equal(
      new ValidationError({
        email: { path: "email", message: "email is required" },
      }).code,
      "VALIDATION",
    );
  });

  test("native ORM codes map to SDK-facing canonical codes", () => {
    assert.equal(canonicalErrorCode("fk_violation"), "FOREIGN_KEY_VIOLATION");
    assert.equal(canonicalErrorCode("lock_not_available"), "LOCK_NOT_AVAILABLE");
    assert.equal(canonicalErrorCode("not_null_violation"), "NOT_NULL_VIOLATION");
    assert.equal(canonicalErrorCode("check_violation"), "CHECK_VIOLATION");
    assert.equal(canonicalErrorCode("serialization_failure"), "SERIALIZATION_FAILURE");
  });

  test("a call refused because its request ended keeps the runtime's spelling", () => {
    // The runtime refuses fetch, KV, storage and workflow calls of an ended
    // request with the same code, so the database spells it the same way.
    assert.equal(canonicalErrorCode("request_cancelled"), "request_cancelled");
    assert.equal(
      (mapNativeError(nativeError("request_cancelled")) as Error & { code?: string }).code,
      "request_cancelled",
    );
    // The control beside it: a database code is still canonicalized.
    assert.equal(canonicalErrorCode("unique_violation"), "UNIQUE_VIOLATION");
  });
});
