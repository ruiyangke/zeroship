import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { Collection, NativeDb } from "../src/collection.js";
import { normalizeSchema } from "../src/schema.js";
import { t, naming } from "../src/types.js";
import { ValidationError } from "../src/errors.js";

// ---------------------------------------------------------------------------
// Schema & helpers
// ---------------------------------------------------------------------------

const schema = normalizeSchema({
  name: t.string().required(),
  age: t.number().min(0),
  role: t.string().default("user"),
  tags: t.array(t.string()),
});

type PlainObject = Record<string, unknown>;

interface CallRecord {
  method: string;
  args: unknown[];
}

function makeMockNative(overrides: Partial<Record<keyof NativeDb, unknown>> = {}): {
  native: NativeDb;
  calls: CallRecord[];
} {
  const calls: CallRecord[] = [];

  function record(method: string, args: unknown[], returnVal: unknown) {
    calls.push({ method, args });
    return Promise.resolve(returnVal);
  }

  const native: NativeDb = {
    insert: (col, doc) =>
      record("insert", [col, doc], JSON.stringify({ id: "abc123", ...(doc as PlainObject) })) as Promise<string>,
    insertMany: (col, docs) =>
      record(
        "insertMany",
        [col, docs],
        JSON.stringify(
          (docs as PlainObject[]).map((d, i) => ({ id: `id${i}`, ...d }))
        )
      ) as Promise<string>,
    findOne: (col, filter) =>
      record("findOne", [col, filter], JSON.stringify({ id: "xyz", name: "Alice", age: 30 })) as Promise<string | null>,
    find: (col, filter, opts) =>
      record("find", [col, filter, opts], JSON.stringify([{ id: "1", name: "Bob" }])) as Promise<string>,
    updateOne: (col, filter, update) =>
      record("updateOne", [col, filter, update], JSON.stringify({ id: "1", name: "Bob" })) as Promise<string>,
    updateMany: (col, filter, update) =>
      record("updateMany", [col, filter, update], JSON.stringify({ updated: 3 })) as Promise<string>,
    deleteOne: (col, filter) =>
      record("deleteOne", [col, filter], JSON.stringify({ id: "1" })) as Promise<string>,
    deleteMany: (col, filter) =>
      record("deleteMany", [col, filter], JSON.stringify({ deleted: 5 })) as Promise<string>,
    count: (col, filter) =>
      record("count", [col, filter], JSON.stringify({ count: 7 })) as Promise<string>,
    distinct: (col, field, filter) =>
      record("distinct", [col, field, filter], JSON.stringify(["admin", "user"])) as Promise<string>,
    aggregate: (col, pipeline) =>
      record(
        "aggregate",
        [col, pipeline],
        JSON.stringify([{ id: "g1", total: 100 }])
      ) as Promise<string>,
    upsert: (col, doc, conflictFields) =>
      record("upsert", [col, doc, conflictFields], JSON.stringify({ id: "u1", ...(doc as PlainObject) })) as Promise<string>,
    ...overrides,
  };

  return { native, calls };
}

// ---------------------------------------------------------------------------
// create()
// ---------------------------------------------------------------------------

describe("Collection.create()", () => {
  test("calls native.insert with correct collection and validated doc", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.create({ name: "Alice", age: 25 });
    assert.equal(calls[0].method, "insert");
    assert.equal((calls[0].args[0] as string), "users");
  });

  test("applies default value before inserting", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.create({ name: "Bob" });
    const doc = calls[0].args[1] as PlainObject;
    assert.equal(doc.role, "user");
  });

  test("returns doc with _id instead of id", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.create({ name: "Carol", age: 20 });
    assert.equal(error, null);
    assert.ok(data !== null && "id" in data);
    assert.equal(data.id, "abc123");
  });

  test("returns ValidationError for missing required field", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.create({ age: 20 });
    assert.equal(data, null);
    assert.ok(error instanceof ValidationError);
  });

  test("returns ValidationError for wrong type", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.create({ name: 42 as unknown as string });
    assert.equal(data, null);
    assert.ok(error instanceof ValidationError);
  });

  test("returns ValidationError when number is below min", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.create({ name: "X", age: -5 });
    assert.equal(data, null);
    assert.ok(error instanceof ValidationError);
  });

  test("maps native error to Error with code 11000 on duplicate", async () => {
    const { native } = makeMockNative({
      insert: () => Promise.reject(new Error("unique constraint violation")),
    });
    const col = new Collection("users", schema, native);
    const { data, error } = await col.create({ name: "Alice" });
    assert.equal(data, null);
    assert.ok(error !== null && (error as { code?: number }).code === 11000);
  });
});

// ---------------------------------------------------------------------------
// insertMany()
// ---------------------------------------------------------------------------

describe("Collection.insertMany()", () => {
  test("validates each doc and calls insertMany", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.insertMany([
      { name: "A", age: 1 },
      { name: "B", age: 2 },
    ]);
    assert.equal(calls[0].method, "insertMany");
    assert.equal((calls[0].args[1] as PlainObject[]).length, 2);
  });

  test("returns array with _id mapped", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.insertMany([{ name: "A" }, { name: "B" }]);
    assert.equal(error, null);
    assert.ok(data !== null);
    assert.equal(data.length, 2);
    assert.ok("id" in data[0]);
  });

  test("returns ValidationError if any doc is invalid", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.insertMany([{ name: "OK" }, { age: 10 }]);
    assert.equal(data, null);
    assert.ok(error instanceof ValidationError);
  });
});

