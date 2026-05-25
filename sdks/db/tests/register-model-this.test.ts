/**
 * Pin the v8_class `this`-binding contract for `native.registerModel`.
 *
 * Both `installSchema` (sdks/db/src/db-types.ts via bootstrap) and `model()` (sdks/db/src/model.ts)
 * dispatch `native.registerModel(name, schema, indexes)`. The native side
 * is a v8_class method whose internal brand check throws
 * "Illegal invocation" if the receiver isn't the original instance —
 * which happens silently when the method is hoisted into a local variable
 * and called as a plain function (`fn(args)` instead of `obj.fn(args)`).
 *
 * Regression history:
 *   - fa30871c introduced the typed-cast refactor that lost `this` in
 *     both files.
 *   - e564c010 fixed `db-types.ts` only; `model.ts`'s try/catch silently
 *     swallowed the same bug.
 *
 * This test pins the contract by using a mock that REQUIRES the correct
 * receiver and asserts both call sites pass the check.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { installSchemaForTest } from "./_install-helper.js";
import { model } from "@zeroship/bootstrap/install-schema";
import { t } from "@zeroship/db";

/** A mock native whose `registerModel` enforces a brand check: throws if
 *  invoked with the wrong `this`. Mirrors what the real v8_class does. */
function makeBrandedNative() {
  const calls: { this: unknown; name: string }[] = [];
  const native = {
    registerModel(this: unknown, name: string, _schema: unknown, _indexes?: unknown): Promise<void> {
      // Brand check: real v8_class throws TypeError("Illegal invocation")
      // when `this` is not the original instance.
      if (this !== native) {
        throw new TypeError("Illegal invocation");
      }
      calls.push({ this: this, name });
      return Promise.resolve();
    },
    // P9 PR 3: native `transaction(callback)` orchestrator stub.
    transaction: async (cb: (raw: unknown) => unknown) => cb(undefined),
    collection(_name: string) {
      return {
        async findOne() { return null; },
        async find() { return []; },
        async insert(row: Record<string, unknown>) { return row; },
      };
    },
  };
  return { native: native as unknown as ZeroshipDb, calls };
}

describe("registerModel — `this` binding preserved", () => {
  test("installSchema dispatches registerModel with correct receiver", async () => {
    const { native, calls } = makeBrandedNative();
    const db = installSchemaForTest(
      { users: { name: t.string().required() } },
      { native },
    );
    // Force the registration chain to run by touching the collection
    // (every CRUD path awaits the `ready` promise the SDK pins on the
    // Collection — if registerModel threw "Illegal invocation", the
    // first op would fail).
    const { error } = await db.users.find({});
    assert.equal(error, null);
    assert.equal(calls.length, 1, "registerModel should fire exactly once");
    assert.equal(calls[0].name, "users");
  });

  test("model() factory dispatches registerModel with correct receiver", async () => {
    const { native, calls } = makeBrandedNative();
    // The bug pre-fix: `model()` would call registerModel as a plain
    // function, the brand check would throw, and the try/catch in
    // model.ts silently swallowed the TypeError — leaving
    // `registrationPromise` null. With the fix this dispatches cleanly.
    const Users = model(
      "users",
      { name: t.string().required() },
      native,
    );
    const { error } = await Users.find({});
    assert.equal(error, null);
    assert.equal(calls.length, 1, "registerModel should fire exactly once");
    assert.equal(calls[0].name, "users");
  });
});
