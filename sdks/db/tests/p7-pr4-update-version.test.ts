/**
 * **P7 PR 4** — `update(filter, patch)` interaction with the platform's
 * server-side `version` auto-bump + optimistic-concurrency typed error.
 *
 * Two SDK-side responsibilities the runtime can't observe directly:
 *
 * 1. The SDK no longer adds `$inc: { version: 1 }` to the patch (the
 *    runtime's `build_update_*_with_system_fields` appends the bump).
 *    A pre-PR-4 SDK that still adds it would double-bump; we pin the
 *    behaviour change so a regression in `_augmentUpdateWithVersion`
 *    is caught.
 * 2. When the runtime throws a typed optimistic-concurrency error, the
 *    SDK's `update()` / `updateMany()` translate to `OptimisticLockError`
 *    so callers can branch on the class or `OPTIMISTIC_CONCURRENCY`.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { installSchemaForTest } from "./_install-helper.js";
import { t, schema as schemaWrap } from "@zeroship/db";
import { OptimisticLockError } from "@zeroship/db";

type AnyRec = Record<string, unknown>;

/** Native double that captures the last `update` arguments so the
 *  test can assert the SDK no longer ships `$inc: { version: 1 }`. */
function makeNativeCapturingUpdate() {
  const captured: { filter?: AnyRec; update?: AnyRec } = {};
  const native = {
    registerModel: () => Promise.resolve(),
    collection(_name: string) {
      return {
        async update(filter: AnyRec, update: AnyRec) {
          captured.filter = filter;
          captured.update = update;
          // Echo a faux row back so the SDK returns it through the
          // map-doc path. Includes the version field bumped by 1 so
          // the assertion below matches what the runtime would have
          // produced after auto-bumping server-side.
          return { id: "post_x", title: update.title ?? "x", version: 2 };
        },
      };
    },
    _captured: captured,
  };
  return { native: native as unknown as ZeroshipDb, captured };
}

/** Native double that throws the runtime's typed CAS-failure code from
 *  `update` to simulate the PR 4 path. */
function makeNativeOptimisticConcurrencyFailure() {
  const native = {
    registerModel: () => Promise.resolve(),
    collection(_name: string) {
      return {
        async update(_filter: AnyRec, _update: AnyRec) {
          const e = new Error(
            "Optimistic concurrency check failed for posts post_x: expected version 5",
          );
          (e as Error & { code: string }).code = "version_mismatch";
          throw e;
        },
      };
    },
  };
  return native as unknown as ZeroshipDb;
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
    // CRITICAL — the SDK must NOT add `$inc: { version: 1 }`. The
    // runtime's auto-bump owns the increment; SDK-side $inc would
    // double-bump.
    assert.equal(
      (captured.update as AnyRec | undefined)?.$inc,
      undefined,
      "SDK must not ship $inc:{version:1}; runtime owns the bump",
    );
  });

  test("update_with_stale_version_throws_OPTIMISTIC_CONCURRENCY", async () => {
    const native = makeNativeOptimisticConcurrencyFailure();
    const db = installSchemaForTest(
      {
        posts: schemaWrap({
          title: t.string().required(),
        }).withVersioning(),
      },
      { native },
    );
    const result = await db.posts.update(
      { id: "post_x", version: 5 } as never,
      { title: "renamed" },
    );
    // Runtime threw its typed optimistic-concurrency error; SDK rethrew as
    // `OptimisticLockError`; the `_run` rail catches + returns it via
    // `result.error`.
    assert.equal(result.data, null);
    assert.ok(result.error);
    assert.ok(
      result.error instanceof OptimisticLockError,
      `expected OptimisticLockError, got ${(result.error as Error).constructor.name}`,
    );
    assert.equal((result.error as OptimisticLockError).code, "OPTIMISTIC_CONCURRENCY");
    assert.equal((result.error as OptimisticLockError).expectedVersion, 5);
    // The `retryable: true` advisory flag must be set.
    assert.equal((result.error as OptimisticLockError).retryable, true);
  });

  test("update_without_version_filter_succeeds_blindly", async () => {
    // No `version` in filter → non-CAS path. The SDK must not throw
    // OptimisticLockError; the runtime's blind UPDATE returns a row.
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
    // Pure unit check on `OptimisticLockError`: the `retryable: true`
    // advisory flag is the SDK-side surface for the runtime's hint.
    const e = new OptimisticLockError(7, "posts");
    assert.equal(e.retryable, true);
    assert.equal(e.code, "OPTIMISTIC_CONCURRENCY");
    assert.equal(e.expectedVersion, 7);
  });
});
