/**
 * P9 PR 3 — `env.db.transaction(fn)` rides the native orchestrator.
 *
 * Transaction begin/commit/rollback/nested-savepoint moved into Rust
 * (`crates/plugin-db/src/orchestrator/transaction.rs`). The
 * `@zeroship/bootstrap` `transactionImpl` is now a thin `Result`-wrapping
 * shim over the native `env.db.transaction(callback, opts)` v8_method,
 * keeping only the JS-only concerns (DataLoader drain + `_txDepth`
 * bookkeeping).
 *
 * These tests mock the native method (no DB) to pin the observable
 * creator-facing contract through the bootstrap wrapper:
 *   - commit on resolve → `result.data` is the body value;
 *   - rollback on throw → `result.error` is the thrown error;
 *   - nested transaction isolates an inner failure (savepoint) while the
 *     outer continues;
 *   - the `tx` collections still throw on error (Result→throw wrapping
 *     preserved);
 *   - `env.db.beginTransaction` is gone.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { installSchemaForTest } from "./_install-helper.js";
import { t } from "@zeroship/db";

type AnyRec = Record<string, unknown>;

/**
 * A native mock whose `transaction(callback)` faithfully models the Rust
 * orchestrator's contract: it calls the callback (begin succeeded),
 * resolves with its result on commit, and re-throws on rollback. Nested
 * `env.db.transaction(...)` calls (made from inside the callback) recurse
 * through this same method — modelling the savepoint path: an inner
 * rejection is isolated (the inner call rejects) without aborting the
 * outer call.
 */
function makeNativeTxMock(rowsByTable: Record<string, AnyRec[]> = {}) {
  const settles: string[] = [];
  let depth = 0;
  const native = {
    registerModel: () => Promise.resolve(),
    async transaction(cb: (raw: unknown) => unknown, _opts?: { isolationLevel?: string }) {
      depth += 1;
      const level = depth;
      settles.push(level === 1 ? "begin" : `savepoint:${level}`);
      try {
        const out = await cb(undefined);
        settles.push(level === 1 ? "commit" : `release:${level}`);
        return out;
      } catch (e) {
        settles.push(level === 1 ? "rollback" : `rollback-to:${level}`);
        throw e;
      } finally {
        depth -= 1;
      }
    },
    collection(name: string) {
      return {
        async insert(row: AnyRec) {
          (rowsByTable[name] ??= []).push(row);
          return { id: `${name}_${(rowsByTable[name]?.length ?? 0)}`, ...row };
        },
        async find(_filter: AnyRec, _opts: AnyRec) {
          return [...(rowsByTable[name] ?? [])];
        },
        async update(_filter: AnyRec, _update: AnyRec) {
          // CAS-miss simulation: returning null with a versioned filter
          // drives Collection.update to throw OptimisticLockError.
          return null;
        },
      };
    },
    get _settles() {
      return settles;
    },
  };
  return native as unknown as import("../src/collection.js").NativeDb & { _settles: string[] };
}

