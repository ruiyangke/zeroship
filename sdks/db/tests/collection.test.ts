import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { Collection, NativeDb } from "../src/collection.js";
import { normalizeSchema } from "../src/schema.js";
import { t } from "../src/types.js";
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
    const result = await col.create({ name: "Carol", age: 20 });
    assert.ok("_id" in result);
    assert.equal(result._id, "abc123");
  });

  test("throws ValidationError for missing required field", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    await assert.rejects(
      () => col.create({ age: 20 }),
      (err: unknown) => err instanceof ValidationError
    );
  });

  test("throws ValidationError for wrong type", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    await assert.rejects(
      () => col.create({ name: 42 as unknown as string }),
      (err: unknown) => err instanceof ValidationError
    );
  });

  test("throws ValidationError when number is below min", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    await assert.rejects(
      () => col.create({ name: "X", age: -5 }),
      (err: unknown) => err instanceof ValidationError
    );
  });

  test("maps native error to Error with code 11000 on duplicate", async () => {
    const { native } = makeMockNative({
      insert: () => Promise.reject(new Error("unique constraint violation")),
    });
    const col = new Collection("users", schema, native);
    await assert.rejects(
      () => col.create({ name: "Alice" }),
      (err: unknown) => (err as { code?: number }).code === 11000
    );
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
    const results = await col.insertMany([{ name: "A" }, { name: "B" }]);
    assert.equal(results.length, 2);
    assert.ok("_id" in results[0]);
  });

  test("throws ValidationError if any doc is invalid", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    await assert.rejects(
      () => col.insertMany([{ name: "OK" }, { age: 10 }]),
      (err: unknown) => err instanceof ValidationError
    );
  });
});

// ---------------------------------------------------------------------------
// findOne()
// ---------------------------------------------------------------------------

describe("Collection.findOne()", () => {
  test("maps _id filter to id before calling native", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.findOne({ _id: "abc" });
    const filter = calls[0].args[1] as PlainObject;
    assert.ok("id" in filter);
    assert.equal(filter.id, "abc");
    assert.ok(!("_id" in filter));
  });

  test("returns null when native returns null", async () => {
    const { native } = makeMockNative({
      findOne: () => Promise.resolve(null),
    });
    const col = new Collection("users", schema, native);
    const result = await col.findOne({ name: "Ghost" });
    assert.equal(result, null);
  });

  test("returns mapped doc (id → _id)", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const result = await col.findOne({ name: "Alice" });
    assert.ok(result !== null);
    assert.equal(result._id, "xyz");
    assert.equal(result.name, "Alice");
  });
});

// ---------------------------------------------------------------------------
// find()
// ---------------------------------------------------------------------------

describe("Collection.find()", () => {
  test("returns a Query instance", () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const { Query } = require("../src/query.js");
    const q = col.find({ active: true });
    assert.ok(q instanceof Query);
  });

  test("find() passes mapped filter to Query", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.find({ _id: "123" });
    assert.equal(calls[0].method, "find");
    const filter = calls[0].args[1] as PlainObject;
    assert.equal(filter.id, "123");
    assert.ok(!("_id" in filter));
  });

  test("find() result docs have _id", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const results = await col.find({});
    assert.ok("_id" in results[0]);
    assert.equal(results[0]._id, "1");
  });
});

// ---------------------------------------------------------------------------
// updateOne()
// ---------------------------------------------------------------------------

describe("Collection.updateOne()", () => {
  test("calls native.updateOne with mapped filter", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.updateOne({ _id: "1" }, { $set: { name: "NewName" } });
    assert.equal(calls[0].method, "updateOne");
    const filter = calls[0].args[1] as PlainObject;
    assert.equal(filter.id, "1");
  });

  test("returns { matchedCount: 1, modifiedCount: 1 } on match", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const result = await col.updateOne({ name: "Alice" }, { $set: { age: 31 } });
    assert.deepEqual(result, { matchedCount: 1, modifiedCount: 1 });
  });

  test("returns { matchedCount: 0, modifiedCount: 0 } when no match", async () => {
    const { native } = makeMockNative({
      updateOne: () => Promise.resolve(null as unknown as string),
    });
    const col = new Collection("users", schema, native);
    const result = await col.updateOne({ name: "Ghost" }, { $set: { age: 5 } });
    assert.deepEqual(result, { matchedCount: 0, modifiedCount: 0 });
  });

  test("validates $set fields", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    await assert.rejects(
      () => col.updateOne({ name: "Alice" }, { $set: { age: "not-a-number" as unknown as number } }),
      (err: unknown) => err instanceof ValidationError
    );
  });

  test("validates top-level (non-$) fields", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    await assert.rejects(
      () => col.updateOne({ name: "Alice" }, { age: "bad" as unknown as number }),
      (err: unknown) => err instanceof ValidationError
    );
  });

  // C2: _id/createdAt/updatedAt mapping in $set
  test("maps _id → id inside $set update", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.updateOne({ name: "Alice" }, { $set: { _id: "new-id" } });
    const update = calls[0].args[2] as PlainObject;
    const setFields = update.$set as PlainObject;
    assert.equal(setFields.id, "new-id");
    assert.equal(setFields._id, undefined);
  });

  test("maps createdAt → created_at inside $set update", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.updateOne({ name: "Alice" }, { $set: { createdAt: "2024-01-01" } });
    const update = calls[0].args[2] as PlainObject;
    const setFields = update.$set as PlainObject;
    assert.equal(setFields.created_at, "2024-01-01");
    assert.equal(setFields.createdAt, undefined);
  });

  test("maps updatedAt → updated_at inside $set update", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.updateOne({ name: "Alice" }, { $set: { updatedAt: "2024-06-01" } });
    const update = calls[0].args[2] as PlainObject;
    const setFields = update.$set as PlainObject;
    assert.equal(setFields.updated_at, "2024-06-01");
    assert.equal(setFields.updatedAt, undefined);
  });

  // I4: updateOne return value edge cases
  test("returns { matchedCount: 0 } when native returns empty string", async () => {
    const { native } = makeMockNative({
      updateOne: () => Promise.resolve("null"),
    });
    const col = new Collection("users", schema, native);
    const result = await col.updateOne({ name: "Ghost" }, { $set: { age: 5 } });
    assert.deepEqual(result, { matchedCount: 0, modifiedCount: 0 });
  });

  // C3: $push/$addToSet validation
  test("$push value validated against array item type: valid", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    await assert.doesNotReject(
      () => col.updateOne({ name: "Alice" }, { $push: { tags: "newtag" } })
    );
  });

  test("$push value validated against array item type: invalid throws", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    await assert.rejects(
      () => col.updateOne({ name: "Alice" }, { $push: { tags: 42 as unknown as string } }),
      (err: unknown) => err instanceof ValidationError
    );
  });

  test("$addToSet value validated against array item type: invalid throws", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    await assert.rejects(
      () => col.updateOne({ name: "Alice" }, { $addToSet: { tags: true as unknown as string } }),
      (err: unknown) => err instanceof ValidationError
    );
  });
});

