import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { model } from "../src/model.js";
import { Collection, NativeDb } from "../src/collection.js";
import { t } from "../src/types.js";

// ---------------------------------------------------------------------------
// Shared mock native
// ---------------------------------------------------------------------------

function makeMockNative(): NativeDb {
  return {
    insert: (_col, doc) =>
      Promise.resolve(JSON.stringify({ id: "mock-id", ...(doc as Record<string, unknown>) })),
    insertMany: (_col, docs) =>
      Promise.resolve(JSON.stringify((docs as unknown[]).map((d, i) => ({ id: `id${i}`, ...(d as Record<string, unknown>) })))),
    findOne: (_col, _filter) =>
      Promise.resolve(JSON.stringify({ id: "xyz", name: "Alice" })),
    find: (_col, _filter, _opts) =>
      Promise.resolve(JSON.stringify([{ id: "1", name: "Bob" }])),
    updateOne: (_col, _filter, _update) =>
      Promise.resolve(JSON.stringify({ id: "1" })),
    updateMany: (_col, _filter, _update) =>
      Promise.resolve(JSON.stringify({ updated: 2 })),
    deleteOne: (_col, _filter) =>
      Promise.resolve(JSON.stringify({ id: "1" })),
    deleteMany: (_col, _filter) =>
      Promise.resolve(JSON.stringify({ deleted: 1 })),
    count: (_col, _filter) =>
      Promise.resolve(JSON.stringify({ count: 3 })),
    distinct: (_col, _field, _filter) =>
      Promise.resolve(JSON.stringify(["a", "b"])),
    aggregate: (_col, _pipeline) =>
      Promise.resolve(JSON.stringify([{ id: "g1", total: 5 }])),
  };
}

// ---------------------------------------------------------------------------
// model() with builder schema
// ---------------------------------------------------------------------------

describe("model() — builder schema", () => {
  test("returns a Collection instance", () => {
    const native = makeMockNative();
    const Users = model("users", { name: t.string().required() }, native);
    assert.ok(Users instanceof Collection);
  });

  test("returned Collection has working create()", async () => {
    const native = makeMockNative();
    const Users = model("users", { name: t.string().required() }, native);
    const { data, error } = await Users.create({ name: "Alice" });
    assert.equal(error, null);
    assert.ok(data !== null && "id" in data);
    assert.equal(data.name, "Alice");
  });

  test("returned Collection has working findOne()", async () => {
    const native = makeMockNative();
    const Users = model("users", { name: t.string() }, native);
    const { data, error } = await Users.findOne({ name: "Alice" });
    assert.equal(error, null);
    assert.ok(data !== null);
    assert.equal(data.id, "xyz");
  });
});

// ---------------------------------------------------------------------------
// model() with Mongoose-style schema
// ---------------------------------------------------------------------------

describe("model() — Mongoose schema", () => {
  test("returns a Collection instance with Mongoose-style schema", () => {
    const native = makeMockNative();
    const schema = {
      name: { type: String, required: true },
      age: { type: Number, min: 0 },
    };
    const Users = model("users", schema, native);
    assert.ok(Users instanceof Collection);
  });

  test("Mongoose schema Collection has working create()", async () => {
    const native = makeMockNative();
    const schema = {
      name: { type: String, required: true },
    };
    const Users = model("users", schema, native);
    const { data, error } = await Users.create({ name: "Bob" });
    assert.equal(error, null);
    assert.ok(data !== null && "id" in data);
  });
});

// ---------------------------------------------------------------------------
// model() with nativeOverride
// ---------------------------------------------------------------------------

describe("model() — nativeOverride", () => {
  test("uses provided mock native instead of globalThis.zeroship.db", async () => {
    let insertCalled = false;
    const mockNative: NativeDb = {
      ...makeMockNative(),
      insert: (_col, doc) => {
        insertCalled = true;
        return Promise.resolve(JSON.stringify({ id: "override-id", ...(doc as Record<string, unknown>) }));
      },
    };
    const Users = model("users", { name: t.string().required() }, mockNative);
    await Users.create({ name: "Test" });
    assert.ok(insertCalled, "mock native insert should have been called");
  });

  test("nativeOverride result is used (not globalThis)", async () => {
    const mockNative = makeMockNative();
    const Users = model("articles", { title: t.string().required() }, mockNative);
    const { data, error } = await Users.create({ title: "Hello" });
    assert.equal(error, null);
    assert.ok(data !== null);
    assert.equal(data.id, "mock-id");
  });
});

// ---------------------------------------------------------------------------
// model() without native — throws clear error
// ---------------------------------------------------------------------------

describe("model() — missing native", () => {
  test("throws clear error when no nativeOverride and env.db is absent", () => {
    // In the Node test environment, the `zeroship` module is the file-linked
    // stub (sdks/zeroship-stub), which exports `env = {}` — i.e. no db
    // namespace. Calling `model()` without `nativeOverride` should hit the
    // getNativeDb() throw path. No global setup/teardown needed.
    assert.throws(
      () => model("users", { name: t.string() }),
      (err: unknown) => {
        assert.ok(err instanceof Error);
        assert.ok(
          err.message.includes("env.db not available"),
          `Expected error about env.db not available, got: ${err.message}`
        );
        return true;
      }
    );
  });
});

// ---------------------------------------------------------------------------
// model() input validation
// ---------------------------------------------------------------------------

describe("model() — input validation", () => {
  test("throws when name is empty string", () => {
    const native = makeMockNative();
    assert.throws(
      () => model("", { name: t.string() }, native),
      (err: unknown) => {
        assert.ok(err instanceof Error);
        assert.ok(err.message.includes("model name must be a non-empty string"));
        return true;
      }
    );
  });

  test("throws when name is whitespace-only string", () => {
    const native = makeMockNative();
    assert.throws(
      () => model("   ", { name: t.string() }, native),
      (err: unknown) => {
        assert.ok(err instanceof Error);
        assert.ok(err.message.includes("model name must be a non-empty string"));
        return true;
      }
    );
  });

  test("throws when name is not a string", () => {
    const native = makeMockNative();
    assert.throws(
      () => model(42 as unknown as string, { name: t.string() }, native),
      (err: unknown) => {
        assert.ok(err instanceof Error);
        assert.ok(err.message.includes("model name must be a non-empty string"));
        return true;
      }
    );
  });

  test("throws when schema is null", () => {
    const native = makeMockNative();
    assert.throws(
      () => model("users", null as unknown as Record<string, unknown>, native),
      (err: unknown) => {
        assert.ok(err instanceof Error);
        assert.ok(err.message.includes("model schema must be an object"));
        return true;
      }
    );
  });

  test("throws when schema is undefined", () => {
    const native = makeMockNative();
    assert.throws(
      () => model("users", undefined as unknown as Record<string, unknown>, native),
      (err: unknown) => {
        assert.ok(err instanceof Error);
        assert.ok(err.message.includes("model schema must be an object"));
        return true;
      }
    );
  });

  test("throws when schema is a non-object (string)", () => {
    const native = makeMockNative();
    assert.throws(
      () => model("users", "bad" as unknown as Record<string, unknown>, native),
      (err: unknown) => {
        assert.ok(err instanceof Error);
        assert.ok(err.message.includes("model schema must be an object"));
        return true;
      }
    );
  });
});
