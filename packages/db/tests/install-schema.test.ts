/** DB facade installation and schema authoring contracts. */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import {
  installSchema,
  type NativeDb,
} from "../../../crates/zeroship-data-v8/js/testing.js";
import { t } from "../src/index.js";

describe("installSchema — decoded projection source", () => {
  // The Rust host decodes and normalizes the descriptor, then hands the
  // adapter `{ collections: { name: { fields, indexes } } }`. These pin:
  // collections come from the projection, an absent projection installs
  // nothing, and the declared t.* object is never consulted.
  function makeMockNative() {
    return {
      transaction(cb: (raw: unknown) => unknown) { return cb(undefined); },
      collection(_n: string) { return { async find() { return []; } }; },
    } as unknown as NativeDb;
  }

  test("transaction name lookup reaches collections that collide with db APIs", async () => {
    const writes: Array<{ name: string; row: unknown }> = [];
    const native = {
      transaction(callback: (raw: unknown) => unknown) {
        const rawTx = {
          collection(name: string) {
            return native.collection(name);
          },
        };
        return Promise.resolve(callback(rawTx));
      },
      collection(name: string) {
        return {
          insert(row: unknown) {
            writes.push({ name, row });
            return Promise.resolve(row);
          },
          async find() { return []; },
        };
      },
    } as unknown as NativeDb;
    const projection = {
      collections: Object.fromEntries(
        ["posts", "transaction", "collection", "from"].map(name => [
          name,
          {
            fields: {
              id: { type: "id", idPrefix: "row", required: true, primaryKey: true },
              value: { type: "string", required: true },
            },
            indexes: [],
          },
        ]),
      ),
    };

    assert.doesNotThrow(() => {
      installSchema(native, projection as never);
    });

    const db = native as unknown as {
      transaction<R>(callback: (tx: {
        posts: {
          insert(row: unknown): Promise<unknown>;
        };
        collection(name: string): {
          insert(row: unknown): Promise<unknown>;
        };
      }) => Promise<R>): Promise<{ data: R | null; error: Error | null }>;
    };
    const result = await db.transaction(async tx => {
      assert.equal(tx.posts, tx.collection("posts"));
      const transactionTable = tx.collection("transaction");
      assert.equal(transactionTable, tx.collection("transaction"));
      await transactionTable.insert({ id: "row_1", value: "transaction" });
      await tx.collection("collection").insert({ id: "row_2", value: "collection" });
      await tx.collection("from").insert({ id: "row_3", value: "from" });
      return "committed";
    });

    assert.equal(result.error, null);
    assert.equal(result.data, "committed");
    assert.deepEqual(writes.map(write => write.name), ["transaction", "collection", "from"]);
  });

  test("sources collections FROM the projection, ignoring the declared t.* object", () => {
    const native = makeMockNative();
    // Projection: platform-decoded wire FieldDefs (snake_case columns,
    // system fields included). DIFFERENT collection name than the declared
    // object so the source is unambiguous.
    const projection = {
      collections: {
        posts: {
          fields: {
            id: { type: "id", idPrefix: "post", required: true, primaryKey: true },
            title: { type: "string", required: true },
            created_at: { type: "timestamp" },
          },
          indexes: [],
        },
      },
    };
    installSchema(
      native, projection as never,
    );
    const handle = native as unknown as Record<string, unknown>;
    assert.ok(handle.posts, "projection collection `posts` planted on env.db");
    assert.equal(
      handle.todos,
      undefined,
      "only projection collections are installed",
    );
  });

  test("does not require assigned descriptor fields in insert input", async () => {
    const inserted: unknown[] = [];
    const native = {
      transaction(cb: (raw: unknown) => unknown) { return cb(undefined); },
      collection() {
        return {
          insert(row: unknown) {
            inserted.push(row);
            return Promise.resolve({
              id: "hit_1",
              created_at: Date.now(),
              updated_at: Date.now(),
              created_by: null,
              updated_by: null,
              version: 1,
              deleted_at: null,
              ...(row as Record<string, unknown>),
            });
          },
          async find() { return []; },
        };
      },
    } as unknown as NativeDb;
    const projection = {
      collections: {
        hits: {
          fields: {
            id: { type: "string", required: true, primaryKey:true, assign:{by:"typedId", on:"insert"} },
            created_at: { type: "timestamp", required: true, assign:{by:"now", on:"insert"} },
            updated_at: { type: "timestamp", required: true, assign:{by:"now", on:"write"} },
            version: { type: "integer", required: true, assign:{by:"increment(1)", on:"write"} },
            path: { type: "string", required: true },
          },
          indexes: [],
        },
      },
    };
    installSchema(native, projection as never);

    const handle = native as unknown as Record<
      string,
      { insert(row: { path: string }): Promise<{ data: unknown; error: Error | null }> }
    >;
    const res = await handle.hits.insert({ path: "/hit/ready" });

    assert.equal(res.error, null);
    assert.deepEqual(inserted, [{ path: "/hit/ready" }]);
  });

  test("installs no collections when no projection is supplied", async () => {
    const native = makeMockNative();
    installSchema(
      native, undefined,
    );
    const handle = native as unknown as Record<string, unknown>;
    assert.equal(handle.todos, undefined, "no collection is installed without a projection");
  });

  test("reads indexes and field facets directly from the projection", async () => {
    const ops: Array<{ name: string; op: string }> = [];
    const native = {
      transaction(cb: (raw: unknown) => unknown) { return cb(undefined); },
      collection(name: string) {
        return {
          update() {
            ops.push({ name, op: "update" });
            return Promise.resolve(null);
          },
          delete() {
            ops.push({ name, op: "delete" });
            return Promise.resolve(null);
          },
          async find() { return []; },
        };
      },
    } as unknown as NativeDb;
    const projection = {
      collections: {
        posts: {
          fields: {
            id: { type: "id", idPrefix: "post", required: true, primaryKey:true, assign:{by:"typedId", on:"insert"} },
            revision: {type:"integer", concurrency:true, assign:{by:"increment(1)", on:"write"}},
            removed: {type:"timestamp", softDelete:true, assign:{by:"now", on:"delete"}},
            title: { type: "string", required: true },
            status: { type: "string", required: true },
          },
          indexes: [{ name: "posts_title_status_idx", fields: ["title", "status"] }],
        },
      },
    };
    installSchema(
      native, projection as never,
    );
    const handle = native as unknown as Record<
      string,
      {
        delete(id: string): Promise<unknown>;
        update(
          idOrFilter: unknown,
          patch: unknown,
        ): Promise<{ data: unknown; error: { code?: string } | null }>;
      }
    >;
    assert.deepEqual(
      (handle.posts as unknown as { _indexes: unknown })._indexes,
      [{ name: "posts_title_status_idx", fields: ["title", "status"] }],
    );
    await handle.posts.delete("post_abc");
    assert.deepEqual(
      ops,
      [{ name: "posts", op: "delete" }],
      "delete dispatches the native lifecycle operation",
    );

    const res = await handle.posts.update({ id: "post_abc", revision: 1 }, { title: "x" });
    assert.equal(res.error?.code, "OPTIMISTIC_CONCURRENCY");
  });
});