// ---------------------------------------------------------------------------
// updateMany()
// ---------------------------------------------------------------------------

describe("Collection.updateMany()", () => {
  test("returns { matchedCount: N, modifiedCount: N }", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const result = await col.updateMany({ role: "user" }, { $set: { role: "member" } });
    assert.deepEqual(result, { matchedCount: 3, modifiedCount: 3 });
  });

  test("maps filter _id → id", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.updateMany({ _id: "x" }, { $set: { name: "Y" } });
    const filter = calls[0].args[1] as PlainObject;
    assert.equal(filter.id, "x");
  });

  // C2: _id/createdAt/updatedAt mapping in $set for updateMany
  test("maps _id → id inside $set in updateMany", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.updateMany({ role: "user" }, { $set: { _id: "x" } });
    const update = calls[0].args[2] as PlainObject;
    const setFields = update.$set as PlainObject;
    assert.equal(setFields.id, "x");
    assert.equal(setFields._id, undefined);
  });

  // C3: $push/$addToSet in updateMany
  test("$push invalid value in updateMany throws", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    await assert.rejects(
      () => col.updateMany({ role: "user" }, { $push: { tags: 99 as unknown as string } }),
      (err: unknown) => err instanceof ValidationError
    );
  });
});

// ---------------------------------------------------------------------------
// deleteOne()
// ---------------------------------------------------------------------------

describe("Collection.deleteOne()", () => {
  test("returns { deletedCount: 1 } when doc found", async () => {
    const { native } = makeMockNative();
    const col = new Collection("users", schema, native);
    const result = await col.deleteOne({ name: "Alice" });
    assert.deepEqual(result, { deletedCount: 1 });
  });

  test("returns { deletedCount: 0 } when no doc", async () => {
    const { native } = makeMockNative({
      deleteOne: () => Promise.resolve(null as unknown as string),
    });
    const col = new Collection("users", schema, native);
    const result = await col.deleteOne({ name: "Ghost" });
    assert.deepEqual(result, { deletedCount: 0 });
  });

  test("maps _id → id in filter", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.deleteOne({ _id: "abc" });
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
    const result = await col.deleteMany({ role: "user" });
    assert.deepEqual(result, { deletedCount: 5 });
  });

  test("maps _id → id in filter", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.deleteMany({ _id: "z" });
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
    const n = await col.countDocuments({});
    assert.equal(n, 7);
  });

  test("maps _id → id in filter", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.countDocuments({ _id: "x" });
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
    const values = await col.distinct("role");
    assert.deepEqual(values, ["admin", "user"]);
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
    await col.distinct("name", { _id: "abc" });
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
    const col = new Collection("users", schema, native);
    await col.findOne({ createdAt: "2024-01-01" });
    const filter = calls[0].args[1] as PlainObject;
    assert.equal(filter.created_at, "2024-01-01");
    assert.equal(filter.createdAt, undefined);
  });

  test("countDocuments maps updatedAt → updated_at in filter", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
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
      { $group: { _id: "$role", count: { $sum: 1 } } },
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
    const results = await col.aggregate([{ $match: { role: "admin" } }]);
    assert.ok("_id" in results[0]);
    assert.equal(results[0]._id, "g1");
  });

  test("passes $match with mapped filter", async () => {
    const { native, calls } = makeMockNative();
    const col = new Collection("users", schema, native);
    await col.aggregate([{ $match: { _id: "abc" } }]);
    const pipeline = calls[0].args[1] as PlainObject[];
    const matchFilter = pipeline[0].$match as PlainObject;
    assert.equal(matchFilter.id, "abc");
    assert.ok(!("_id" in matchFilter));
  });
});
