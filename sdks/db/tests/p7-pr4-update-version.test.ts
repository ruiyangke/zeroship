/** SDK optimistic-concurrency behavior follows descriptor field roles. */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { installSchemaForTest } from "./_install-helper.js";
import { Collection, t, schema as schemaWrap } from "@zeroship/db";
import { OptimisticLockError } from "@zeroship/db";
import type { NativeDb } from "../src/native.js";

type AnyRec = Record<string, unknown>;

function makeNativeCapturingUpdate() {
  const captured: { filter?: AnyRec; update?: AnyRec } = {};
  const native = {
    collection(_name: string) {
      return {
        async update(filter: AnyRec, update: AnyRec) {
          captured.filter = filter;
          captured.update = update;
          return { id: "post_x", title: update.title ?? "x", version: 2 };
        },
      };
    },
    _captured: captured,
  };
  return { native: native as unknown as NativeDb, captured };
}

function makeNativeOptimisticConcurrencyFailure() {
  const native = {
    collection(_name: string) {
      return {
        async update(_filter: AnyRec, _update: AnyRec) {
          const e = new Error(
            "Optimistic concurrency check failed for posts post_x: expected `revision` value 5",
          );
          (e as Error & { code: string }).code = "concurrency_mismatch";
          throw e;
        },
      };
    },
  };
  return native as unknown as NativeDb;
}

describe("P7 PR 4 — update() + version CAS via runtime", () => {
  test("update_returns_row_with_incremented_version", async () => {
    const { native, captured } = makeNativeCapturingUpdate();
    const db = installSchemaForTest(
      {
        posts: schemaWrap({
          title: t.string().required(),
        }).withVersioning(),
      },
      { native },
    );
    const result = await db.posts.update(
      { id: "post_x", version: 1 } as never,
      { title: "renamed" },
    );
    assert.equal(result.error, null, `error: ${result.error}`);
    const row = result.data as AnyRec | null;
    assert.ok(row);
    assert.equal(row.version, 2, "version comes back bumped (runtime auto-bumped)");
    assert.equal(
      (captured.update as AnyRec | undefined)?.$inc,
      undefined,
      "SDK must not ship $inc:{version:1}; runtime owns the bump",
    );
  });

  test("update_with_stale_custom_concurrency_field_reports_that_field", async () => {
    const native = makeNativeOptimisticConcurrencyFailure();
    const posts = new Collection(
      "posts",
      {
        id: { type: "string", required: true, primaryKey: true },
        title: { type: "string", required: true },
        revision: { type: "integer", required: true, concurrency: true },
      },
      native,
    );
    const result = await posts.update(
      { id: "post_x", revision: 5 } as never,
      { title: "renamed" },
    );
    assert.equal(result.data, null);
    assert.ok(result.error);
    assert.ok(
      result.error instanceof OptimisticLockError,
      `expected OptimisticLockError, got ${(result.error as Error).constructor.name}`,
    );
    assert.equal((result.error as OptimisticLockError).code, "OPTIMISTIC_CONCURRENCY");
    assert.equal((result.error as OptimisticLockError).concurrencyColumn, "revision");
    assert.equal((result.error as OptimisticLockError).expectedValue, 5);
    assert.equal("expectedVersion" in (result.error as OptimisticLockError), false);
    assert.match(result.error.message, /expected `revision` value 5/);
    assert.equal((result.error as OptimisticLockError).retryable, true);
  });

  test("update_without_version_filter_succeeds_blindly", async () => {
    const { native } = makeNativeCapturingUpdate();
    const db = installSchemaForTest(
      {
        posts: schemaWrap({
          title: t.string().required(),
        }).withVersioning(),
      },
      { native },
    );
    const result = await db.posts.update(
      { id: "post_x" } as never,
      { title: "renamed" },
    );
    assert.equal(result.error, null);
    const row = result.data as AnyRec | null;
    assert.ok(row);
  });

  test("OPTIMISTIC_CONCURRENCY_error_is_retryable", () => {
    const e = new OptimisticLockError({ column: "revision", expected: 7 }, "posts");
    assert.equal(e.retryable, true);
    assert.equal(e.code, "OPTIMISTIC_CONCURRENCY");
    assert.equal(e.concurrencyColumn, "revision");
    assert.equal(e.expectedValue, 7);
  });
});