// ---------------------------------------------------------------------------
// findOne()
// ---------------------------------------------------------------------------

describe("Collection.findOne()", () => {
  test("maps _id filter to id before calling native", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.findOne({ id: "abc" });
    const filter = calls[0].args[1] as PlainObject;
    assert.ok("id" in filter);
    assert.equal(filter.id, "abc");
  });

  test("returns null data when native returns null", async () => {
    const { native } = makeMockNative({
      findOne: () => Promise.resolve(null),
    });
    const col = new Collection("users", schema, native);
    const { data, error } = await col.findOne({ name: "Ghost" });
    assert.equal(error, null);
    assert.equal(data, null);
  });

  test("returns mapped doc (id → _id)", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.findOne({ name: "Alice" });
    assert.equal(error, null);
    assert.ok(data !== null);
    assert.equal(data.id, "xyz");
    assert.equal(data.name, "Alice");
  });
});

// ---------------------------------------------------------------------------
// find()
// ---------------------------------------------------------------------------

describe("Collection.find()", () => {
  test("returns a Query instance", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { Query } = await import("../src/query.js");
    const q = col.find({ active: true });
    assert.ok(q instanceof Query);
  });

  test("find() passes mapped filter to Query", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.find({ id: "123" });
    assert.equal(calls[0].method, "find");
    const filter = calls[0].args[1] as PlainObject;
    assert.equal(filter.id, "123");
  });

  test("find() result docs have _id", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.find({});
    assert.equal(error, null);
    assert.ok(data !== null && "id" in data[0]);
    assert.equal(data[0].id, "1");
  });
});

// ---------------------------------------------------------------------------
// updateOne()
// ---------------------------------------------------------------------------

describe("Collection.updateOne()", () => {
  test("calls native.updateOne with mapped filter", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.updateOne({ id: "1" }, { $set: { name: "NewName" } });
    assert.equal(calls[0].method, "updateOne");
    const filter = calls[0].args[1] as PlainObject;
    assert.equal(filter.id, "1");
  });

  test("returns { matchedCount: 1, modifiedCount: 1 } on match", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.updateOne({ name: "Alice" }, { $set: { age: 31 } });
    assert.equal(error, null);
    assert.deepEqual(data, { matchedCount: 1, modifiedCount: 1 });
  });

  test("returns { matchedCount: 0, modifiedCount: 0 } when no match", async () => {
    const { native } = makeMockNative({
      updateOne: () => Promise.resolve(null as unknown as string),
    });
    const col = new Collection("users", schema, native);
    const { data, error } = await col.updateOne({ name: "Ghost" }, { $set: { age: 5 } });
    assert.equal(error, null);
    assert.deepEqual(data, { matchedCount: 0, modifiedCount: 0 });
  });

  test("returns ValidationError for invalid $set fields", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.updateOne({ name: "Alice" }, { $set: { age: "not-a-number" as unknown as number } });
    assert.equal(data, null);
    assert.ok(error instanceof ValidationError);
  });

  test("returns ValidationError for invalid top-level (non-$) fields", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.updateOne({ name: "Alice" }, { age: "bad" as unknown as number });
    assert.equal(data, null);
    assert.ok(error instanceof ValidationError);
  });

  // C2: _id/createdAt/updatedAt mapping in $set
  test("maps _id → id inside $set update (flattened)", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.updateOne({ name: "Alice" }, { $set: { id: "new-id" } });
    const update = calls[0].args[2] as PlainObject;
    // $set is flattened: { $set: { id: "x" } } → { id: "x" }
    assert.equal(update.id, "new-id");
    assert.equal(update.$set, undefined);
  });

  test("maps createdAt → created_at inside $set update (flattened)", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native, { naming: naming.snakeCase });
    await col.updateOne({ name: "Alice" }, { $set: { createdAt: "2024-01-01" } });
    const update = calls[0].args[2] as PlainObject;
    assert.equal(update.created_at, "2024-01-01");
    assert.equal(update.createdAt, undefined);
  });

  test("maps updatedAt → updated_at inside $set update (flattened)", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native, { naming: naming.snakeCase });
    await col.updateOne({ name: "Alice" }, { $set: { updatedAt: "2024-06-01" } });
    const update = calls[0].args[2] as PlainObject;
    assert.equal(update.updated_at, "2024-06-01");
    assert.equal(update.updatedAt, undefined);
  });

  // I4: updateOne return value edge cases
  test("returns { matchedCount: 0 } when native returns empty string", async () => {
    const { native } = makeMockNative({
      updateOne: () => Promise.resolve("null"),
    });
    const col = new Collection("users", schema, native);
    const { data, error } = await col.updateOne({ name: "Ghost" }, { $set: { age: 5 } });
    assert.equal(error, null);
    assert.deepEqual(data, { matchedCount: 0, modifiedCount: 0 });
  });

  // C3: $push/$addToSet validation
  test("$push value validated against array item type: valid", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.updateOne({ name: "Alice" }, { $push: { tags: "newtag" } });
    assert.equal(error, null);
    assert.ok(data !== null);
  });

  test("$push value validated against array item type: invalid returns error", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.updateOne({ name: "Alice" }, { $push: { tags: 42 as unknown as string } });
    assert.equal(data, null);
    assert.ok(error instanceof ValidationError);
  });

  test("$addToSet value validated against array item type: invalid returns error", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.updateOne({ name: "Alice" }, { $addToSet: { tags: true as unknown as string } });
    assert.equal(data, null);
    assert.ok(error instanceof ValidationError);
  });
});

