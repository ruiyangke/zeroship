/**
 * DataLoader / per-collection batch tests.
 *
 * Verifies that `Collection.get(id)` calls within one microtask coalesce
 * into a single underlying `find({id: {$in: [...]}})`, and that the
 * fallback paths (filter shape, select, orderBy, active tx) bypass the
 * loader and dispatch directly.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { createDb } from "../src/db.js";
import { model } from "../src/model.js";
import { t } from "../src/types.js";

type AnyRec = Record<string, unknown>;

type CallLog = {
  find: { filter: AnyRec; opts: AnyRec }[];
  findOne: { filter: AnyRec; opts: AnyRec }[];
};

/** A mock native that records every `find` / `findOne` call and returns
 *  rows from the provided row table keyed by id. */
function makeMockNative(rows: Record<number, AnyRec>, opts?: { findThrows?: Error }) {
  const calls: CallLog = { find: [], findOne: [] };
  let beginCount = 0;
  const native = {
    registerModel: () => Promise.resolve(),
    beginTransaction: async (_o?: { isolationLevel?: string }) => {
      beginCount += 1;
      return {
        commit: async () => undefined,
        rollback: async () => undefined,
      };
    },
    collection(_name: string) {
      return {
        async findOne(filter: AnyRec, o: AnyRec) {
          calls.findOne.push({ filter, opts: o });
          // Match by id when present, otherwise return the first row
          // whose field map matches every filter key. Good enough for
          // tests that probe a small fixed row table.
          const id = filter.id;
          if (typeof id === "number") return rows[id] ?? null;
          for (const r of Object.values(rows)) {
            let ok = true;
            for (const [k, v] of Object.entries(filter)) {
              if (r[k] !== v) { ok = false; break; }
            }
            if (ok) return r;
          }
          return null;
        },
        async find(filter: AnyRec, o: AnyRec) {
          calls.find.push({ filter, opts: o });
          if (opts?.findThrows) throw opts.findThrows;
          // `{id: {$in: [...]}}` from the loader path.
          const idClause = filter.id;
          if (
            idClause !== null &&
            typeof idClause === "object" &&
            Array.isArray((idClause as AnyRec).$in)
          ) {
            const ids = (idClause as { $in: number[] }).$in;
            return ids.map((i) => rows[i]).filter(Boolean);
          }
          // Fallback — flat id equality (unusual in this suite).
          if (typeof idClause === "number") {
            const r = rows[idClause];
            return r ? [r] : [];
          }
          return [];
        },
      };
    },
    get _beginCount() {
      return beginCount;
    },
  };
  return { native: native as unknown as ZeroshipDb, calls };
}

