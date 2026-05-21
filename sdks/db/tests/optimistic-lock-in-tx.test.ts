/**
 * Robustness — OptimisticLockError raised inside a `db.transaction`
 * body must be caught by the outer tx wrapper and surface as
 * `result.error`, not as an unhandled rejection. Pins the path:
 *
 *   tx.users.update(...) -> Collection.update() -> updateOne() returns null
 *   (CAS miss) -> Collection.update throws OptimisticLockError ->
 *   `unwrap` re-throws -> tx body rejects -> outer wrapper catches and
 *   rolls back -> result.error === OptimisticLockError instance.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { installSchemaForTest } from "./_install-helper.js";
import { t, schema as schemaWrap } from "@zeroship/db";
import { OptimisticLockError } from "@zeroship/db";

type AnyRec = Record<string, unknown>;

function makeNativeCasMissOnUpdate() {
  const calls: { method: string; args: unknown[] }[] = [];
  let rolledBack = false;
  let committed = false;
  const native = {
    registerModel: () => Promise.resolve(),
    async beginTransaction(_opts?: { isolationLevel?: string }) {
      return {
        async commit() { committed = true; },
        async rollback() { rolledBack = true; },
      };
    },
    collection(_name: string) {
      return {
        async updateOne(filter: AnyRec, update: AnyRec) {
          calls.push({ method: "updateOne", args: [filter, update] });
          // Returning null with a versioned filter (version: N) drives
          // Collection.update to throw OptimisticLockError.
          return null;
        },
      };
    },
    get _state() {
      return { rolledBack, committed, calls };
    },
  };
  return native as unknown as ZeroshipDb;
}

describe("db.transaction — OptimisticLockError surfaces via result.error", () => {
  test("tx body update with CAS miss → rollback + result.error is OptimisticLockError", async () => {
    const native = makeNativeCasMissOnUpdate();
    const db = installSchemaForTest(
      {
        widgets: schemaWrap({
          name: t.string().required(),
        }).withVersioning(),
      },
      { native },
    );

    const result = await db.transaction(async (tx) => {
      // Versioned filter: { id: 1, version: 7 } — the CAS guard
      // detects the modified row (updateOne returns null) and the
      // Collection throws OptimisticLockError.
      const out = await tx.widgets.update(
        { id: 1, version: 7 } as never,
        { name: "renamed" },
      );
      return out;
    });

    assert.equal(result.data, null);
    assert.ok(result.error);
    assert.ok(
      result.error instanceof OptimisticLockError,
      `expected OptimisticLockError, got ${(result.error as Error).constructor.name}`,
    );
    const occ = result.error as OptimisticLockError;
    assert.equal(occ.code, "optimistic_lock_failure");
    assert.equal(occ.expectedVersion, 7);

    // Outer wrapper rolled back, did not commit.
    const state = (native as unknown as { _state: { rolledBack: boolean; committed: boolean } })._state;
    assert.equal(state.rolledBack, true);
    assert.equal(state.committed, false);
  });

  test("a non-OCC throw from the body also rolls back and surfaces as result.error", async () => {
    const native = {
      registerModel: () => Promise.resolve(),
      async beginTransaction() {
        return { async commit() {}, async rollback() {} };
      },
      collection(_name: string) {
        return {
          async updateOne(_f: AnyRec, _u: AnyRec) { return { id: 1, name: "x" }; },
        };
      },
    } as unknown as ZeroshipDb;
    const db = installSchemaForTest({ widgets: { name: t.string().required() } }, { native });
    const result = await db.transaction(async () => {
      throw new Error("body bombed");
    });
    assert.ok(result.error);
    assert.equal(result.error.message, "body bombed");
    assert.equal(result.data, null);
  });
});