// ---------------------------------------------------------------------------
// updateMany()
// ---------------------------------------------------------------------------

describe("Collection.updateMany()", () => {
  test("returns { matchedCount: N, modifiedCount: N }", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.updateMany({ role: "user" }, { $set: { role: "member" } });
    assert.equal(error, null);
    assert.deepEqual(data, { matchedCount: 3, modifiedCount: 3 });
  });

  test("maps filter _id → id", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.updateMany({ id: "x" }, { $set: { name: "Y" } });
    const filter = calls[0].args[1] as PlainObject;
    assert.equal(filter.id, "x");
  });

  // C2: _id/createdAt/updatedAt mapping in $set for updateMany (flattened)
  test("maps _id → id inside $set in updateMany", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.updateMany({ role: "user" }, { $set: { id: "x" } });
    const update = calls[0].args[2] as PlainObject;
    assert.equal(update.id, "x");
    assert.equal(update.$set, undefined);
  });

  // C3: $push/$addToSet in updateMany
  test("$push invalid value in updateMany returns error", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.updateMany({ role: "user" }, { $push: { tags: 99 as unknown as string } });
    assert.equal(data, null);
    assert.ok(error instanceof ValidationError);
  });
});

// ---------------------------------------------------------------------------
// deleteOne()
// ---------------------------------------------------------------------------

describe("Collection.deleteOne()", () => {
  test("returns { deletedCount: 1 } when doc found", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.deleteOne({ name: "Alice" });
    assert.equal(error, null);
    assert.deepEqual(data, { deletedCount: 1 });
  });

  test("returns { deletedCount: 0 } when no doc", async () => {
    const { native } = makeMockNative({
      deleteOne: () => Promise.resolve(null as unknown as string),
    });
    const col = new Collection("users", schema, native);
    const { data, error } = await col.deleteOne({ name: "Ghost" });
    assert.equal(error, null);
    assert.deepEqual(data, { deletedCount: 0 });
  });

  test("maps _id → id in filter", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.deleteOne({ id: "abc" });
    const filter = calls[0].args[1] as PlainObject;
    assert.equal(filter.id, "abc");
  });
});

// ---------------------------------------------------------------------------
// deleteMany()
// ---------------------------------------------------------------------------

describe("Collection.deleteMany()", () => {
  test("returns { deletedCount: N } from native result", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.deleteMany({ role: "user" });
    assert.equal(error, null);
    assert.deepEqual(data, { deletedCount: 5 });
  });

  test("maps _id → id in filter", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.deleteMany({ id: "z" });
    const filter = calls[0].args[1] as PlainObject;
    assert.equal(filter.id, "z");
  });
});

// ---------------------------------------------------------------------------
// countDocuments()
// ---------------------------------------------------------------------------

describe("Collection.countDocuments()", () => {
  test("returns count from native", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.countDocuments({});
    assert.equal(error, null);
    assert.equal(data, 7);
  });

  test("maps _id → id in filter", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.countDocuments({ id: "x" });
    const filter = calls[0].args[1] as PlainObject;
    assert.equal(filter.id, "x");
  });

  test("no-arg call uses empty filter", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.countDocuments();
    assert.deepEqual(calls[0].args[1], {});
  });
});

// ---------------------------------------------------------------------------
// distinct()
// ---------------------------------------------------------------------------

describe("Collection.distinct()", () => {
  test("returns array of distinct values", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.distinct("role");
    assert.equal(error, null);
    assert.deepEqual(data, ["admin", "user"]);
  });

  test("passes field name to native", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.distinct("role", { active: true });
    assert.equal(calls[0].args[1], "role");
  });

  test("maps _id → id in filter", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.distinct("name", { id: "abc" });
    const filter = calls[0].args[2] as PlainObject;
    assert.equal(filter.id, "abc");
  });

  test("no-arg filter defaults to empty", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.distinct("role");
    assert.deepEqual(calls[0].args[2], {});
  });
});

// ---------------------------------------------------------------------------
// I5: createdAt/updatedAt outbound in filters
// ---------------------------------------------------------------------------

describe("Collection — createdAt/updatedAt filter mapping", () => {
  test("findOne maps createdAt → created_at in filter", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native, { naming: naming.snakeCase });
    await col.findOne({ createdAt: "2024-01-01" });
    const filter = calls[0].args[1] as PlainObject;
    assert.equal(filter.created_at, "2024-01-01");
    assert.equal(filter.createdAt, undefined);
  });

  test("countDocuments maps updatedAt → updated_at in filter", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native, { naming: naming.snakeCase });
    await col.countDocuments({ updatedAt: "2024-06-01" });
    const filter = calls[0].args[1] as PlainObject;
    assert.equal(filter.updated_at, "2024-06-01");
    assert.equal(filter.updatedAt, undefined);
  });
});

// ---------------------------------------------------------------------------
// aggregate()
// ---------------------------------------------------------------------------

