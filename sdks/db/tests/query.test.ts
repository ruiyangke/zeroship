import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { Query } from "../src/query.js";
import { naming } from "../src/types.js";

type PlainObject = Record<string, unknown>;

function makeMockNative(rows: PlainObject[]) {
  const calls: { collection: string; filter: PlainObject; opts: PlainObject }[] = [];
  const fn = async (
    collection: string,
    filter: PlainObject,
    opts: PlainObject
  ): Promise<string> => {
    calls.push({ collection, filter, opts });
    return JSON.stringify(rows);
  };
  return { fn, calls };
}

describe("Query chain building", () => {
  test("sort() returns this", () => {
    const { fn } = makeMockNative([]);
    const q = new Query("users", {}, fn);
    assert.equal(q.sort({ name: 1 }), q);
  });

  test("limit() returns this", () => {
    const { fn } = makeMockNative([]);
    const q = new Query("users", {}, fn);
    assert.equal(q.limit(10), q);
  });

  test("skip() returns this", () => {
    const { fn } = makeMockNative([]);
    const q = new Query("users", {}, fn);
    assert.equal(q.skip(20), q);
  });

  test("select() returns this", () => {
    const { fn } = makeMockNative([]);
    const q = new Query("users", {}, fn);
    assert.equal(q.select("name email"), q);
  });

  test("chains are independent per instance", () => {
    const { fn } = makeMockNative([]);
    const q1 = new Query("users", {}, fn).limit(5);
    const q2 = new Query("users", {}, fn).limit(10);
    assert.notEqual(q1, q2);
  });
});

describe("Query select parsing", () => {
  test("select string: space-separated → array in opts", async () => {
    const { fn, calls } = makeMockNative([]);
    const q = new Query("users", {}, fn).select("name email age");
    await q._exec();
    assert.deepEqual(calls[0].opts.select, ["name", "email", "age"]);
  });

  test("select array: passed through as-is", async () => {
    const { fn, calls } = makeMockNative([]);
    const q = new Query("users", {}, fn).select(["name", "email"]);
    await q._exec();
    assert.deepEqual(calls[0].opts.select, ["name", "email"]);
  });

  test("select single field string", async () => {
    const { fn, calls } = makeMockNative([]);
    const q = new Query("users", {}, fn).select("name");
    await q._exec();
    assert.deepEqual(calls[0].opts.select, ["name"]);
  });
});

describe("Query thenable execution", () => {
  test("await Query calls native with correct args", async () => {
    const { fn, calls } = makeMockNative([]);
    const filter = { active: true };
    const q = new Query("users", filter, fn).sort({ name: 1 }).limit(5).skip(10);
    await q;
    assert.equal(calls[0].collection, "users");
    assert.deepEqual(calls[0].filter, { active: true });
    assert.deepEqual(calls[0].opts.orderBy, { name: 1 });
    assert.equal(calls[0].opts.limit, 5);
    assert.equal(calls[0].opts.offset, 10);
  });

  test("await Query maps result docs (id → _id)", async () => {
    const { fn } = makeMockNative([
      { id: "1", name: "Alice" },
      { id: "2", name: "Bob" },
    ]);
    const q = new Query("users", {}, fn);
    const { data, error } = await q;
    assert.equal(error, null);
    assert.ok(data !== null);
    assert.equal(data[0].id, "1");
    assert.equal(data[0].name, "Alice");
    assert.equal(data[1].id, "2");
  });

  test("await Query maps created_at → createdAt", async () => {
    const { fn } = makeMockNative([
      { id: "1", created_at: "2024-01-01", updated_at: "2024-06-01" },
    ]);
    const q = new Query("docs", {}, fn, naming.snakeCase.toField);
    const { data, error } = await q;
    assert.equal(error, null);
    assert.ok(data !== null);
    assert.equal(data[0].createdAt, "2024-01-01");
    assert.equal(data[0].updatedAt, "2024-06-01");
  });

  test("Query without options sends empty opts", async () => {
    const { fn, calls } = makeMockNative([]);
    const q = new Query("items", {}, fn);
    await q;
    assert.deepEqual(calls[0].opts, {});
  });

  test("Query.then is thenable (Promise.resolve compatibility)", async () => {
    const { fn } = makeMockNative([{ id: "x" }]);
    const q = new Query("items", {}, fn);
    const { data, error } = await Promise.resolve(q);
    assert.equal(error, null);
    assert.ok(data !== null);
    assert.equal(data[0].id, "x");
  });
});