describe("P9 PR 3 — native env.db.transaction(fn)", () => {
  test("transaction(fn) commits on resolve", async () => {
    const native = makeNativeTxMock();
    const db = installSchemaForTest(
      { posts: { title: t.string().required() } },
      { native },
    );

    const result = await db.transaction(async (tx) => {
      await tx.posts.insert({ title: "hello" });
      return "done";
    });

    assert.equal(result.error, null);
    assert.equal(result.data, "done", "transaction(fn) resolves with the body return value");
    assert.deepEqual(
      (native as unknown as { _settles: string[] })._settles,
      ["begin", "commit"],
      "a top-level tx that resolved must COMMIT",
    );
  });

  test("transaction(fn) rolls back + returns err on throw", async () => {
    const native = makeNativeTxMock();
    const db = installSchemaForTest(
      { posts: { title: t.string().required() } },
      { native },
    );

    const boom = Object.assign(new Error("nope"), { code: "user_abort" });
    const result = await db.transaction(async (tx) => {
      await tx.posts.insert({ title: "doomed" });
      throw boom;
    });

    assert.equal(result.data, null);
    assert.ok(result.error, "expected result.error");
    assert.equal(result.error, boom, "the thrown error surfaces verbatim as result.error");
    assert.equal((result.error as { code?: string }).code, "user_abort");
    assert.deepEqual(
      (native as unknown as { _settles: string[] })._settles,
      ["begin", "rollback"],
      "a top-level tx whose body threw must ROLLBACK",
    );
  });

  test("nested transaction isolates inner failure (savepoint)", async () => {
    const native = makeNativeTxMock();
    const db = installSchemaForTest(
      { posts: { title: t.string().required() } },
      { native },
    );

    const result = await db.transaction(async (tx) => {
      // Inner tx fails — modelled as a recursive transaction(fn) call
      // (the savepoint path). The inner rejection is isolated; the outer
      // keeps going.
      const inner = await db.transaction(async (tx2) => {
        await tx2.posts.insert({ title: "inner-doomed" });
        throw new Error("inner abort");
      });
      assert.ok(inner.error, "inner tx must report its failure");
      await tx.posts.insert({ title: "outer-survives" });
      return "outer-ok";
    });

    assert.equal(result.error, null, "outer tx must commit despite the inner failure");
    assert.equal(result.data, "outer-ok");
    // Settle order: outer begin, inner savepoint, inner rollback-to,
    // outer commit. The inner failure rolled back to its savepoint
    // without aborting the outer.
    assert.deepEqual(
      (native as unknown as { _settles: string[] })._settles,
      ["begin", "savepoint:2", "rollback-to:2", "commit"],
      "inner failure rolls back to its SAVEPOINT; outer COMMITs",
    );
  });

  test("tx methods throw on error (Result→throw wrapping preserved)", async () => {
    // The tx-view collections wrap native `Result`/reject into throws so
    // the callback contract is "throw to abort". A CAS miss on update
    // surfaces as a thrown OptimisticLockError inside the callback, which
    // the orchestrator turns into a rollback + `result.error`.
    // Import OptimisticLockError + schema from the SAME entry the SDK's
    // internal `Collection.update` throws from, so the `instanceof` check
    // compares one class identity (a dual import would yield two distinct
    // classes and a spurious mismatch).
    const { OptimisticLockError, schema: schemaWrap } = await import("@zeroship/db");
    const native = makeNativeTxMock();
    const db = installSchemaForTest(
      {
        widgets: schemaWrap({
          name: t.string().required(),
        }).withVersioning(),
      },
      { native },
    );

    const result = await db.transaction(async (tx) => {
      // Versioned filter → CAS guard → native update returns null →
      // TxCollection.update throws OptimisticLockError (Result→throw).
      return await tx.widgets.update({ id: 1, version: 3 } as never, { name: "x" });
    });

    assert.equal(result.data, null);
    assert.ok(
      result.error instanceof OptimisticLockError,
      `expected OptimisticLockError, got ${(result.error as Error)?.constructor?.name}`,
    );
    assert.deepEqual(
      (native as unknown as { _settles: string[] })._settles,
      ["begin", "rollback"],
      "the thrown OptimisticLockError must roll the tx back",
    );
  });

  test("re-install then transaction(fn) does not recurse (native method captured once)", async () => {
    // The install loop plants an own `transaction` property that shadows
    // the native method. A second installSchema on the same native handle
    // must still reach the *native* orchestrator, not the previously
    // installed wrapper (which would recurse forever). The bootstrap
    // stashes the native method under a hidden key on first install and
    // reuses it.
    const native = makeNativeTxMock();
    const db1 = installSchemaForTest(
      { posts: { title: t.string().required() } },
      { native },
    );
    // First-install transaction works.
    const r1 = await db1.transaction(async () => "first");
    assert.equal(r1.data, "first");

    // Re-install on the SAME native handle (the install loop overwrites
    // env.db.transaction again).
    const db2 = installSchemaForTest(
      { posts: { title: t.string().required() }, tags: { label: t.string().required() } },
      { native },
    );
    // The re-installed wrapper must reach the native orchestrator, not
    // recurse into itself.
    const r2 = await db2.transaction(async () => "second");
    assert.equal(r2.error, null, "re-installed transaction(fn) must not recurse / error");
    assert.equal(r2.data, "second");
  });

  test("beginTransaction is not exposed on env.db", async () => {
    const native = makeNativeTxMock();
    const db = installSchemaForTest(
      { posts: { title: t.string().required() } },
      { native },
    );
    // The native primitive was deleted in P9 PR 3 — neither the mock nor
    // the installed surface exposes `beginTransaction`.
    assert.equal(
      (db as unknown as Record<string, unknown>).beginTransaction,
      undefined,
      "env.db.beginTransaction must be undefined",
    );
    assert.equal(
      typeof (db as unknown as { transaction?: unknown }).transaction,
      "function",
      "env.db.transaction (the Result-wrapping shim) must be present",
    );
  });
});