describe("Collection.aggregate()", () => {
  test("translates $group stage and calls native.aggregate", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.aggregate([
      { $group: { id: "$role", count: { $sum: 1 } } },
    ]);
    assert.equal(calls[0].method, "aggregate");
    const pipeline = calls[0].args[1] as PlainObject[];
    const stage = pipeline[0].$group as PlainObject;
    assert.equal(stage.by, "role");
    assert.deepEqual(stage.count, { $count: true });
  });

  test("returns mapped docs (id → _id)", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.aggregate([{ $match: { role: "admin" } }]);
    assert.equal(error, null);
    assert.ok(data !== null && "id" in data[0]);
    assert.equal(data[0].id, "g1");
  });

  test("passes $match with mapped filter", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.aggregate([{ $match: { id: "abc" } }]);
    const pipeline = calls[0].args[1] as PlainObject[];
    const matchFilter = pipeline[0].$match as PlainObject;
    assert.equal(matchFilter.id, "abc");
  });
});

// ---------------------------------------------------------------------------
// Naming strategy — snakeCase
// ---------------------------------------------------------------------------

describe("Collection — naming strategy (snakeCase)", () => {
  test("create sends snake_case keys to native insert", async () => {
    const s = normalizeSchema({ firstName: t.string().required(), lastName: t.string() });
    const { native, calls } = makeMockNative();
    const col = new Collection("users", s, native, { naming: naming.snakeCase });
    await col.create({ firstName: "Alice" } as any);
    const doc = calls[0].args[1] as PlainObject;
    assert.equal(doc.first_name, "Alice");
    assert.equal(doc.firstName, undefined);
  });

  test("create maps snake_case result back to camelCase", async () => {
    const s = normalizeSchema({ firstName: t.string().required() });
    const { native } = makeMockNative({
      insert: (_col: string, _doc: unknown) =>
        Promise.resolve(JSON.stringify({ id: 1, first_name: "Alice", created_at: 1000, updated_at: 1000 })),
    });
    const col = new Collection("users", s, native, { naming: naming.snakeCase });
    const { data } = await col.create({ firstName: "Alice" } as any);
    assert.equal(data!.firstName, "Alice");
    assert.equal(data!.createdAt, 1000);
    assert.equal((data as any).first_name, undefined);
    assert.equal((data as any).created_at, undefined);
  });

  test("findOne sends snake_case filter keys", async () => {
    const s = normalizeSchema({ firstName: t.string() });
    const { native, calls } = makeMockNative();
    const col = new Collection("users", s, native, { naming: naming.snakeCase });
    await col.findOne({ firstName: "Alice" } as any);
    const filter = calls[0].args[1] as PlainObject;
    assert.equal(filter.first_name, "Alice");
    assert.equal(filter.firstName, undefined);
  });

  test("findOne maps snake_case result back to camelCase", async () => {
    const s = normalizeSchema({ firstName: t.string() });
    const { native } = makeMockNative({
      findOne: () => Promise.resolve(JSON.stringify({ id: 1, first_name: "Bob" })),
    });
    const col = new Collection("users", s, native, { naming: naming.snakeCase });
    const { data } = await col.findOne({} as any);
    assert.equal(data!.firstName, "Bob");
    assert.equal((data as any).first_name, undefined);
  });

  test("updateOne sends snake_case filter and update keys", async () => {
    const s = normalizeSchema({ firstName: t.string(), viewCount: t.number() });
    const { native, calls } = makeMockNative();
    const col = new Collection("users", s, native, { naming: naming.snakeCase });
    await col.updateOne({ firstName: "Alice" } as any, { viewCount: 5 } as any);
    const filter = calls[0].args[1] as PlainObject;
    const update = calls[0].args[2] as PlainObject;
    assert.equal(filter.first_name, "Alice");
    assert.equal(update.view_count, 5);
  });

  test("$inc operator keys are mapped to snake_case", async () => {
    const s = normalizeSchema({ viewCount: t.number() });
    const { native, calls } = makeMockNative();
    const col = new Collection("users", s, native, { naming: naming.snakeCase });
    await col.updateOne({ id: 1 } as any, { $inc: { viewCount: 1 } } as any);
    const update = calls[0].args[2] as PlainObject;
    assert.deepEqual(update.view_count, { $inc: 1 });
    assert.equal(update.viewCount, undefined);
  });

  test("distinct converts field name to snake_case", async () => {
    const s = normalizeSchema({ firstName: t.string() });
    const { native, calls } = makeMockNative();
    const col = new Collection("users", s, native, { naming: naming.snakeCase });
    await col.distinct("firstName" as any);
    assert.equal(calls[0].args[1], "first_name");
  });

  test("insertMany sends snake_case keys and maps results back", async () => {
    const s = normalizeSchema({ firstName: t.string().required() });
    const { native } = makeMockNative({
      insertMany: (_col: string, _docs: unknown) =>
        Promise.resolve(JSON.stringify([
          { id: 1, first_name: "Alice", created_at: 1000, updated_at: 1000 },
          { id: 2, first_name: "Bob", created_at: 2000, updated_at: 2000 },
        ])),
    });
    const col = new Collection("users", s, native, { naming: naming.snakeCase });
    const { data } = await col.insertMany([{ firstName: "Alice" }, { firstName: "Bob" }] as any);
    assert.equal(data![0].firstName, "Alice");
    assert.equal(data![1].firstName, "Bob");
    assert.equal(data![0].createdAt, 1000);
  });

  test("aggregate $match maps camelCase to snake_case", async () => {
    const s = normalizeSchema({ firstName: t.string() });
    const { native, calls } = makeMockNative();
    const col = new Collection("users", s, native, { naming: naming.snakeCase });
    await col.aggregate([{ $match: { firstName: "Alice" } }]);
    const pipeline = calls[0].args[1] as PlainObject[];
    const match = pipeline[0].$match as PlainObject;
    assert.equal(match.first_name, "Alice");
    assert.equal(match.firstName, undefined);
  });

  test("aggregate $sort maps camelCase keys to snake_case", async () => {
    const s = normalizeSchema({ createdAt: t.number() });
    const { native, calls } = makeMockNative();
    const col = new Collection("users", s, native, { naming: naming.snakeCase });
    await col.aggregate([{ $sort: { createdAt: -1 } }]);
    const pipeline = calls[0].args[1] as PlainObject[];
    assert.deepEqual(pipeline[0], { $sort: { created_at: -1 } });
  });

  test("asIs strategy passes field names unchanged", async () => {
    const s = normalizeSchema({ firstName: t.string().required() });
    const { native, calls } = makeMockNative();
    const col = new Collection("users", s, native, { naming: naming.asIs });
    await col.create({ firstName: "Alice" } as any);
    const doc = calls[0].args[1] as PlainObject;
    assert.equal(doc.firstName, "Alice");
    assert.equal(doc.first_name, undefined);
  });
});

