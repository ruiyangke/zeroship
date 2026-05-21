/**
 * R4 IMPORTANT-1 regression — `mapFilterOutbound(null)` used to return
 * `null` (the `for...in null` loop is a zero-iteration no-op, not a
 * throw), which let `deleteMany(null)` / `updateMany(null)` reach the
 * native layer as "match every row" — a delete-all bug reachable via
 * a JSON-RPC input or an `as any` escape past the TS type. The fix is
 * a defensive null/non-object reject at the top of `mapFilterOutbound`,
 * with `code = "invalid_filter"`. Inside `_run` the throw becomes a
 * `Result.error`; the destructive native call never fires.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { env } from "zeroship";
import { installSchemaForTest } from "./_install-helper.js";
import { t } from "../src/types.js";

type AnyRec = Record<string, unknown>;

function makeFakeSub() {
  let closed = false;
  return {
    async next() { if (closed) return null; return new Promise(() => undefined); },
    close() { closed = true; },
  };
}

function installEnv(native: unknown): void {
  (env as { db?: unknown }).db = native;
}

/** Native mock that RECORDS every mutating call so the test can assert
 *  the destructive op was never reached. */
function makeRecordingNative() {
  const calls: { op: string; filter: unknown }[] = [];
  const native = {
    registerModel: async () => undefined,
    beginTransaction: async () => ({
      commit: async () => undefined,
      rollback: async () => undefined,
    }),
    collection(_name: string) {
      return {
        async findOne(_f: AnyRec) { return null; },
        async find(_f: AnyRec) { return []; },
        async insert(row: AnyRec) { return row; },
        async deleteMany(filter: unknown) {
          calls.push({ op: "deleteMany", filter });
          return 0;
        },
        async updateMany(filter: unknown) {
          calls.push({ op: "updateMany", filter });
          return 0;
        },
      };
    },
    openSubscription(_name: string) { return makeFakeSub(); },
  };
  return { native: native as unknown as ZeroshipDb, calls };
}

describe("R4 IMPORTANT-1 — null/non-object filter rejection", () => {
  test("deleteMany(null) resolves to Result.error with code=invalid_filter", async () => {
    const { native, calls } = makeRecordingNative();
    installEnv(native);
    const db = installSchemaForTest(
      { todos: { title: t.string().required() } },
      { native },
    );
    // `as any` is the exact escape that the type system was supposed to
    // prevent. JSON-RPC inputs land here too.
    const r = await db.todos.deleteMany(null as any);
    assert.ok(r.error instanceof Error, "expected Result.error");
    assert.equal((r.error as { code?: string }).code, "invalid_filter");
    assert.equal(r.data, null);
    // CRITICAL: the native deleteMany must NOT have been invoked.
    assert.equal(
      calls.filter((c) => c.op === "deleteMany").length,
      0,
      "null filter must never reach the native deleteMany call",
    );
  });

  test("updateMany(null, patch) resolves to Result.error with code=invalid_filter", async () => {
    const { native, calls } = makeRecordingNative();
    installEnv(native);
    const db = installSchemaForTest(
      { todos: { title: t.string().required() } },
      { native },
    );
    const r = await db.todos.updateMany(null as any, { $set: { title: "x" } });
    assert.ok(r.error instanceof Error, "expected Result.error");
    assert.equal((r.error as { code?: string }).code, "invalid_filter");
    assert.equal(r.data, null);
    assert.equal(
      calls.filter((c) => c.op === "updateMany").length,
      0,
      "null filter must never reach the native updateMany call",
    );
  });

  test("deleteMany() with no argument matches all rows (legitimate)", async () => {
    // The fix only rejects `null` and non-objects — passing no argument
    // (undefined) coerces to `{}`, which is the documented "match all"
    // form. This is intentional: a deliberate `deleteMany()` is
    // legitimate; an accidental `deleteMany(null)` is the footgun.
    const { native, calls } = makeRecordingNative();
    installEnv(native);
    const db = installSchemaForTest(
      { todos: { title: t.string().required() } },
      { native },
    );
    const r = await db.todos.deleteMany();
    assert.equal(r.error, null);
    assert.equal(r.data?.deletedCount, 0);
    assert.equal(calls.filter((c) => c.op === "deleteMany").length, 1);
  });

  test("deleteMany(123 as any) — non-object filter is rejected too", async () => {
    const { native, calls } = makeRecordingNative();
    installEnv(native);
    const db = installSchemaForTest(
      { todos: { title: t.string().required() } },
      { native },
    );
    const r = await db.todos.deleteMany(123 as any);
    assert.ok(r.error instanceof Error);
    assert.equal((r.error as { code?: string }).code, "invalid_filter");
    assert.equal(
      calls.filter((c) => c.op === "deleteMany").length,
      0,
      "non-object filter must never reach the native deleteMany call",
    );
  });
});
