/**
 * Robustness — `db.transaction(...)` `commit()` rejects while `rollback()`
 * also rejects. Pins the `commit_failed_indeterminate` contract:
 *
 *   - `result.error` is non-null
 *   - `result.error.code === "commit_failed_indeterminate"`
 *   - `result.error.cause` is the original commit rejection (so callers
 *     can walk the cause chain to surface the underlying driver error)
 *
 * The rollback rejection is silently swallowed by the SDK today — the
 * audit (`db-robustness-gaps-2026-05-19.md`, Gap E) flagged that as a
 * separate gap; this test only pins what IS reachable from
 * `result.error`.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { createDb } from "../src/db.js";
import { t } from "../src/types.js";

type AnyRec = Record<string, unknown>;

function makeDoubleFailingNative(commitErr: Error, rollbackErr: Error) {
  let txCount = 0;
  const native = {
    registerModel: () => Promise.resolve(),
    async beginTransaction(_opts?: { isolationLevel?: string }) {
      txCount += 1;
      return {
        async commit() { throw commitErr; },
        async rollback() { throw rollbackErr; },
      };
    },
    collection(_name: string) {
      return {
        async findOne(_f: AnyRec) { return null; },
        async find(_f: AnyRec) { return []; },
      };
    },
    get _txCount() { return txCount; },
  };
  return native as unknown as ZeroshipDb;
}

describe("db.transaction — commit_failed_indeterminate", () => {
  test("commit rejects + rollback rejects → result.error carries the code and the cause chain", async () => {
    const commitErr = Object.assign(new Error("network drop after COMMIT"), {
      code: "connection_lost",
    });
    const rollbackErr = Object.assign(new Error("rollback after commit is moot"), {
      code: "rollback_after_commit",
    });
    const native = makeDoubleFailingNative(commitErr, rollbackErr);

    const db = createDb(
      { users: { name: t.string().required() } },
      { native },
    );

    const result = await db.transaction(async () => {
      // empty body — we only care about the commit/rollback failure.
      return 42;
    });

    assert.ok(result.error, "expected an error");
    assert.equal(result.data, null);
    const err = result.error as Error & { code?: string; cause?: unknown };
    assert.equal(
      err.code,
      "commit_failed_indeterminate",
      "wrapped error must carry the indeterminate code",
    );
    assert.match(err.message, /commit failed/i);
    // The audit's "cause chain" requirement: at minimum, the commit
    // error must be reachable via `.cause`. The rollback error is not
    // exposed (Gap E flags this separately).
    assert.equal(err.cause, commitErr, "cause must be the original commit rejection");
    assert.equal((err.cause as Error).message, "network drop after COMMIT");
  });

  test("commit rejects, rollback succeeds → still surfaces the indeterminate code", async () => {
    const commitErr = Object.assign(new Error("deadlock at commit"), {
      code: "deadlock_detected",
    });
    const native = {
      registerModel: () => Promise.resolve(),
      async beginTransaction() {
        return {
          async commit() { throw commitErr; },
          async rollback() { /* succeeds */ },
        };
      },
      collection(_name: string) {
        return { async findOne() { return null; }, async find() { return []; } };
      },
    } as unknown as ZeroshipDb;

    const db = createDb(
      { users: { name: t.string().required() } },
      { native },
    );

    const result = await db.transaction(async () => 1);
    assert.ok(result.error);
    const err = result.error as Error & { code?: string; cause?: unknown };
    assert.equal(err.code, "commit_failed_indeterminate");
    assert.equal(err.cause, commitErr);
  });
});