// ---------------------------------------------------------------------------
// Native error envelope detection
// ---------------------------------------------------------------------------

describe("Collection — native error envelope", () => {
  test("create detects {error:...} from native and returns Result.error", async () => {
    const { native } = makeMockNative({
      insert: () => Promise.resolve(JSON.stringify({ error: "unique constraint violation" })),
    });
    const col = new Collection("users", schema, native);
    const { data, error } = await col.create({ name: "Alice" } as any);
    assert.equal(data, null);
    assert.ok(error !== null);
    assert.ok(error.message.includes("unique constraint violation"));
  });

  test("findOne detects {error:...} from native", async () => {
    const { native } = makeMockNative({
      findOne: () => Promise.resolve(JSON.stringify({ error: "permission denied" })),
    });
    const col = new Collection("users", schema, native);
    const { data, error } = await col.findOne({} as any);
    assert.equal(data, null);
    assert.ok(error !== null);
    assert.ok(error.message.includes("permission denied"));
  });

  test("updateOne detects {error:...} from native", async () => {
    const { native } = makeMockNative({
      updateOne: () => Promise.resolve(JSON.stringify({ error: "deadlock detected" })),
    });
    const col = new Collection("users", schema, native);
    const { data, error } = await col.updateOne({} as any, {} as any);
    assert.equal(data, null);
    assert.ok(error !== null);
    assert.ok(error.message.includes("deadlock"));
  });

  test("count detects {error:...} from native", async () => {
    const { native } = makeMockNative({
      count: () => Promise.resolve(JSON.stringify({ error: "table not found" })),
    });
    const col = new Collection("users", schema, native);
    const { data, error } = await col.countDocuments({} as any);
    assert.equal(data, null);
    assert.ok(error !== null);
    assert.ok(error.message.includes("table not found"));
  });
});

// ---------------------------------------------------------------------------
// Per-field operators not rejected by validation
// ---------------------------------------------------------------------------

describe("Collection — per-field operator validation", () => {
  test("updateOne with { views: { $inc: 1 } } per-field style succeeds", async () => {
    const s = normalizeSchema({ views: t.number() });
    const { native, calls } = makeMockNative();
    const col = new Collection("users", s, native);
    const { error } = await col.updateOne({} as any, { views: { $inc: 1 } } as any);
    assert.equal(error, null);
    assert.equal(calls[0].method, "updateOne");
  });

  test("updateOne with { tags: { $push: 'new' } } per-field style succeeds", async () => {
    const s = normalizeSchema({ tags: t.array(t.string()) });
    const { native, calls } = makeMockNative();
    const col = new Collection("users", s, native);
    const { error } = await col.updateOne({} as any, { tags: { $push: "new" } } as any);
    assert.equal(error, null);
    assert.equal(calls[0].method, "updateOne");
  });

  test("updateOne with { views: { $dec: 1 } } per-field style succeeds", async () => {
    const s = normalizeSchema({ views: t.number() });
    const { native, calls } = makeMockNative();
    const col = new Collection("users", s, native);
    const { error } = await col.updateOne({} as any, { views: { $dec: 1 } } as any);
    assert.equal(error, null);
  });

  test("updateOne still validates bare field values", async () => {
    const s = normalizeSchema({ age: t.number() });
    const { native } = makeMockNative();
    const col = new Collection("users", s, native);
    const { error } = await col.updateOne({} as any, { age: "not-a-number" } as any);
    assert.ok(error instanceof ValidationError);
  });
});

// ---------------------------------------------------------------------------
// insertMany empty array guard
// ---------------------------------------------------------------------------

