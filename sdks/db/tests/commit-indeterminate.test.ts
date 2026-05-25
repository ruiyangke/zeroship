/**
 * Robustness — `db.transaction(...)` `commit()` rejects while `rollback()`
 * also rejects. Pins the `COMMIT_FAILED_INDETERMINATE` contract:
 *
 *   - `result.error` is non-null
 *   - `result.error.code === "COMMIT_FAILED_INDETERMINATE"`
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
import { installSchemaForTest } from "./_install-helper.js";
import { t } from "@zeroship/db";

type AnyRec = Record<string, unknown>;

// P9 PR 3: commit failure is now owned by the native orchestrator. The
// mock's `transaction(callback)` runs the callback (begin succeeded),
// then simulates a COMMIT that fails — rejecting with the
// `COMMIT_FAILED_INDETERMINATE`-coded error the Rust orchestrator emits
// (`crates/plugin-db/src/orchestrator/transaction.rs::exec_settle_top_level`).
// The `.cause` is preserved on the rejection so the SDK's `result.error`
// keeps the cause chain the pre-PR3 JS `transactionImpl` produced.
function makeCommitFailingNative(commitErr: Error) {
  let txCount = 0;
  const native = {
    registerModel: () => Promise.resolve(),
    async transaction(cb: (raw: unknown) => unknown, _opts?: { isolationLevel?: string }) {
      txCount += 1;
      // begin → callback resolves (the body succeeded) → COMMIT fails.
      await cb(undefined);
      throw Object.assign(
        new Error(`commit failed — transaction state indeterminate: ${commitErr.message}`, {
          cause: commitErr,
        }),
        { code: "COMMIT_FAILED_INDETERMINATE" as const },
      );
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
  test("COMMIT failure → result.error carries commit_failed_indeterminate + the cause chain", async () => {
    // Migrated from the pre-PR3 double-failing-native shape. The native
    // orchestrator now owns COMMIT and the best-effort ROLLBACK; the mock
    // models a COMMIT that fails after the body resolved, rejecting with
    // the same coded error + cause the Rust path emits.
    const commitErr = Object.assign(new Error("network drop after COMMIT"), {
      code: "CONNECTION_LOST",
    });
    const native = makeCommitFailingNative(commitErr);

    const db = installSchemaForTest(
      { users: { name: t.string().required() } },
      { native },
    );

    const result = await db.transaction(async () => {
      // empty body — we only care about the commit failure.
      return 42;
    });

    assert.ok(result.error, "expected an error");
    assert.equal(result.data, null);
    const err = result.error as Error & { code?: string; cause?: unknown };
    assert.equal(
      err.code,
      "COMMIT_FAILED_INDETERMINATE",
      "wrapped error must carry the indeterminate code",
    );
    assert.match(err.message, /commit failed/i);
    // The "cause chain" requirement: the commit error must be reachable
    // via `.cause` (the native orchestrator preserves it on the
    // rejection; the bootstrap wrapper passes the rejection through
    // verbatim).
    assert.equal(err.cause, commitErr, "cause must be the original commit rejection");
    assert.equal((err.cause as Error).message, "network drop after COMMIT");
  });

  test("COMMIT failure surfaces the indeterminate code even when the body resolved", async () => {
    const commitErr = Object.assign(new Error("deadlock at commit"), {
      code: "DEADLOCK_DETECTED",
    });
    const native = makeCommitFailingNative(commitErr);

    const db = installSchemaForTest(
      { users: { name: t.string().required() } },
      { native },
    );

    const result = await db.transaction(async () => 1);
    assert.ok(result.error);
    const err = result.error as Error & { code?: string; cause?: unknown };
    assert.equal(err.code, "COMMIT_FAILED_INDETERMINATE");
    assert.equal(err.cause, commitErr);
  });
});
