import assert from "node:assert/strict";
import { test } from "node:test";
import { naming, t } from "../src/index.js";
import { installSchemaForTest } from "./_install-helper.js";
import type { NativeDb } from "../src/native.js";
import { Query } from "../src/query.js";

type Row = Record<string, unknown>;
type Call = { collection: string; filter: Row; opts: Row; transaction: boolean };

function fixture(rows: Row[], options: { snakeCase?: boolean; failure?: Error } = {}) {
  const calls: Call[] = [];
  let transaction = false;
  const native = {
    async transaction(callback: (raw: unknown) => unknown) {
      transaction = true;
      try { return await callback(undefined); }
      finally { transaction = false; }
    },
    collection(collection: string) {
      return {
        async find(filter: Row, opts: Row) {
          calls.push({ collection, filter, opts, transaction });
          assert.equal(collection, "posts", "the adapter must not query relation targets");
          if (options.failure) throw options.failure;
          return structuredClone(rows);
        },
      };
    },
  } as unknown as NativeDb;
  const db = installSchemaForTest({
    people: { displayName: t.string(), accountKey: t.string().unique(), payload: t.json() },
    posts: {
      title: t.string(),
      authorId: t.ref("people", { column: "accountKey", relation: "author" }),
      reviewerId: t.ref("people", { relation: "reviewer" }),
    },
  }, { native, naming: options.snakeCase ? naming.snakeCase : naming.asIs });
  return { db, calls };
}

const loaded = { id: "post_a", title: "notes", authorId: "author_a", author: { id: "person_a", accountKey: "author_a", displayName: "Ada" } };

for (const terminal of ["find", "get", "first", "unique", "last", "paginate"] as const) {
  test(`${terminal} forwards with once and consumes native relation rows`, async () => {
    const { db, calls } = fixture([loaded]);
    let result: unknown;
    if (terminal === "get") {
      const read = await db.posts.get("post_a", { with: { author: true } });
      assert.equal(read.error, null);
      result = read.data;
    } else {
      const query = db.posts.find({}, { with: { author: true } }).sort({ id: 1 });
      if (terminal === "find") {
        const read = await query;
        assert.equal(read.error, null);
        result = read.data?.[0];
      } else if (terminal === "paginate") {
        const read = await query.paginate({ numItems: 5 });
        assert.equal(read.error, null);
        result = read.data?.page[0];
      } else {
        const read = await query[terminal]();
        assert.equal(read.error, null);
        result = read.data;
      }
    }
    assert.deepEqual(result, loaded);
    assert.equal(calls.length, 1);
    assert.deepEqual(calls[0].opts.with, { author: true });
  });
}

test("chained with merges references and preserves native nulls", async () => {
  const row = { ...loaded, reviewerId: null, reviewer: null };
  const { db, calls } = fixture([row]);
  const result = await db.posts.find().with({ author: true }).with({ reviewer: true });
  assert.equal(result.error, null);
  assert.deepEqual(result.data, [row]);
  assert.deepEqual(calls.map(call => call.opts.with), [{ author: true, reviewer: true }]);
});

test("relation mappings rename declared target fields without changing JSON or mask payloads", async () => {
  const payload = { nested_key: { display_name: "untouched" } };
  const protectedName = { masked: "A***", classification: "pii", sentinel: "__zsmask__" };
  const nativeRow = {
    id: "post_a", author_id: "author_a", author: {
      id: "person_a", account_key: "author_a", display_name: protectedName,
      payload, unknown_native_key: "kept",
    },
  };
  const { db, calls } = fixture([nativeRow], { snakeCase: true });
  const result = await db.posts.find().with({ author: true });
  assert.equal(result.error, null);
  assert.deepEqual(result.data, [{
    id: "post_a", authorId: "author_a", author: {
      id: "person_a", accountKey: "author_a", displayName: protectedName,
      payload, unknown_native_key: "kept",
    },
  }]);
  assert.deepEqual(calls[0].opts.with, { author: true });
  assert.deepEqual(nativeRow.author.payload, payload);
});

test("empty parent results still forward relation validation to native", async () => {
  const { db, calls } = fixture([]);
  assert.deepEqual(await db.posts.find({}, { with: { author: true } }), { data: [], error: null });
  assert.equal(calls.length, 1);
  assert.deepEqual(calls[0].opts.with, { author: true });
});