describe("Collection — insertMany edge cases", () => {
  test("insertMany([]) returns ok([]) without calling native", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.insertMany([]);
    assert.equal(error, null);
    assert.deepEqual(data, []);
    assert.equal(calls.length, 0);
  });
});

// ---------------------------------------------------------------------------
// distinct field validation
// ---------------------------------------------------------------------------

describe("Collection — distinct field validation", () => {
  test("distinct with valid schema field succeeds", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { error } = await col.distinct("name" as any);
    assert.equal(error, null);
  });

  test("distinct with auto-field 'id' succeeds", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { error } = await col.distinct("id" as any);
    assert.equal(error, null);
  });

  test("distinct with auto-field 'createdAt' succeeds", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { error } = await col.distinct("createdAt" as any);
    assert.equal(error, null);
  });

  test("distinct with unknown field throws ValidationError", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { error } = await col.distinct("nonexistent" as any);
    assert.ok(error instanceof ValidationError);
    assert.ok(error.message.includes("nonexistent"));
  });
});

// ---------------------------------------------------------------------------
// findOneAndUpdate()
// ---------------------------------------------------------------------------

describe("Collection.findOneAndUpdate()", () => {
  test("returns the updated document on match", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.findOneAndUpdate({ name: "Alice" }, { $set: { age: 31 } });
    assert.equal(error, null);
    assert.ok(data !== null);
    assert.equal(data.id, "1");
    assert.equal(data.name, "Bob");
  });

  test("returns null when no document matches", async () => {
    const { native } = makeMockNative({
      updateOne: () => Promise.resolve(null as unknown as string),
    });
    const col = new Collection("users", schema, native);
    const { data, error } = await col.findOneAndUpdate({ name: "Ghost" }, { $set: { age: 5 } });
    assert.equal(error, null);
    assert.equal(data, null);
  });

  test("returns null when native returns 'null' string", async () => {
    const { native } = makeMockNative({
      updateOne: () => Promise.resolve("null"),
    });
    const col = new Collection("users", schema, native);
    const { data, error } = await col.findOneAndUpdate({ name: "Ghost" }, { $set: { age: 5 } });
    assert.equal(error, null);
    assert.equal(data, null);
  });

  test("calls native.updateOne with mapped filter and update", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.findOneAndUpdate({ id: "1" }, { $set: { name: "NewName" } });
    assert.equal(calls[0].method, "updateOne");
    const filter = calls[0].args[1] as PlainObject;
    assert.equal(filter.id, "1");
  });

  test("returns ValidationError for invalid $set fields", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.findOneAndUpdate(
      { name: "Alice" },
      { $set: { age: "not-a-number" as unknown as number } }
    );
    assert.equal(data, null);
    assert.ok(error instanceof ValidationError);
  });

  test("validates $push values against array item type", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.findOneAndUpdate(
      { name: "Alice" },
      { $push: { tags: 42 as unknown as string } }
    );
    assert.equal(data, null);
    assert.ok(error instanceof ValidationError);
  });
});

// ---------------------------------------------------------------------------
// findOneAndDelete()
// ---------------------------------------------------------------------------

describe("Collection.findOneAndDelete()", () => {
  test("returns the deleted document on match", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.findOneAndDelete({ name: "Alice" });
    assert.equal(error, null);
    assert.ok(data !== null);
    assert.equal(data.id, "1");
  });

  test("returns null when no document matches", async () => {
    const { native } = makeMockNative({
      deleteOne: () => Promise.resolve(null as unknown as string),
    });
    const col = new Collection("users", schema, native);
    const { data, error } = await col.findOneAndDelete({ name: "Ghost" });
    assert.equal(error, null);
    assert.equal(data, null);
  });

  test("returns null when native returns empty string", async () => {
    const { native } = makeMockNative({
      deleteOne: () => Promise.resolve(""),
    });
    const col = new Collection("users", schema, native);
    const { data, error } = await col.findOneAndDelete({ name: "Ghost" });
    assert.equal(error, null);
    assert.equal(data, null);
  });

  test("maps filter fields before calling native", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.findOneAndDelete({ id: "abc" });
    assert.equal(calls[0].method, "deleteOne");
    const filter = calls[0].args[1] as PlainObject;
    assert.equal(filter.id, "abc");
  });

  test("maps native error to Result error", async () => {
    const { native } = makeMockNative({
      deleteOne: () => Promise.reject(new Error("connection lost")),
    });
    const col = new Collection("users", schema, native);
    const { data, error } = await col.findOneAndDelete({ name: "Alice" });
    assert.equal(data, null);
    assert.ok(error !== null);
    assert.ok(error.message.includes("connection lost"));
  });
});

// ---------------------------------------------------------------------------
// upsert()
// ---------------------------------------------------------------------------

