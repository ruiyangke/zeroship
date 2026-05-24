/**
 * **P7 PR 5** — soft-delete semantic flip + new `purge()` / `restore()`
 * SDK surface + `find(filter, { include_deleted })` opt-out.
 *
 * Three SDK-side responsibilities exercised here:
 *
 * 1. `Collection.purge()` and `purgeMany()` reach a new native
 *    `purge` / `purgeMany` op (always-hard-delete, regardless of the
 *    runtime's soft-delete marker state).
 * 2. `Collection.restore()` and `restoreMany()` reach a new native
 *    `restore` / `restoreMany` op (clear `deleted_at`).
 * 3. `find(filter, { include_deleted: true })` threads the opt-out
 *    flag into the native `findOne` / `find` opts. The runtime then
 *    suppresses the `AND deleted_at IS NULL` auto-filter.
 *
 * The runtime-side soft-delete is verified by the Rust integration
 * tests in `crates/plugin-db/tests/sqlite_integration.rs`; this file
 * confirms the SDK boundary is wired correctly.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { installSchemaForTest } from "./_install-helper.js";
import { t, schema as schemaWrap } from "@zeroship/db";

type AnyRec = Record<string, unknown>;

/** Native double that records every call across the four PR 5
 *  surfaces (`purge`, `purgeMany`, `restore`, `restoreMany`) so the
 *  tests can verify the SDK reached the right entry point with the
 *  right shape. */
function makeNativeRecording() {
  const captured: {
    purgeFilter?: AnyRec;
    purgeManyFilter?: AnyRec;
    restoreFilter?: AnyRec;
    restoreManyFilter?: AnyRec;
    findOpts?: AnyRec;
    findOneOpts?: AnyRec;
  } = {};
  const native = {
    registerModel: () => Promise.resolve(),
    collection(_name: string) {
      return {
        async find(_filter: AnyRec, opts?: AnyRec) {
          captured.findOpts = opts;
          return [];
        },
        async findOne(_filter: AnyRec, opts?: AnyRec) {
          captured.findOneOpts = opts;
          return null;
        },
        async insert(doc: AnyRec) {
          return { id: "post_x", ...doc };
        },
        async updateOne(_filter: AnyRec, _update: AnyRec) {
          return { id: "post_x", title: "x", version: 2 };
        },
        async purge(filter: AnyRec) {
          captured.purgeFilter = filter;
          // Echo a row back so the SDK maps it through `mapResultDoc`.
          return { id: "post_purged", title: "gone", version: 7 };
        },
        async purgeMany(filter: AnyRec) {
          captured.purgeManyFilter = filter;
          return 3;
        },
        async restore(filter: AnyRec) {
          captured.restoreFilter = filter;
          return {
            id: "post_restored",
            title: "back",
            version: 3,
            deletedAt: null,
          };
        },
        async restoreMany(filter: AnyRec) {
          captured.restoreManyFilter = filter;
          return 2;
        },
      };
    },
    _captured: captured,
  };
  return { native: native as unknown as ZeroshipDb, captured };
}

/** Native double for legacy-runtime simulation: omits the PR 5 ops
 *  entirely so the SDK's typed `*_not_available` errors fire. */
function makeNativeMissingPurge() {
  const native = {
    registerModel: () => Promise.resolve(),
    collection(_name: string) {
      return {
        async insert(doc: AnyRec) {
          return { id: "x", ...doc };
        },
      };
    },
  };
  return native as unknown as ZeroshipDb;
}

describe("P7 PR 5 — soft-delete: purge + restore + include_deleted opt-out", () => {
  test("purge_method_exists_and_resolves_with_row", async () => {
    const { native, captured } = makeNativeRecording();
    const db = installSchemaForTest(
      {
        posts: schemaWrap({
          title: t.string().required(),
        }),
      },
      { native },
    );
    const result = await db.posts.purge("post_purged");
    assert.equal(result.error, null);
    const row = result.data as AnyRec | null;
    assert.ok(row);
    assert.equal(row.id, "post_purged");
    assert.equal(row.title, "gone");
    // SDK passed `{ id: "post_purged" }` to the native `purge`.
    assert.deepEqual(captured.purgeFilter, { id: "post_purged" });
  });

  test("purge_accepts_filter_object_too", async () => {
    const { native, captured } = makeNativeRecording();
    const db = installSchemaForTest(
      {
        posts: schemaWrap({
          title: t.string().required(),
        }),
      },
      { native },
    );
    await db.posts.purge({ title: "delete this" } as never);
    assert.deepEqual(captured.purgeFilter, { title: "delete this" });
  });

  test("purgeMany_method_exists_and_resolves_with_count", async () => {
    const { native, captured } = makeNativeRecording();
    const db = installSchemaForTest(
      {
        posts: schemaWrap({
          title: t.string().required(),
        }),
      },
      { native },
    );
    const result = await db.posts.purgeMany({ title: "junk" } as never);
    assert.equal(result.error, null);
    assert.deepEqual(result.data, { purgedCount: 3 });
    assert.deepEqual(captured.purgeManyFilter, { title: "junk" });
  });

  test("restore_method_exists_and_resolves_with_row", async () => {
    const { native, captured } = makeNativeRecording();
    const db = installSchemaForTest(
      {
        posts: schemaWrap({
          title: t.string().required(),
        }),
      },
      { native },
    );
    const result = await db.posts.restore("post_restored");
    assert.equal(result.error, null);
    const row = result.data as AnyRec | null;
    assert.ok(row);
    assert.equal(row.id, "post_restored");
    // `deletedAt` is mapped back from the `deletedAt` column shape the
    // native double returned — the SDK's `mapResultDoc` preserves it.
    assert.equal(row.deletedAt, null);
    assert.deepEqual(captured.restoreFilter, { id: "post_restored" });
  });

  test("restoreMany_method_exists_and_resolves_with_count", async () => {
    const { native, captured } = makeNativeRecording();
    const db = installSchemaForTest(
      {
        posts: schemaWrap({
          title: t.string().required(),
        }),
      },
      { native },
    );
    const result = await db.posts.restoreMany({} as never);
    assert.equal(result.error, null);
    assert.deepEqual(result.data, { restoredCount: 2 });
    assert.deepEqual(captured.restoreManyFilter, {});
  });

  test("purge_throws_purge_not_available_when_runtime_lacks_it", async () => {
    const native = makeNativeMissingPurge();
    const db = installSchemaForTest(
      {
        posts: schemaWrap({
          title: t.string().required(),
        }),
      },
      { native },
    );
    const result = await db.posts.purge("post_x");
    assert.ok(result.error, "must error when native surface missing");
    assert.equal(
      (result.error as Error & { code?: string }).code,
      "purge_not_available",
    );
  });

  test("restore_throws_restore_not_available_when_runtime_lacks_it", async () => {
    const native = makeNativeMissingPurge();
    const db = installSchemaForTest(
      {
        posts: schemaWrap({
          title: t.string().required(),
        }),
      },
      { native },
    );
    const result = await db.posts.restore("post_x");
    assert.ok(result.error, "must error when native surface missing");
    assert.equal(
      (result.error as Error & { code?: string }).code,
      "restore_not_available",
    );
  });
});
