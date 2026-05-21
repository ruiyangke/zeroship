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
import { installSchemaForTest } from "./_install-helper.js";
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
    const db = installSchemaForTest(
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

  test("pre-tx batched reads complete BEFORE beginTransaction runs", async () => {
    // The race: a `get(id)` queued in the same turn as `db.transaction(...)`
    // is held by IdLoader for the next microtask. If `beginTransaction`
    // installs TX_CONN before the batch flushes, the supposedly-non-tx
    // batched read leaks onto the transaction's connection. We assert the
    // observable ordering: every batched `find` settled before `beginCount`
    // tick. See db.ts drain-before-begin.
    const events: string[] = [];
    const rowsByTable: Record<string, Record<number, AnyRec>> = {
      users: {
        1: { id: 1, email: "a@b.com", name: "Alice" },
        2: { id: 2, email: "b@b.com", name: "Bob" },
      },
    };
    const native = {
      registerModel: () => Promise.resolve(),
      async beginTransaction(_o?: { isolationLevel?: string }) {
        events.push("beginTransaction");
        return {
          commit: async () => { events.push("commit"); },
          rollback: async () => { events.push("rollback"); },
        };
      },
      collection(name: string) {
        return {
          async findOne(filter: AnyRec, _o: AnyRec) {
            events.push(`findOne(${name},${JSON.stringify(filter)})`);
            const id = filter.id;
            if (typeof id === "number") return rowsByTable[name]?.[id] ?? null;
            return null;
          },
          async find(filter: AnyRec, _o: AnyRec) {
            events.push(`find(${name},${JSON.stringify(filter)})`);
            const idClause = filter.id as { $in?: number[] } | undefined;
            if (idClause && Array.isArray(idClause.$in)) {
              return idClause.$in.map((i) => rowsByTable[name]?.[i]).filter(Boolean);
            }
            return [];
          },
        };
      },
    };

    const db = installSchemaForTest(
      {
        users: {
          email: t.string().required().unique(),
          name: t.string().required(),
        },
      },
      { native: native as unknown as ZeroshipDb },
    );

    // Prime the IdLoader so it's allocated. Then queue a get and let
    // it reach `loader.load()` (so the loader queue contains an entry)
    // BUT keep the dispatch microtask from firing before we enter the
    // tx — by starting the tx in the same microtask drain cycle.
    await db.users.get(99); // primes _idLoader
    events.length = 0;

    // Reach into the loader and queue an entry directly: this models a
    // get() that has already passed ensureReady and called load(), so
    // it's in the loader queue at the moment tx starts. The original
    // bug: this queued dispatch fires AFTER beginTransaction, and the
    // find lands on TX_CONN. The fix: drain awaits the flush before
    // beginTransaction is called.
    const usersCol = (db as unknown as Record<string, unknown>).users as {
      _idLoader: { load(id: number): Promise<unknown> } | null;
    };
    assert.ok(usersCol._idLoader !== null, "loader must be primed");
    const preTxGet = usersCol._idLoader!.load(1);

    const txResult = db.transaction(async (tx) => {
      const got = await tx.users.get(2);
      return got;
    });

    const [pre, txr] = await Promise.all([preTxGet, txResult]);
    // preTxGet is the loader.load(...) return — the raw row, not a Result.
    assert.equal((pre as AnyRec | null)?.email, "a@b.com");
    assert.equal(txr.error, null);
    // TxCollection.get unwraps Result; body returned the row directly.
    assert.equal((txr.data as AnyRec | null)?.email, "b@b.com");

    // Critical: the batched find for the pre-tx get must precede
    // beginTransaction in the event log.
    const batchedFindIdx = events.findIndex((e) => e.startsWith("find(users,") && e.includes('"$in":[1]'));
    const beginIdx = events.indexOf("beginTransaction");
    assert.ok(batchedFindIdx >= 0, `expected a batched find, got events=${JSON.stringify(events)}`);
    assert.ok(beginIdx >= 0, `expected beginTransaction, got events=${JSON.stringify(events)}`);
    assert.ok(
      batchedFindIdx < beginIdx,
      `expected batched find (idx=${batchedFindIdx}) BEFORE beginTransaction (idx=${beginIdx}) — events=${JSON.stringify(events)}`,
    );
  });

  test("tx-race: get(id) queued pre-tx that flushes mid-tx is rejected", async () => {
    // Models the residual race left after `d218e54c`'s drain-before-begin:
    // a `get(id)` enqueues an entry, then `db.transaction(...)` runs.
    // `beginTransaction` resolves AFTER `_txDepth` is bumped, so by the
    // time the loader's microtask fires, `_txDepth > 0` and TX_CONN is
    // live in Rust. The loader detects the snapshot/current mismatch
    // and rejects with a clear error instead of routing the batched
    // find onto the tx connection.
    const { IdLoader } = await import("../src/loader.js");
    let currentDepth = 0;
    let flushCalls = 0;
    const loader = new IdLoader<{ id: number; v: string }>(
      async (ids) => {
        flushCalls += 1;
        const m = new Map<number, { id: number; v: string }>();
        for (const i of ids) m.set(i, { id: i, v: `row-${i}` });
        return m;
      },
      () => currentDepth,
    );
    // Enqueue at depth 0 (caller is outside any tx).
    const p = loader.load(42, 0);
    // Before the microtask fires, a tx opens on the owning collection.
    currentDepth = 1;
    let caught: unknown = null;
    try { await p; } catch (e) { caught = e; }
    assert.ok(caught instanceof Error, "expected the loader to reject");
    assert.match(
      (caught as Error).message,
      /transaction opened before flush/,
      `wrong error: ${(caught as Error).message}`,
    );
    assert.equal(flushCalls, 0, "flush must not run when every entry is rejected");
  });

  test("tx-race: snapshot==current (both 0 or both > 0) resolves normally", async () => {
    const { IdLoader } = await import("../src/loader.js");
    let currentDepth = 0;
    const loader = new IdLoader<{ id: number; v: string }>(
      async (ids) => {
        const m = new Map<number, { id: number; v: string }>();
        for (const i of ids) m.set(i, { id: i, v: `row-${i}` });
        return m;
      },
      () => currentDepth,
    );
    // Both enqueue-time and flush-time depth are 0 — normal path.
    const r0 = await loader.load(1, 0);
    assert.equal(r0?.v, "row-1");
    // Entries enqueued inside a tx that flush inside the same tx are
    // honoured — the caller asked for tx routing and that's what they
    // get. (This branch is unusual in practice because Collection.get
    // bypasses the loader when _txDepth > 0, but the loader stays
    // correct under direct use.)
    currentDepth = 1;
    const r1 = await loader.load(2, 1);
    assert.equal(r1?.v, "row-2");
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