describe("Collection.upsert()", () => {
  test("calls native.upsert with validated doc and conflict fields", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.upsert({ name: "Alice", age: 25 }, { conflictFields: ["name"] });
    assert.equal(calls[0].method, "upsert");
    assert.equal(calls[0].args[0], "users");
    const doc = calls[0].args[1] as PlainObject;
    assert.equal(doc.name, "Alice");
    assert.equal(doc.age, 25);
    const conflictFields = calls[0].args[2] as string[];
    assert.deepEqual(conflictFields, ["name"]);
  });

  test("returns the upserted document with id mapped", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.upsert({ name: "Alice", age: 25 }, { conflictFields: ["name"] });
    assert.equal(error, null);
    assert.ok(data !== null);
    assert.equal(data.id, "u1");
    assert.equal(data.name, "Alice");
  });

  test("applies defaults before upserting", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.upsert({ name: "Bob" }, { conflictFields: ["name"] });
    const doc = calls[0].args[1] as PlainObject;
    assert.equal(doc.role, "user");
  });

  test("returns ValidationError for missing required field", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.upsert({ age: 20 }, { conflictFields: ["name"] });
    assert.equal(data, null);
    assert.ok(error instanceof ValidationError);
  });

  test("returns ValidationError for wrong type", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.upsert(
      { name: 42 as unknown as string },
      { conflictFields: ["name"] }
    );
    assert.equal(data, null);
    assert.ok(error instanceof ValidationError);
  });

  test("maps conflict fields through naming strategy", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native, { naming: naming.snakeCase });
    await col.upsert({ name: "Alice" }, { conflictFields: ["createdAt" as any] });
    const conflictFields = calls[0].args[2] as string[];
    assert.deepEqual(conflictFields, ["created_at"]);
  });

  test("maps native error to Error with code 11000 on duplicate", async () => {
    const { native } = makeMockNative({
      upsert: () => Promise.reject(new Error("unique constraint violation")),
    });
    const col = new Collection("users", schema, native);
    const { data, error } = await col.upsert({ name: "Alice" }, { conflictFields: ["name"] });
    assert.equal(data, null);
    assert.ok(error !== null && (error as { code?: number }).code === 11000);
  });
});

// ---------------------------------------------------------------------------
// Soft delete
// ---------------------------------------------------------------------------

describe("Collection — soft delete", () => {
  const sdSchema = normalizeSchema({ name: t.string().required(), role: t.string() });
  sdSchema.deletedAt = { type: "date", required: false };

  test("deleteOne with soft delete calls updateOne instead of deleteOne", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", sdSchema, native, { softDelete: true });
    await col.deleteOne({ name: "Alice" } as any);
    assert.equal(calls[0].method, "updateOne");
  });

  test("deleteMany with soft delete calls updateMany", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", sdSchema, native, { softDelete: true });
    await col.deleteMany({ role: "admin" } as any);
    assert.equal(calls[0].method, "updateMany");
  });

  test("findOne with soft delete adds deleted_at filter", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", sdSchema, native, { softDelete: true });
    await col.findOne({ name: "Alice" } as any);
    const filter = calls[0].args[1] as PlainObject;
    assert.ok("$and" in filter || "deletedAt" in filter || "deleted_at" in filter);
  });

  test("countDocuments with soft delete adds filter to empty query", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", sdSchema, native, { softDelete: true });
    await col.countDocuments({} as any);
    const filter = calls[0].args[1] as PlainObject;
    assert.ok(Object.keys(filter).length > 0);
  });

  test("without soft delete, deleteOne uses native deleteOne", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", sdSchema, native);
    await col.deleteOne({ name: "Alice" } as any);
    assert.equal(calls[0].method, "deleteOne");
  });

  test("without soft delete, findOne does not add extra filter", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", sdSchema, native);
    await col.findOne({ name: "Alice" } as any);
    const filter = calls[0].args[1] as PlainObject;
    assert.equal(filter.name, "Alice");
    assert.equal("$and" in filter, false);
  });

  test("forceDelete calls real native deleteOne", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", sdSchema, native, { softDelete: true });
    await col.forceDelete({ name: "Alice" } as any);
    assert.equal(calls[0].method, "deleteOne");
  });

  test("forceDeleteMany calls real native deleteMany", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", sdSchema, native, { softDelete: true });
    await col.forceDeleteMany({ role: "admin" } as any);
    assert.equal(calls[0].method, "deleteMany");
  });
});

// ---------------------------------------------------------------------------
// Soft Delete
// ---------------------------------------------------------------------------

