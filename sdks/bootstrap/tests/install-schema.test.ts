/**
 * Smoke tests for `@zeroship/bootstrap/install-schema`.
 *
 * The behavioural coverage of `installSchema`, `validateRefTargets`,
 * `normalizeSchema`, `expandUnionToFlatColumns`, `model`, and
 * descriptor installation lives in `sdks/db/tests/` (the existing test files
 * call into these helpers via the `_install-helper.ts` adapter and the
 * `@zeroship/bootstrap/install-schema` subpath). Those tests are the
 * source of truth for the moved code paths — Stage 7's hard rule was
 * "maintain test coverage", and the tests follow the helpers across
 * the package boundary.
 *
 * This file just sanity-checks the bootstrap package's public-to-
 * framework API surface so a fresh `pnpm -F @zeroship/bootstrap test`
 * has a non-zero count and a place to anchor future bootstrap-only
 * tests (dispatcher idempotency, normalizeUserModule edge cases, etc.).
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import {
  installSchema,
  validateRefTargets,
  normalizeSchema,
  expandUnionToFlatColumns,
  model,
} from "../src/install-schema.js";
import { normalizeUserModule } from "../src/normalize.js";
import { t, schema } from "@zeroship/db";

describe("@zeroship/bootstrap public surface", () => {
  test("installSchema is a function", () => {
    assert.equal(typeof installSchema, "function");
  });

  test("validateRefTargets is a function", () => {
    assert.equal(typeof validateRefTargets, "function");
  });

  test("normalizeSchema is a function", () => {
    assert.equal(typeof normalizeSchema, "function");
  });

  test("expandUnionToFlatColumns is a function", () => {
    assert.equal(typeof expandUnionToFlatColumns, "function");
  });

  test("model is a function", () => {
    assert.equal(typeof model, "function");
  });

  test("normalizeUserModule is a function", () => {
    assert.equal(typeof normalizeUserModule, "function");
  });
});

describe("normalizeSchema — minimal smoke", () => {
  test("turns a record of t.* builders into a NormalizedSchema", () => {
    const out = normalizeSchema({
      title: t.string().required(),
      done: t.boolean().default(false),
    });
    assert.equal(out.title.type, "string");
    assert.equal(out.title.required, true);
    assert.equal(out.done.type, "boolean");
    assert.equal(out.done.default, false);
  });
});

describe("normalizeSchema — P7 typed-id prefix (id: t.id(prefix))", () => {
  test("retains an id:t.id(prefix) field with its idPrefix", () => {
    const out = normalizeSchema({
      id: t.id("blog"),
      title: t.string(),
    });
    // The id prefix declaration survives normalization so the runtime
    // descriptor can carry the declared prefix.
    assert.equal(out.id.type, "id");
    assert.equal(out.id.idPrefix, "blog");
    assert.equal(out.title.type, "string");
  });

  test("familiar names retain their declared types", () => {
    assert.deepEqual(normalizeSchema({ id: t.string(), version: t.number() }), {
      id: {type:"string"}, version: {type:"number"},
    });
  });
});

describe("validateRefTargets — minimal smoke", () => {
  test("throws on a missing target collection", () => {
    try {
      validateRefTargets({
        posts: {
          title: t.string().required(),
          // simulates an `as any` escape past the TS check.
          authorId: t.ref("ghost"),
        },
      });
      assert.fail("expected throw");
    } catch (e) {
      const err = e as Error & { code?: string; target?: string };
      assert.equal(err.code, "REF_TARGET_NOT_FOUND");
      assert.equal(err.target, "ghost");
    }
  });

  test("accepts a ref to a declared collection", () => {
    assert.doesNotThrow(() => {
      validateRefTargets({
        users: { name: t.string().required() },
        posts: { title: t.string().required(), authorId: t.ref("users") },
      });
    });
  });
});

describe("normalizeUserModule — minimal smoke", () => {
  test("merges default.rpc with named exports (named wins)", () => {
    const mod = {
      default: { rpc: { dup: () => "fromDefault", onlyDef: () => "d" } },
      dup: () => "named",
      named: () => "named-ok",
    };
    const out = normalizeUserModule(mod);
    assert.equal(typeof out.rpc.dup, "function");
    assert.equal((out.rpc.dup as () => string)(), "named");
    assert.equal((out.rpc.onlyDef as () => string)(), "d");
    assert.equal((out.rpc.named as () => string)(), "named-ok");
  });

  test("picks default.fetch when present, falls back to top-level fetch", () => {
    const fetchFn = () => new Response("ok");
    const mod = { default: { fetch: fetchFn } };
    const out = normalizeUserModule(mod);
    assert.equal(out.fetch, fetchFn);

    const mod2 = { fetch: fetchFn };
    const out2 = normalizeUserModule(mod2);
    assert.equal(out2.fetch, fetchFn);
  });

  test("does not read or surface default.schema", () => {
    const def: Record<string, unknown> = {};
    Object.defineProperty(def, "schema", {
      get() {
        throw new Error("default.schema must not be read");
      },
    });
    const out = normalizeUserModule({ default: def });
    assert.equal("schema" in out, false);
  });
});

describe("installSchema — P4b migration-first descriptor source", () => {
  // The bundled RuntimeSchemaDescriptor (`schema.runtime.json`,
  // v2 `{ version, collections }`) is the schema source of truth when handed
  // to installSchema via `options.descriptor`. These
  // pin: (a) collections come FROM the descriptor (not the declared t.*
  // object); (b) an absent descriptor installs nothing instead of
  // falling back to the declared schema; (c) a present but non-v2 descriptor
  // is a hard boot error.
  function makeMockNative() {
    return {
      transaction(cb: (raw: unknown) => unknown) { return cb(undefined); },
      collection(_n: string) { return { async find() { return []; } }; },
    } as unknown as ZeroshipDb;
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
    } as unknown as ZeroshipDb;
    const descriptor = {
      version: 2,
      collections: Object.fromEntries(
        ["posts", "transaction", "collection", "from"].map(name => [
          name,
          {
            fields: {
              id: { type: "id", idPrefix: "row", required: true, primaryKey: true },
              value: { type: "string", required: true },
            },
            options: { softDelete: false, versioning: false, strictness: "strict" },
            indexes: [],
          },
        ]),
      ),
    };

    assert.doesNotThrow(() => {
      installSchema({} as never, native, { descriptor } as never);
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

  test("sources collections FROM the descriptor, ignoring the declared t.* object", () => {
    const native = makeMockNative();
    // Descriptor: platform-generated wire FieldDefs (snake_case columns,
    // system fields included). DIFFERENT collection name than the declared
    // object so the source is unambiguous.
    const descriptor = {
      version: 2,
      collections: {
        posts: {
          fields: {
            id: { type: "id", idPrefix: "post", required: true, primaryKey: true },
            title: { type: "string", required: true },
            created_at: { type: "date" },
          },
          options: { softDelete: false, versioning: false, strictness: "strict" },
          indexes: [],
        },
      },
    };
    installSchema(
      // Declared t.* object — MUST be ignored when the descriptor is present.
      { todos: { title: t.string().required() } } as never,
      native,
      { descriptor } as never,
    );
    const handle = native as unknown as Record<string, unknown>;
    assert.ok(handle.posts, "descriptor collection `posts` planted on env.db");
    assert.equal(
      handle.todos,
      undefined,
      "declared `todos` must NOT be planted when a descriptor supersedes it",
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
    } as unknown as ZeroshipDb;
    const descriptor = {
      version: 2,
      collections: {
        hits: {
          fields: {
            id: { type: "string", required: true, primaryKey:true, assign:{by:"typedId", on:"insert"} },
            created_at: { type: "date", required: true, assign:{by:"now", on:"insert"} },
            updated_at: { type: "date", required: true, assign:{by:"now", on:"write"} },
            version: { type: "int", required: true, assign:{by:"increment(1)", on:"write"} },
            path: { type: "string", required: true },
          },
          options: { softDelete: false, versioning: false, strictness: "strict" },
          indexes: [],
        },
      },
    };
    installSchema({} as never, native, { descriptor } as never);

    const handle = native as unknown as Record<
      string,
      { insert(row: { path: string }): Promise<{ data: unknown; error: Error | null }> }
    >;
    const res = await handle.hits.insert({ path: "/hit/ready" });

    assert.equal(res.error, null);
    assert.deepEqual(inserted, [{ path: "/hit/ready" }]);
  });

  test("installs no collections when no descriptor is supplied", async () => {
    const native = makeMockNative();
    installSchema(
      { todos: { title: t.string().required() } },
      native,
    );
    const handle = native as unknown as Record<string, unknown>;
    assert.equal(handle.todos, undefined, "declared `todos` is ignored without a descriptor");
  });

  test("throws when the descriptor is not v2-shaped", () => {
    const native = makeMockNative();
    assert.throws(
      () =>
        installSchema(
          { todos: { title: t.string().required() } },
          native,
          { descriptor: {} } as never,
        ),
      (err: unknown) => {
        assert.equal((err as { code?: string }).code, "INVALID_RUNTIME_DESCRIPTOR");
        assert.match((err as Error).message, /RuntimeSchemaDescriptor/);
        return true;
      },
    );
  });

  // A native env.db whose `collection(name)` records which mutating op it
  // routed to (soft delete → `update`; hard delete → `delete`).
  function makeOpRecordingNative(ops: Array<{ name: string; op: string }>) {
    return {
      transaction(cb: (raw: unknown) => unknown) { return cb(undefined); },
      collection(name: string) {
        return {
          update(_filter: unknown, _patch: unknown) {
            ops.push({ name, op: "update" });
            // null result on a CAS update surfaces OptimisticLockError when
            // versioning is live; otherwise a plain `null`.
            return Promise.resolve(null);
          },
          delete(_filter: unknown) {
            ops.push({ name, op: "delete" });
            return Promise.resolve(null);
          },
          async find() { return []; },
        };
      },
    } as unknown as ZeroshipDb;
  }

  test("reads collection options and indexes directly from descriptor v2", async () => {
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
    } as unknown as ZeroshipDb;
    const descriptor = {
      version: 2,
      collections: {
        posts: {
          fields: {
            id: { type: "id", idPrefix: "post", required: true, primaryKey:true, assign:{by:"typedId", on:"insert"} },
            revision: {type:"integer", concurrency:true, assign:{by:"increment(1)", on:"write"}},
            removed: {type:"timestamp", softDelete:true, assign:{by:"now", on:"delete"}},
            title: { type: "string", required: true },
            status: { type: "string", required: true },
          },
          options: { softDelete: true, versioning: true, strictness: "lenient" },
          indexes: [{ name: "posts_title_status_idx", fields: ["title", "status"] }],
        },
      },
    };
    installSchema(
      {} as never,
      native,
      { descriptor } as never,
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

  test("does not recover legacy descriptor options from declaredSchemas", () => {
    const ops: Array<{ name: string; op: string }> = [];
    const native = makeOpRecordingNative(ops);
    const descriptor = {
      posts: {
        id: { type: "id", idPrefix: "post", required: true, primaryKey: true },
        title: { type: "string", required: true },
      },
    };
    assert.throws(
      () =>
        installSchema(
          descriptor as never,
          native,
          {
            descriptor,
            declaredSchemas: {
              posts: schema({ title: t.string().required() }).softDelete(),
            },
          } as never,
        ),
      (err: unknown) => {
        assert.equal((err as { code?: string }).code, "INVALID_RUNTIME_DESCRIPTOR");
        return true;
      },
    );
    assert.deepEqual(ops, []);
  });
});