// ---------------------------------------------------------------------------
// opts key names (must match Rust: orderBy, offset, limit, select)
// ---------------------------------------------------------------------------

describe("Query opts key names", () => {
  test("sort() produces orderBy key (not sort)", async () => {
    const { fn, calls } = makeMockNative([]);
    const q = new Query("users", {}, fn).sort({ name: 1 });
    await q;
    assert.ok("orderBy" in calls[0].opts);
    assert.equal(calls[0].opts.sort, undefined);
    assert.deepEqual(calls[0].opts.orderBy, { name: 1 });
  });

  test("skip() produces offset key (not skip)", async () => {
    const { fn, calls } = makeMockNative([]);
    const q = new Query("users", {}, fn).skip(20);
    await q;
    assert.ok("offset" in calls[0].opts);
    assert.equal(calls[0].opts.skip, undefined);
    assert.equal(calls[0].opts.offset, 20);
  });

  test("limit() produces limit key", async () => {
    const { fn, calls } = makeMockNative([]);
    const q = new Query("users", {}, fn).limit(10);
    await q;
    assert.equal(calls[0].opts.limit, 10);
  });

  test("select() produces select key", async () => {
    const { fn, calls } = makeMockNative([]);
    const q = new Query("users", {}, fn).select("name email");
    await q;
    assert.deepEqual(calls[0].opts.select, ["name", "email"]);
  });

  test("bare find() sends empty opts", async () => {
    const { fn, calls } = makeMockNative([]);
    const q = new Query("users", {}, fn);
    await q;
    assert.deepEqual(calls[0].opts, {});
  });

  test("string sort parses correctly", async () => {
    const { fn, calls } = makeMockNative([]);
    const q = new Query("users", {}, fn).sort("-createdAt name");
    await q;
    assert.deepEqual(calls[0].opts.orderBy, { createdAt: -1, name: 1 });
  });
});

// ---------------------------------------------------------------------------
// Query with naming strategy toField
// ---------------------------------------------------------------------------

describe("Query with toField", () => {
  test("maps snake_case result to camelCase when toField provided", async () => {
    const { fn } = makeMockNative([
      { id: 1, first_name: "Alice", created_at: 1000 },
    ]);
    const q = new Query("users", {}, fn, naming.snakeCase.toField);
    const { data } = await q;
    assert.equal(data![0].firstName, "Alice");
    assert.equal(data![0].createdAt, 1000);
    assert.equal((data![0] as any).first_name, undefined);
  });

  test("without toField, keys pass through unchanged", async () => {
    const { fn } = makeMockNative([{ id: 1, first_name: "Alice" }]);
    const q = new Query("users", {}, fn);
    const { data } = await q;
    assert.equal((data![0] as any).first_name, "Alice");
    assert.equal(data![0].firstName, undefined);
  });
});

// ---------------------------------------------------------------------------
// select() exclusion rejection
// ---------------------------------------------------------------------------

describe("Query select exclusion", () => {
  test("inclusion projection works", () => {
    const { fn } = makeMockNative([]);
    assert.doesNotThrow(() => new Query("u", {}, fn).select({ name: 1, email: 1 }));
  });

  test("exclusion projection throws", () => {
    const { fn } = makeMockNative([]);
    assert.throws(
      () => new Query("u", {}, fn).select({ password: 0, secret: 0 }),
      /exclusion projections/
    );
  });

  test("mixed projection keeps truthy keys", async () => {
    const { fn, calls } = makeMockNative([]);
    const q = new Query("u", {}, fn).select({ name: 1, password: 0 });
    await q;
    assert.deepEqual(calls[0].opts.select, ["name"]);
  });
});