describe("Collection — soft delete", () => {
  test("deleteOne with softDelete calls native.updateOne instead of deleteOne", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native, { softDelete: true });
    const { data, error } = await col.deleteOne({ name: "Alice" });
    assert.equal(error, null);
    assert.equal(calls[0].method, "updateOne");
    const update = calls[0].args[2] as PlainObject;
    assert.ok("deletedAt" in update, "update should contain deletedAt key");
    assert.equal(typeof update.deletedAt, "number");
    assert.deepEqual(data, { deletedCount: 1 });
  });

  test("deleteMany with softDelete calls native.updateMany instead of deleteMany", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native, { softDelete: true });
    const { data, error } = await col.deleteMany({ role: "user" });
    assert.equal(error, null);
    assert.equal(calls[0].method, "updateMany");
    const update = calls[0].args[2] as PlainObject;
    assert.ok("deletedAt" in update, "update should contain deletedAt key");
    assert.deepEqual(data, { deletedCount: 3 });
  });

  test("findOne with softDelete auto-filters deleted documents", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native, { softDelete: true });
    await col.findOne({ name: "Alice" });
    const filter = calls[0].args[1] as PlainObject;
    // Should merge soft-delete condition via $and
    assert.ok("$and" in filter, "filter should contain $and for soft-delete");
    const andArray = filter.$and as PlainObject[];
    assert.equal(andArray.length, 2);
    assert.equal(andArray[0].name, "Alice");
    assert.equal(andArray[1].deletedAt, null);
  });

  test("findOne with softDelete and empty filter uses just the soft-delete filter", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native, { softDelete: true });
    await col.findOne({});
    const filter = calls[0].args[1] as PlainObject;
    // Empty user filter → only soft-delete filter, no $and needed
    assert.equal(filter.deletedAt, null);
    assert.equal(filter.$and, undefined);
  });

  test("find with softDelete auto-filters deleted documents", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native, { softDelete: true });
    await col.find({ role: "admin" });
    const filter = calls[0].args[1] as PlainObject;
    assert.ok("$and" in filter);
    const andArray = filter.$and as PlainObject[];
    assert.equal(andArray[1].deletedAt, null);
  });

  test("countDocuments with softDelete auto-filters deleted documents", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native, { softDelete: true });
    await col.countDocuments({ role: "admin" });
    const filter = calls[0].args[1] as PlainObject;
    assert.ok("$and" in filter);
    const andArray = filter.$and as PlainObject[];
    assert.equal(andArray[1].deletedAt, null);
  });

  test("exists with softDelete auto-filters deleted documents", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native, { softDelete: true });
    await col.exists({ name: "Alice" });
    // exists delegates to countDocuments
    const filter = calls[0].args[1] as PlainObject;
    assert.ok("$and" in filter);
  });

  test("distinct with softDelete auto-filters deleted documents", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native, { softDelete: true });
    await col.distinct("role", { name: "Alice" });
    const filter = calls[0].args[2] as PlainObject;
    assert.ok("$and" in filter);
    const andArray = filter.$and as PlainObject[];
    assert.equal(andArray[1].deletedAt, null);
  });

  test("forceDelete bypasses soft delete and calls native.deleteOne", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native, { softDelete: true });
    const { data, error } = await col.forceDelete({ name: "Alice" });
    assert.equal(error, null);
    assert.equal(calls[0].method, "deleteOne");
    assert.deepEqual(data, { deletedCount: 1 });
  });

  test("forceDeleteMany bypasses soft delete and calls native.deleteMany", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native, { softDelete: true });
    const { data, error } = await col.forceDeleteMany({ role: "user" });
    assert.equal(error, null);
    assert.equal(calls[0].method, "deleteMany");
    assert.deepEqual(data, { deletedCount: 5 });
  });

  test("without softDelete, deleteOne still calls native.deleteOne", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.deleteOne({ name: "Alice" });
    assert.equal(calls[0].method, "deleteOne");
  });

  test("without softDelete, findOne does not inject soft-delete filter", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.findOne({ name: "Alice" });
    const filter = calls[0].args[1] as PlainObject;
    assert.equal(filter.$and, undefined);
    assert.equal(filter.name, "Alice");
  });

  test("findOneAndDelete with softDelete calls updateOne instead of deleteOne", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native, { softDelete: true });
    const { data, error } = await col.findOneAndDelete({ name: "Alice" });
    assert.equal(error, null);
    assert.equal(calls[0].method, "updateOne");
    assert.ok(data !== null);
    assert.equal(data.id, "1");
  });

  test("softDelete with snakeCase naming maps deletedAt to deleted_at", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native, { naming: naming.snakeCase, softDelete: true });
    await col.deleteOne({ name: "Alice" });
    assert.equal(calls[0].method, "updateOne");
    const update = calls[0].args[2] as PlainObject;
    assert.ok("deleted_at" in update, "update should use snake_case deleted_at");
    assert.equal(typeof update.deleted_at, "number");
  });

  test("softDelete with snakeCase naming filters with deleted_at in reads", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native, { naming: naming.snakeCase, softDelete: true });
    await col.findOne({ name: "Alice" });
    const filter = calls[0].args[1] as PlainObject;
    assert.ok("$and" in filter);
    const andArray = filter.$and as PlainObject[];
    assert.equal(andArray[1].deleted_at, null);
  });

  test("deleteOne with softDelete returns deletedCount 0 when no match", async () => {
    const { native } = makeMockNative({
      updateOne: () => Promise.resolve(null as unknown as string),
    });
    const col = new Collection("users", schema, native, { softDelete: true });
    const { data, error } = await col.deleteOne({ name: "Ghost" });
    assert.equal(error, null);
    assert.deepEqual(data, { deletedCount: 0 });
  });
});

// ---------------------------------------------------------------------------
// select() narrowing — runtime behavior
// ---------------------------------------------------------------------------

describe("Query.select() runtime", () => {
  test("select with array of fields stores them correctly", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.find({}).select(["name", "age"]);
    const opts = calls[0].args[2] as PlainObject;
    assert.deepEqual(opts.select, ["name", "age"]);
  });

  test("select with string stores parsed fields", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.find({}).select("name age");
    const opts = calls[0].args[2] as PlainObject;
    assert.deepEqual(opts.select, ["name", "age"]);
  });

  test("select with object stores inclusion keys", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.find({}).select({ name: 1, age: 1 });
    const opts = calls[0].args[2] as PlainObject;
    assert.deepEqual(opts.select, ["name", "age"]);
  });

  test("select returns results correctly", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { data, error } = await col.find({}).select(["name"]);
    assert.equal(error, null);
    assert.ok(data !== null);
    assert.ok(Array.isArray(data));
  });
});
