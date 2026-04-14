/**
 * Tests for createDb — transaction isolation levels and cursor pagination in TxQuery.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { createDb } from "../src/db.js";
import { t } from "../src/types.js";
import type { NativeDb } from "../src/collection.js";

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

type PlainObject = Record<string, unknown>;

interface CallRecord {
  method: string;
  args: unknown[];
}

function makeMockNative(overrides: Partial<Record<string, unknown>> = {}): {
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
      record("insert", [col, doc], JSON.stringify({ id: 1, ...(doc as PlainObject) })) as Promise<string>,
    insertMany: (col, docs) =>
      record("insertMany", [col, docs], JSON.stringify(
        (docs as PlainObject[]).map((d, i) => ({ id: i + 1, ...d }))
      )) as Promise<string>,
    findOne: (col, filter) =>
      record("findOne", [col, filter], JSON.stringify({ id: 1, name: "Alice", age: 30 })) as Promise<string | null>,
    find: (col, filter, opts) =>
      record("find", [col, filter, opts], JSON.stringify([{ id: 1, name: "Alice" }, { id: 2, name: "Bob" }])) as Promise<string>,
    updateOne: (col, filter, update) =>
      record("updateOne", [col, filter, update], JSON.stringify({ id: 1, name: "Alice" })) as Promise<string>,
    updateMany: (col, filter, update) =>
      record("updateMany", [col, filter, update], JSON.stringify({ updated: 2 })) as Promise<string>,
    deleteOne: (col, filter) =>
      record("deleteOne", [col, filter], JSON.stringify({ id: 1 })) as Promise<string>,
    deleteMany: (col, filter) =>
      record("deleteMany", [col, filter], JSON.stringify({ deleted: 3 })) as Promise<string>,
    count: (col, filter) =>
      record("count", [col, filter], JSON.stringify({ count: 5 })) as Promise<string>,
    distinct: (col, field, filter) =>
      record("distinct", [col, field, filter], JSON.stringify(["a", "b"])) as Promise<string>,
    aggregate: (col, pipeline) =>
      record("aggregate", [col, pipeline], JSON.stringify([{ total: 100 }])) as Promise<string>,
    upsert: (col, doc, conflictFields) =>
      record("upsert", [col, doc, conflictFields], JSON.stringify({ id: 1, ...(doc as PlainObject) })) as Promise<string>,
    registerModel: (col, schema) =>
      record("registerModel", [col, schema], undefined) as Promise<void>,
    beginTransaction: (isolationLevel?: string) =>
      record("beginTransaction", [isolationLevel], undefined) as Promise<void>,
    commitTransaction: () =>
      record("commitTransaction", [], undefined) as Promise<void>,
    rollbackTransaction: () =>
      record("rollbackTransaction", [], undefined) as Promise<void>,
    ...overrides,
  };

  return { native, calls };
}

// ---------------------------------------------------------------------------
// Transaction isolation levels
// ---------------------------------------------------------------------------

describe("Transaction isolation levels", () => {
  test("transaction without options calls beginTransaction with no args", async () => {
    const { native, calls } = makeMockNative();
    const db = createDb({ users: { name: t.string() } }, { native });

    await db.transaction(async (tx) => {
      const emp = await tx.users.create({ name: "Alice" });
      return emp;
    });

    const beginCall = calls.find((c) => c.method === "beginTransaction");
    assert.ok(beginCall, "beginTransaction should have been called");
    assert.equal(beginCall.args[0], undefined);
  });

  test("transaction passes isolationLevel to beginTransaction", async () => {
    const { native, calls } = makeMockNative();
    const db = createDb({ users: { name: t.string() } }, { native });

    await db.transaction(
      async (tx) => {
        const emp = await tx.users.create({ name: "Alice" });
        return emp;
      },
      { isolationLevel: "serializable" },
    );

    const beginCall = calls.find((c) => c.method === "beginTransaction");
    assert.ok(beginCall, "beginTransaction should have been called");
    assert.equal(beginCall.args[0], "serializable");
  });

  test("transaction passes repeatable read isolation level", async () => {
    const { native, calls } = makeMockNative();
    const db = createDb({ users: { name: t.string() } }, { native });

    await db.transaction(
      async (tx) => {
        await tx.users.findOne({ name: "Alice" });
        return null;
      },
      { isolationLevel: "repeatable read" },
    );

    const beginCall = calls.find((c) => c.method === "beginTransaction");
    assert.ok(beginCall);
    assert.equal(beginCall.args[0], "repeatable read");
  });

  test("transaction calls commit on success", async () => {
    const { native, calls } = makeMockNative();
    const db = createDb({ users: { name: t.string() } }, { native });

    const { data, error } = await db.transaction(
      async (tx) => {
        await tx.users.create({ name: "Alice" });
        return "done";
      },
      { isolationLevel: "read committed" },
    );

    assert.equal(error, null);
    assert.equal(data, "done");
    assert.ok(calls.some((c) => c.method === "commitTransaction"));
  });

  test("transaction calls rollback on error", async () => {
    const { native, calls } = makeMockNative();
    const db = createDb({ users: { name: t.string() } }, { native });

    const { data, error } = await db.transaction(
      async (_tx) => {
        throw new Error("intentional failure");
      },
      { isolationLevel: "serializable" },
    );

    assert.equal(data, null);
    assert.ok(error !== null);
    assert.ok(error.message.includes("intentional failure"));
    assert.ok(calls.some((c) => c.method === "rollbackTransaction"));
  });
});

// ---------------------------------------------------------------------------
// TxQuery cursor pagination (after)
// ---------------------------------------------------------------------------

describe("TxQuery cursor pagination (after)", () => {
  test("TxQuery.after() is chainable and works with await", async () => {
    const { native } = makeMockNative();
    const db = createDb({ users: { name: t.string() } }, { native });

    const { data, error } = await db.transaction(async (tx) => {
      const users = await tx.users.find().after(10).limit(5);
      return users;
    });

    assert.equal(error, null);
    assert.ok(data !== null);
  });

  test("TxQuery.after() passes correct filter to native", async () => {
    const { native, calls } = makeMockNative();
    const db = createDb({ users: { name: t.string() } }, { native });

    await db.transaction(async (tx) => {
      await tx.users.find().after(42).limit(10);
      return null;
    });

    const findCall = calls.find((c) => c.method === "find");
    assert.ok(findCall, "find should have been called");
    assert.deepEqual(findCall.args[1], { id: { $gt: 42 } });
  });

  test("TxQuery chains sort, limit, after together", async () => {
    const { native, calls } = makeMockNative();
    const db = createDb({ users: { name: t.string() } }, { native });

    await db.transaction(async (tx) => {
      await tx.users.find().sort({ id: 1 }).after(5).limit(20);
      return null;
    });

    const findCall = calls.find((c) => c.method === "find");
    assert.ok(findCall);
    assert.deepEqual(findCall.args[1], { id: { $gt: 5 } });
    const opts = findCall.args[2] as PlainObject;
    assert.deepEqual(opts.orderBy, { id: 1 });
    assert.equal(opts.limit, 20);
  });
});

// ---------------------------------------------------------------------------
// TxCollection: findOneAndUpdate, findOneAndDelete, upsert
// ---------------------------------------------------------------------------

describe("TxCollection findOneAndUpdate", () => {
  test("returns the updated document inside a transaction", async () => {
    const { native } = makeMockNative();
    const db = createDb({ users: { name: t.string() } }, { native });

    const { data, error } = await db.transaction(async (tx) => {
      const doc = await tx.users.findOneAndUpdate({ name: "Alice" }, { $set: { name: "Bob" } });
      return doc;
    });

    assert.equal(error, null);
    assert.ok(data !== null);
    assert.equal(data.id, 1);
  });

  test("returns null when no match inside a transaction", async () => {
    const { native } = makeMockNative({
      updateOne: () => Promise.resolve(null as unknown as string),
    });
    const db = createDb({ users: { name: t.string() } }, { native });

    const { data, error } = await db.transaction(async (tx) => {
      return tx.users.findOneAndUpdate({ name: "Ghost" }, { $set: { name: "X" } });
    });

    assert.equal(error, null);
    assert.equal(data, null);
  });

  test("throws (rolls back) on validation error", async () => {
    const { native } = makeMockNative();
    const db = createDb(
      { users: { name: t.string().required(), age: t.number() } },
      { native }
    );

    const { data, error } = await db.transaction(async (tx) => {
      return tx.users.findOneAndUpdate({ name: "Alice" }, { $set: { age: "bad" as unknown as number } });
    });

    assert.equal(data, null);
    assert.ok(error !== null);
  });
});

describe("TxCollection findOneAndDelete", () => {
  test("returns the deleted document inside a transaction", async () => {
    const { native } = makeMockNative();
    const db = createDb({ users: { name: t.string() } }, { native });

    const { data, error } = await db.transaction(async (tx) => {
      return tx.users.findOneAndDelete({ name: "Alice" });
    });

    assert.equal(error, null);
    assert.ok(data !== null);
    assert.equal(data.id, 1);
  });

  test("returns null when no document matches", async () => {
    const { native } = makeMockNative({
      deleteOne: () => Promise.resolve(null as unknown as string),
    });
    const db = createDb({ users: { name: t.string() } }, { native });

    const { data, error } = await db.transaction(async (tx) => {
      return tx.users.findOneAndDelete({ name: "Ghost" });
    });

    assert.equal(error, null);
    assert.equal(data, null);
  });
});

describe("TxCollection upsert", () => {
  test("returns the upserted document inside a transaction", async () => {
    const { native, calls } = makeMockNative();
    const db = createDb({ users: { name: t.string().required() } }, { native });

    const { data, error } = await db.transaction(async (tx) => {
      return tx.users.upsert({ name: "Alice" }, { conflictFields: ["name"] });
    });

    assert.equal(error, null);
    assert.ok(data !== null);
    assert.equal(data.id, 1);
    assert.equal(data.name, "Alice");
    assert.ok(calls.some((c) => c.method === "upsert"));
  });

  test("throws (rolls back) on validation error", async () => {
    const { native } = makeMockNative();
    const db = createDb(
      { users: { name: t.string().required(), age: t.number() } },
      { native }
    );

    const { data, error } = await db.transaction(async (tx) => {
      return tx.users.upsert({ age: 25 }, { conflictFields: ["name"] });
    });

    assert.equal(data, null);
    assert.ok(error !== null);
  });
});
