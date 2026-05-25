import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { withRetry, isOptimisticLockError } from "../src/with-retry.js";
import { OptimisticLockError } from "../src/errors.js";

describe("withRetry", () => {
  test("succeeds on first try without invoking the retry path", async () => {
    let calls = 0;
    const result = await withRetry(async () => {
      calls += 1;
      return "ok";
    });
    assert.equal(result, "ok");
    assert.equal(calls, 1);
  });

  test("succeeds on 3rd try after 2 OCC throws", async () => {
    let calls = 0;
    const result = await withRetry(async () => {
      calls += 1;
      if (calls < 3) {
        throw new OptimisticLockError(calls, "products");
      }
      return "done";
    });
    assert.equal(result, "done");
    assert.equal(calls, 3);
  });

  test("gives up after max attempts and rethrows the last error", async () => {
    let calls = 0;
    const err = await withRetry(async () => {
      calls += 1;
      throw new OptimisticLockError(7, "x");
    }, { max: 3 }).then(
      () => null,
      (e) => e,
    );
    assert.ok(err instanceof OptimisticLockError);
    assert.equal(err.expectedVersion, 7);
    assert.equal(calls, 3);
  });

  test("does not retry on errors that fail the predicate", async () => {
    let calls = 0;
    const err = await withRetry(async () => {
      calls += 1;
      throw new Error("not an OCC failure");
    }).then(() => null, (e) => e);
    assert.equal(calls, 1);
    assert.ok(err instanceof Error);
    assert.equal(err.message, "not an OCC failure");
  });

  test("default max is 3", async () => {
    let calls = 0;
    await withRetry(async () => {
      calls += 1;
      throw new OptimisticLockError(1);
    }).catch(() => {});
    assert.equal(calls, 3);
  });

  test("custom on() predicate is honored", async () => {
    let calls = 0;
    const result = await withRetry(
      async () => {
        calls += 1;
        if (calls < 2) {
          const e = new Error("custom retry");
          (e as Error & { code: string }).code = "SERIALIZATION_FAILURE";
          throw e;
        }
        return "ok";
      },
      {
        on: (e) =>
          isOptimisticLockError(e) ||
          (e as { code?: string }).code === "SERIALIZATION_FAILURE",
      },
    );
    assert.equal(result, "ok");
    assert.equal(calls, 2);
  });

  test("backoff is called with 1-indexed attempt count", async () => {
    const attempts: number[] = [];
    let calls = 0;
    await withRetry(
      async () => {
        calls += 1;
        if (calls < 3) throw new OptimisticLockError(calls);
        return "done";
      },
      {
        backoff: (a) => {
          attempts.push(a);
          return 0;
        },
      },
    );
    // After call 1 → backoff(1); after call 2 → backoff(2); call 3 succeeds.
    assert.deepEqual(attempts, [1, 2]);
  });

  test("max=1 means one attempt with no retry", async () => {
    let calls = 0;
    await withRetry(
      async () => {
        calls += 1;
        throw new OptimisticLockError(1);
      },
      { max: 1 },
    ).catch(() => {});
    assert.equal(calls, 1);
  });

  test("max <= 0 throws TypeError immediately", () => {
    assert.rejects(
      () => withRetry(async () => "x", { max: 0 }),
      /positive integer/,
    );
  });

  test("non-Error throws are wrapped before the predicate sees them", async () => {
    let calls = 0;
    // A bare string throw — the predicate gets a wrapped Error, but the
    // ORIGINAL thrown value is what bubbles up to the caller (no swallow).
    const result = await withRetry(async () => {
      calls += 1;
      throw "raw string";
    }, { on: () => false }).then(() => null, (e) => e);
    assert.equal(calls, 1);
    assert.equal(result, "raw string");
  });

  test("isOptimisticLockError matches OptimisticLockError instances", () => {
    assert.equal(isOptimisticLockError(new OptimisticLockError(1)), true);
    assert.equal(isOptimisticLockError(new Error("x")), false);
    assert.equal(isOptimisticLockError("not an error"), false);
    assert.equal(isOptimisticLockError(null), false);
  });

  test("isOptimisticLockError matches plain Error with the right code", () => {
    const e = new Error("x");
    (e as Error & { code: string }).code = "OPTIMISTIC_CONCURRENCY";
    assert.equal(isOptimisticLockError(e), true);
  });

  test("isOptimisticLockError maps the native optimistic-concurrency code", () => {
    const e = new Error("x");
    (e as Error & { code: string }).code = "version_mismatch";
    assert.equal(isOptimisticLockError(e), true);
  });
});
