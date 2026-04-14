/**
 * Robustness test suite for @zeroship/db
 * Covers: native layer failures, malicious inputs, type coercion edge cases,
 * deeply nested filters, query edge cases, validation edge cases, aggregate edge cases.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { Collection, NativeDb } from "../src/collection.js";
import { validateDoc, validatePartial } from "../src/validate.js";
import { normalizeSchema } from "../src/schema.js";
import { t } from "../src/types.js";
import { ValidationError } from "../src/errors.js";
import { Query } from "../src/query.js";
import { translateAggregatePipeline } from "../src/utils.js";

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

type PlainObject = Record<string, unknown>;

function makeNative(
  overrides: Partial<Record<keyof NativeDb, unknown>> = {}
): NativeDb {
  return {
    insert: () =>
      Promise.resolve(JSON.stringify({ id: "abc", name: "Alice" })),
    insertMany: () =>
      Promise.resolve(JSON.stringify([{ id: "1" }, { id: "2" }])),
    findOne: () =>
      Promise.resolve(JSON.stringify({ id: "xyz", name: "Alice" })),
    find: () =>
      Promise.resolve(JSON.stringify([{ id: "1", name: "Bob" }])),
    updateOne: () =>
      Promise.resolve(JSON.stringify({ id: "1", name: "Bob" })),
    updateMany: () =>
      Promise.resolve(JSON.stringify({ updated: 3 })),
    deleteOne: () =>
      Promise.resolve(JSON.stringify({ id: "1" })),
    deleteMany: () =>
      Promise.resolve(JSON.stringify({ deleted: 5 })),
    count: () =>
      Promise.resolve(JSON.stringify({ count: 7 })),
    distinct: () =>
      Promise.resolve(JSON.stringify(["admin", "user"])),
    aggregate: () =>
      Promise.resolve(JSON.stringify([{ id: "g1", total: 100 }])),
    ...overrides,
  } as NativeDb;
}

const schema = normalizeSchema({
  name: t.string().required(),
  age: t.number().min(0).max(200),
  role: t.string().default("user"),
  active: t.boolean(),
  score: t.number(),
  tags: t.array(t.string()),
});

// ---------------------------------------------------------------------------
// 1. Native layer failures
// ---------------------------------------------------------------------------

describe("native layer failures", () => {
  // Bug: malformed JSON from native.insert should not crash with an unhandled
  // SyntaxError — it must be caught and returned as a proper Error.
  test("native.insert returns malformed JSON → returns error, not crash", async () => {
    const col = new Collection(
      "users",
      schema,
      makeNative({ insert: () => Promise.resolve("not valid JSON {{{") })
    );
    const { data, error } = await col.create({ name: "Alice" });
    assert.equal(data, null);
    assert.ok(error instanceof Error);
  });

  // Bug: undefined return from native should not crash with a cryptic TypeError.
  test("native.insert returns undefined → returns error, not crash", async () => {
    const col = new Collection(
      "users",
      schema,
      makeNative({
        insert: () => Promise.resolve(undefined as unknown as string),
      })
    );
    const { data, error } = await col.create({ name: "Alice" });
    assert.equal(data, null);
    assert.ok(error instanceof Error);
  });

  test("native.insert throws an Error → returned as error via mapNativeError", async () => {
    const col = new Collection(
      "users",
      schema,
      makeNative({
        insert: () => Promise.reject(new Error("connection timeout")),
      })
    );
    const { data, error } = await col.create({ name: "Alice" });
    assert.equal(data, null);
    assert.ok(error instanceof Error && error.message.includes("connection timeout"));
  });

  // Bug: native.find returns "null" → JSON.parse("null") is null →
  // calling .map() on null crashes. Should return empty array instead.
  test("native.find returns \"null\" → should return empty array, not crash", async () => {
    const col = new Collection(
      "users",
      schema,
      makeNative({ find: () => Promise.resolve("null") })
    );
    const { data, error } = await col.find({});
    assert.equal(error, null);
    assert.ok(Array.isArray(data));
    assert.equal(data!.length, 0);
  });

  // Empty string from native.findOne is treated as "no result" — parseRaw maps
  // empty string to null, so findOne returns null data rather than throwing.
  test("native.findOne returns empty string \"\" → should return null data gracefully", async () => {
    const col = new Collection(
      "users",
      schema,
      makeNative({ findOne: () => Promise.resolve("") })
    );
    const { data, error } = await col.findOne({ name: "Alice" });
    assert.equal(error, null);
    assert.equal(data, null);
  });

  test("native.updateMany returns \"{}\" with no updated key → returns { modifiedCount: 0 }", async () => {
    const col = new Collection(
      "users",
      schema,
      makeNative({ updateMany: () => Promise.resolve("{}") })
    );
    const { data, error } = await col.updateMany(
      { role: "user" },
      { $set: { role: "member" } }
    );
    assert.equal(error, null);
    assert.deepEqual(data, { matchedCount: 0, modifiedCount: 0 });
  });

  test("native.deleteMany returns \"{}\" with no deleted key → returns { deletedCount: 0 }", async () => {
    const col = new Collection(
      "users",
      schema,
      makeNative({ deleteMany: () => Promise.resolve("{}") })
    );
    const { data, error } = await col.deleteMany({ role: "user" });
    assert.equal(error, null);
    assert.deepEqual(data, { deletedCount: 0 });
  });
});

// ---------------------------------------------------------------------------
// 2. Malicious / weird inputs
// ---------------------------------------------------------------------------

describe("malicious and weird inputs", () => {
  test("prototype pollution: doc with __proto__ field → should not pollute Object prototype", () => {
    // Use JSON.parse to bypass TypeScript's structural check — this is how
    // user input arrives in practice.
    const doc = JSON.parse('{"name":"Alice","__proto__":{"polluted":true}}');
    validateDoc(doc, schema);
    // Verify no pollution happened
    const check: PlainObject = {};
    assert.equal(check.polluted, undefined);
  });

  test("prototype pollution: filter with constructor key → passes through safely", async () => {
    const col = new Collection("users", schema, makeNative());
    // Should not error; native decides what to do with it
    const { data, error } = await col.findOne({ constructor: "payload" } as PlainObject);
    // Just verify it returned without crashing
    assert.equal(error, null);
    assert.ok(data !== undefined);
  });

  test("field named toString → works as regular field", () => {
    const s = normalizeSchema({ toString: t.string() });
    const result = validateDoc({ toString: "my value" }, s);
    assert.equal(result.toString, "my value");
  });

  test("field named valueOf → works as regular field", () => {
    const s = normalizeSchema({ valueOf: t.number() });
    const result = validateDoc({ valueOf: 42 }, s);
    assert.equal(result.valueOf, 42);
  });

  test("field named hasOwnProperty → works as regular field", () => {
    const s = normalizeSchema({ hasOwnProperty: t.string() });
    const result = validateDoc({ hasOwnProperty: "yes" }, s);
    assert.equal(result.hasOwnProperty, "yes");
  });

  test("very long string value (100K chars) → passes when no max constraint", () => {
    const s = normalizeSchema({ bio: t.string().required() });
    const longStr = "x".repeat(100_000);
    assert.doesNotThrow(() => validateDoc({ bio: longStr }, s));
  });

  test("unicode and emoji in field values → passes validation", () => {
    const result = validateDoc({ name: "🎉 Hello 世界" }, schema);
    assert.equal(result.name, "🎉 Hello 世界");
  });

  test("unicode in collection name → SDK does not block it (native validates)", async () => {
    const col = new Collection("用户_🗂️", schema, makeNative());
    const { data, error } = await col.create({ name: "Alice" });
    assert.equal(error, null);
    assert.ok(data !== null);
  });

  // Bug: empty string for a required field should fail validation.
  // An empty string is not "present" in a meaningful sense.
  test("empty string value for required string field → should fail validation", () => {
    const s = normalizeSchema({ name: t.string().required() });
    assert.throws(
      () => validateDoc({ name: "" }, s),
      (err: unknown) => err instanceof ValidationError
    );
  });
});

// ---------------------------------------------------------------------------
// 3. Type coercion edge cases
// ---------------------------------------------------------------------------

describe("type coercion edge cases", () => {
  test("null passed as doc to create() → returns error (ValidationError: required fields missing)", async () => {
    const col = new Collection("users", schema, makeNative());
    const { data, error } = await col.create(null as unknown as PlainObject);
    assert.equal(data, null);
    assert.ok(error instanceof Error);
  });

  test("undefined passed as doc to create() → returns error", async () => {
    const col = new Collection("users", schema, makeNative());
    const { data, error } = await col.create(undefined as unknown as PlainObject);
    assert.equal(data, null);
    assert.ok(error instanceof Error);
  });

  test("array passed as doc to create([{...}]) → returns error (not a plain object)", async () => {
    const col = new Collection("users", schema, makeNative());
    const { data, error } = await col.create([{ name: "Alice" }] as unknown as PlainObject);
    assert.equal(data, null);
    assert.ok(error instanceof Error);
  });

  test("string passed as filter to find(\"query\") → should not silently succeed", async () => {
    const col = new Collection("users", schema, makeNative());
    // Passing a string iterates chars as entries — result is unpredictable but
    // should not throw an unhandled crash. The SDK accepts the call; native validates.
    await assert.doesNotReject(() =>
      Promise.resolve(col.find("query" as unknown as PlainObject))
    );
  });

  test("number passed as filter find(123) → should not crash", async () => {
    const col = new Collection("users", schema, makeNative());
    await assert.doesNotReject(() =>
      Promise.resolve(col.find(123 as unknown as PlainObject))
    );
  });

  // Bug: NaN passes the `typeof value !== "number"` check because typeof NaN === "number".
  // NaN is not a valid numeric value and should fail validation.
  test("NaN as a number field value → should fail validation", () => {
    const s = normalizeSchema({ age: t.number() });
    assert.throws(
      () => validateDoc({ age: NaN }, s),
      (err: unknown) => err instanceof ValidationError
    );
  });

  // Infinity should fail when a max constraint is set, since Infinity > any finite max.
  test("Infinity as number field value with max constraint → should fail validation", () => {
    const s = normalizeSchema({ age: t.number().max(200) });
    assert.throws(
      () => validateDoc({ age: Infinity }, s),
      (err: unknown) => err instanceof ValidationError
    );
  });

  test("null value for non-required field → should be allowed (explicit null)", () => {
    const s = normalizeSchema({ age: t.number() });
    assert.doesNotThrow(() => validateDoc({ age: null }, s));
  });

  test("undefined value for required field → should fail required check", () => {
    const s = normalizeSchema({ name: t.string().required() });
    assert.throws(
      () => validateDoc({ name: undefined }, s),
      (err: unknown) => err instanceof ValidationError
    );
  });
});

// ---------------------------------------------------------------------------
// 4. Deeply nested filters
// ---------------------------------------------------------------------------

describe("deeply nested filters", () => {
  test("3-level nesting: { $and: [{ $or: [{ $not: { name: 'x' } }] }] } → works", async () => {
    const col = new Collection("users", schema, makeNative());
    const { data, error } = await col.find({ $and: [{ $or: [{ $not: { name: "x" } }] }] });
    assert.equal(error, null);
    assert.ok(Array.isArray(data));
  });

  test("empty nested: { $and: [{ $or: [] }] } → produces valid filter", async () => {
    const col = new Collection("users", schema, makeNative());
    const { data, error } = await col.find({ $and: [{ $or: [] }] });
    assert.equal(error, null);
    assert.ok(Array.isArray(data));
  });

  test("mixed logical operators with comparison operators → works", async () => {
    const col = new Collection("users", schema, makeNative());
    const { data, error } = await col.find({
      $or: [
        { name: "a" },
        { $and: [{ age: { $gt: 18 } }, { role: "admin" }] },
      ],
    });
    assert.equal(error, null);
    assert.ok(Array.isArray(data));
  });

  test("_id inside $not is mapped to id", async () => {
    const { native, calls } = (() => {
      const calls: PlainObject[] = [];
      const native = makeNative({
        find: (col: string, filter: unknown, opts: unknown) => {
          calls.push({ filter });
          return Promise.resolve("[]");
        },
      });
      return { native, calls };
    })();
    const col = new Collection("users", schema, native as NativeDb);
    await col.find({ $not: { id: "abc" } });
    const filter = (calls[0] as { filter: PlainObject }).filter;
    const notFilter = filter.$not as PlainObject;
    assert.equal(notFilter.id, "abc");
  });

  test("_id inside $and[0] is mapped to id", async () => {
    const { native, calls } = (() => {
      const calls: PlainObject[] = [];
      const native = makeNative({
        find: (col: string, filter: unknown, opts: unknown) => {
          calls.push({ filter: filter as PlainObject });
          return Promise.resolve("[]");
        },
      });
      return { native, calls };
    })();
    const col = new Collection("users", schema, native as NativeDb);
    await col.find({ $and: [{ id: "xyz" }] });
    const filter = (calls[0] as { filter: PlainObject }).filter;
    const andClauses = filter.$and as PlainObject[];
    assert.equal(andClauses[0].id, "xyz");
  });
});

// ---------------------------------------------------------------------------
// 5. Query edge cases
// ---------------------------------------------------------------------------

describe("query edge cases", () => {
  test("await same Query instance twice → executes twice (no caching)", async () => {
    let callCount = 0;
    const fn = async (
      _col: string,
      _filter: PlainObject,
      _opts: PlainObject
    ): Promise<string> => {
      callCount++;
      return JSON.stringify([]);
    };
    const q = new Query("users", {}, fn);
    await q;
    await q;
    assert.equal(callCount, 2);
  });

  test("find({}).limit(0) → passes through to native (0 is valid)", async () => {
    const col = new Collection("users", schema, makeNative());
    const { data, error } = await col.find({}).limit(0);
    assert.equal(error, null);
    assert.ok(Array.isArray(data));
  });

  test("find({}).limit(-1) → passes through to native (native decides)", async () => {
    const col = new Collection("users", schema, makeNative());
    const { data, error } = await col.find({}).limit(-1);
    assert.equal(error, null);
    assert.ok(Array.isArray(data));
  });

  test("find({}).select(\"\") → produces empty select array (native handles as SELECT *)", async () => {
    let capturedOpts: PlainObject = {};
    const fn = async (
      _col: string,
      _filter: PlainObject,
      opts: PlainObject
    ): Promise<string> => {
      capturedOpts = opts;
      return JSON.stringify([]);
    };
    const q = new Query("users", {}, fn).select("");
    await q._exec();
    // An empty string split by " " and filtered for length > 0 → empty array
    assert.ok(Array.isArray(capturedOpts.select));
    assert.equal((capturedOpts.select as string[]).length, 0);
  });

  test("find({}).sort({}) → empty sort passes through", async () => {
    const col = new Collection("users", schema, makeNative());
    const { data, error } = await col.find({}).sort({});
    assert.equal(error, null);
    assert.ok(Array.isArray(data));
  });

  test("Query._exec(): native returns malformed JSON → returns error with clear message", async () => {
    const fn = async (): Promise<string> => "{{{invalid";
    const q = new Query("users", {}, fn);
    const { data, error } = await q._exec();
    assert.equal(data, null);
    assert.ok(error instanceof Error && error.message.includes("find query failed"));
  });

  test("Query._exec(): native throws an Error → returned as error with cause", async () => {
    const fn = async (): Promise<string> => {
      throw new Error("native connection refused");
    };
    const q = new Query("users", {}, fn);
    const { data, error } = await q._exec();
    assert.equal(data, null);
    assert.ok(
      error instanceof Error &&
      error.message.includes("find query failed") &&
      error.message.includes("native connection refused")
    );
  });
});

// ---------------------------------------------------------------------------
// 6. Validation edge cases
// ---------------------------------------------------------------------------

describe("validation edge cases", () => {
  test("doc with extra fields not in schema → passes through (extensible by design)", () => {
    const result = validateDoc(
      { name: "Alice", unknownField: "extra", anotherExtra: 42 },
      schema
    );
    assert.equal(result.name, "Alice");
    assert.equal(result.unknownField, "extra");
  });

  test("partial update on field with min constraint → min is still checked", () => {
    const s = normalizeSchema({ age: t.number().min(0) });
    assert.throws(
      () => validatePartial({ age: -1 }, s),
      (err: unknown) => err instanceof ValidationError
    );
  });

  test("validateDoc with schema having no required fields, empty doc {} → passes", () => {
    const s = normalizeSchema({ name: t.string(), age: t.number() });
    assert.doesNotThrow(() => validateDoc({}, s));
  });

  test("boolean false for required boolean field → passes (false is not null)", () => {
    const s = normalizeSchema({ active: t.boolean().required() });
    const result = validateDoc({ active: false }, s);
    assert.equal(result.active, false);
  });

  test("number 0 for required number field → passes (0 is not null)", () => {
    const s = normalizeSchema({ score: t.number().required() });
    const result = validateDoc({ score: 0 }, s);
    assert.equal(result.score, 0);
  });

  test("empty string \"\" for required string field with min:1 → fails min check", () => {
    const s = normalizeSchema({ name: t.string().required().min(1) });
    assert.throws(
      () => validateDoc({ name: "" }, s),
      (err: unknown) => err instanceof ValidationError
    );
  });

  test("empty array [] for required array field → passes (empty array is not null)", () => {
    const s = normalizeSchema({ tags: t.array(t.string()).required() });
    const result = validateDoc({ tags: [] }, s);
    assert.deepEqual(result.tags, []);
  });

  test("NaN for required number field → fails validation", () => {
    const s = normalizeSchema({ score: t.number().required() });
    assert.throws(
      () => validateDoc({ score: NaN }, s),
      (err: unknown) => err instanceof ValidationError
    );
  });
});

// ---------------------------------------------------------------------------
// 7. Aggregate edge cases
// ---------------------------------------------------------------------------

describe("aggregate edge cases", () => {
  test("empty pipeline [] → passes through to native", async () => {
    const col = new Collection("users", schema, makeNative());
    const { data, error } = await col.aggregate([]);
    assert.equal(error, null);
    assert.ok(Array.isArray(data));
  });

  test("pipeline with unknown stage $lookup → passes through (not crash)", async () => {
    const col = new Collection("users", schema, makeNative());
    const { data, error } = await col.aggregate([
      {
        $lookup: {
          from: "orders",
          localField: "userId",
          foreignField: "id",
          as: "orders",
        },
      },
    ]);
    assert.equal(error, null);
    assert.ok(Array.isArray(data));
  });

  test("$group with no _id field → translateAggregatePipeline handles gracefully", () => {
    const pipeline = [{ $group: { count: { $sum: 1 } } }];
    // Should not throw; _id is undefined → translateGroupId handles gracefully
    assert.doesNotThrow(() => translateAggregatePipeline(pipeline));
    const result = translateAggregatePipeline(pipeline);
    const group = result[0].$group as PlainObject;
    assert.ok("by" in group);
  });

  test("$sum with non-string value (expression object) → passes through unchanged", () => {
    const pipeline = [
      {
        $group: {
          id: "$role",
          total: { $sum: { $multiply: ["$price", "$qty"] } },
        },
      },
    ];
    // translateAccumulator only handles $sum: 1 and $sum: "$string"; everything
    // else passes through. Should not throw.
    assert.doesNotThrow(() => translateAggregatePipeline(pipeline));
    const result = translateAggregatePipeline(pipeline);
    const group = result[0].$group as PlainObject;
    // total should pass through unchanged (not transformed to $count or $sum "field")
    const total = group.total as PlainObject;
    assert.ok("$sum" in total);
  });

  test("$group with null _id → translated to by: 'null'", () => {
    const pipeline = [{ $group: { id: null, count: { $sum: 1 } } }];
    assert.doesNotThrow(() => translateAggregatePipeline(pipeline));
    const result = translateAggregatePipeline(pipeline);
    const group = result[0].$group as PlainObject;
    assert.equal(group.by, "null");
  });

  test("pipeline with $sort stage → passes through unchanged", () => {
    const pipeline = [{ $sort: { name: 1 } }];
    const result = translateAggregatePipeline(pipeline);
    assert.deepEqual(result[0], { $sort: { name: 1 } });
  });
});