describe("IdLoader — DataLoader batching for get(id)", () => {
  test("two concurrent get(id) calls collapse into one find with $in", async () => {
    const { native, calls } = makeMockNative({
      1: { id: 1, email: "a@b.com", name: "Alice" },
      2: { id: 2, email: "b@b.com", name: "Bob" },
    });
    const Users = model(
      "users",
      {
        email: t.string().required().unique(),
        name: t.string().required(),
      },
      native,
    );

    const [a, b] = await Promise.all([Users.get(1), Users.get(2)]);
    assert.equal(a.error, null);
    assert.equal(b.error, null);
    assert.equal(a.data?.email, "a@b.com");
    assert.equal(b.data?.email, "b@b.com");

    assert.equal(calls.find.length, 1, "expected exactly one underlying find");
    assert.equal(calls.findOne.length, 0, "findOne should not be called");
    const idClause = calls.find[0].filter.id as { $in: number[] };
    assert.ok(idClause && Array.isArray(idClause.$in));
    assert.deepEqual([...idClause.$in].sort((x, y) => x - y), [1, 2]);
  });

  test("filter object falls through to direct dispatch (findOne)", async () => {
    const { native, calls } = makeMockNative({
      1: { id: 1, email: "a@b.com", name: "Alice" },
    });
    const Users = model(
      "users",
      {
        email: t.string().required().unique(),
        name: t.string().required(),
      },
      native,
    );

    const [byId1, byId2, byFilter] = await Promise.all([
      Users.get(1),
      Users.get(1),
      Users.get({ email: "a@b.com" }),
    ]);
    assert.equal(byId1.data?.email, "a@b.com");
    assert.equal(byId2.data?.email, "a@b.com");
    assert.equal(byFilter.data?.email, "a@b.com");

    // The two numeric gets coalesce; the filter call dispatches directly.
    assert.equal(calls.find.length, 1, "one batched find for numeric ids");
    assert.equal(calls.findOne.length, 1, "filter call uses findOne directly");
  });

  test("get(id, {select: [...]}) bypasses the loader", async () => {
    const { native, calls } = makeMockNative({
      1: { id: 1, email: "a@b.com", name: "Alice" },
    });
    const Users = model(
      "users",
      {
        email: t.string().required().unique(),
        name: t.string().required(),
      },
      native,
    );

    const { data } = await Users.get(1, { select: ["email"] });
    assert.ok(data);
    assert.equal(calls.find.length, 0);
    assert.equal(calls.findOne.length, 1, "select narrows → direct findOne");
  });

  test("get(id) for missing row resolves to null", async () => {
    const { native, calls } = makeMockNative({
      1: { id: 1, email: "a@b.com", name: "Alice" },
    });
    const Users = model(
      "users",
      {
        email: t.string().required().unique(),
        name: t.string().required(),
      },
      native,
    );

    const [hit, miss] = await Promise.all([Users.get(1), Users.get(99)]);
    assert.equal(hit.data?.email, "a@b.com");
    assert.equal(miss.error, null);
    assert.equal(miss.data, null);
    assert.equal(calls.find.length, 1);
  });

  test("an error in the batched find rejects every queued caller", async () => {
    const boom = new Error("batched fetch failed");
    const { native, calls } = makeMockNative({}, { findThrows: boom });
    const Users = model(
      "users",
      {
        email: t.string().required().unique(),
        name: t.string().required(),
      },
      native,
    );

    const [a, b, c] = await Promise.all([
      Users.get(1),
      Users.get(2),
      Users.get(3),
    ]);
    assert.equal(a.data, null);
    assert.equal(b.data, null);
    assert.equal(c.data, null);
    assert.ok(a.error);
    assert.ok(b.error);
    assert.ok(c.error);
    assert.equal(a.error!.message, "batched fetch failed");
    assert.equal(b.error!.message, "batched fetch failed");
    assert.equal(c.error!.message, "batched fetch failed");
    assert.equal(calls.find.length, 1, "one batched call, all 3 rejected");
  });

  test("inside db.transaction(...) the loader is bypassed (each get uses findOne)", async () => {
    const { native, calls } = makeMockNative({
      1: { id: 1, email: "a@b.com", name: "Alice" },
      2: { id: 2, email: "b@b.com", name: "Bob" },
    });
    const db = createDb(
      {
        users: {
          email: t.string().required().unique(),
          name: t.string().required(),
        },
      },
      { native },
    );

    const { data, error } = await db.transaction(async (tx) => {
      const [u1, u2] = await Promise.all([tx.users.get(1), tx.users.get(2)]);
      return { u1, u2 };
    });
    assert.equal(error, null);
    assert.ok(data);
    assert.equal(data!.u1?.email, "a@b.com");
    assert.equal(data!.u2?.email, "b@b.com");
    assert.equal(calls.findOne.length, 2, "tx-active reads dispatch directly");
    assert.equal(calls.find.length, 0);
  });

  test("repeated id in one microtask is deduped before the wire call", async () => {
    const { native, calls } = makeMockNative({
      7: { id: 7, email: "x@y.com", name: "Same" },
    });
    const Users = model(
      "users",
      {
        email: t.string().required().unique(),
        name: t.string().required(),
      },
      native,
    );

    const results = await Promise.all([Users.get(7), Users.get(7), Users.get(7)]);
    for (const r of results) assert.equal(r.data?.email, "x@y.com");
    assert.equal(calls.find.length, 1);
    const ids = (calls.find[0].filter.id as { $in: number[] }).$in;
    assert.deepEqual(ids, [7], "duplicate ids removed before dispatch");
  });
});