test("native relation errors preserve their code through find and get", async () => {
  const failure = Object.assign(new Error("invalid reference"), { code: "unknown_relation" });
  const { db, calls } = fixture([], { failure });
  const query = await db.posts.find({}, { with: { author: true } });
  const single = await db.posts.get("post_a", { with: { author: true } });
  for (const result of [query, single]) {
    assert.equal(result.data, null);
    assert.equal((result.error as Error & { code: string }).code, failure.code);
  }
  assert.equal(calls.length, 2);
});

test("direct Query forwards relation options without an SDK loader", async () => {
  const calls: ZeroshipDbFindOpts[] = [];
  const query = new Query("posts", {}, async (_name, _filter, opts) => {
    calls.push(opts);
    return [loaded];
  });
  const result = await query.with({ author: true });
  assert.equal(result.error, null);
  assert.deepEqual(result.data, [loaded]);
  assert.deepEqual(calls, [{ with: { author: true } }]);
});

for (const terminal of ["find", "get", "paginate"] as const) {
  test(`transaction ${terminal} forwards relations through its native call`, async () => {
    const { db, calls } = fixture([loaded]);
    const result = await db.transaction(async tx => {
      if (terminal === "get") return tx.posts.get("post_a", { with: { author: true } });
      const query = tx.posts.find().with({ author: true });
      if (terminal === "paginate") return (await query.paginate({ numItems: 5 })).page[0];
      return (await query)[0];
    });
    assert.equal(result.error, null);
    assert.deepEqual(result.data, loaded);
    assert.equal(calls.length, 1);
    assert.equal(calls[0].transaction, true);
    assert.deepEqual(calls[0].opts.with, { author: true });
  });
}

for (const author of [{ id: "person_a", accountKey: "author_a" }, null]) {
  test(`pagination keeps a scalar FK cursor when its native relation is ${author === null ? "missing" : "loaded"}`, async () => {
    const { db, calls } = fixture([
      { id: "post_a", authorId: "author_a", author },
      { id: "post_b", authorId: "author_b", author: null },
    ]);
    const page = await db.posts.find().with({ author: true }).sort({ authorId: 1 }).paginate({ numItems: 1 });
    assert.equal(page.error, null);
    assert.equal(page.data!.page[0].authorId, "author_a");
    assert.deepEqual(page.data!.page[0].author, author);
    await db.posts.find().with({ author: true }).sort({ authorId: 1 }).paginate({ numItems: 1, cursor: page.data!.continueCursor });
    assert.deepEqual(calls[1].filter, { $or: [
      { authorId: { $gt: "author_a" } },
      { $and: [{ authorId: "author_a" }, { id: { $gt: "post_a" } }] },
    ] });
    assert.equal(calls.length, 2);
  });
}

for (const selectFirst of [true, false]) {
  test(`relation aliases survive selection with selectFirst=${selectFirst}`, async () => {
    const row = { id: "post_a", author: loaded.author };
    const { db, calls } = fixture([row]);
    const result = await (selectFirst
      ? db.posts.find().select(["id"]).with({ author: true })
      : db.posts.find().with({ author: true }).select(["id"]));
    assert.equal(result.error, null);
    assert.deepEqual(result.data, [row]);
    assert.deepEqual(calls[0].opts, { select: ["id"], with: { author: true } });
  });
}

test("reference builders retain scalar storage and named metadata through modifiers", () => {
  const integer = t.int().references("people", { column: "id", relation: "author" }).nullable().required();
  const bigint = t.bigInt().references("people", { column: "id", relation: "author" }).required();
  assert.deepEqual(integer.toFieldDef(), { type: "integer", refTarget: "people", refColumn: "id", relation: "author", required: true });
  assert.deepEqual(bigint.toFieldDef(), { type: "bigInt", refTarget: "people", refColumn: "id", relation: "author", required: true });
  assert.throws(() => t.number().references("people"), /reference storage/);
});

test("redeclaring a reference replaces its previous target options", () => {
  const field = t.ref("people", { column: "account_key", relation: "author", onDelete: "cascade" })
    .references("teams", { relation: "team" });
  assert.deepEqual(field.toFieldDef(), { type: "ref", refTarget: "teams", relation: "team" });
});
