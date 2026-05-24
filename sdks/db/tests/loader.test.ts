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
import { model } from "@zeroship/bootstrap/install-schema";
import { t } from "@zeroship/db";

type AnyRec = Record<string, unknown>;

/**
 * **P9 PR 1** — the SDK's `get()` (and every other "first matching row"
 * path) now routes through `find` with `{ limit: 1 }`. The mock log
 * splits `find` calls into two buckets:
 *   - `findBatched`: the `{id: {$in: [...]}}` shape coming from the
 *     IdLoader's batched flush.
 *   - `findSingle`: the `{ limit: 1 }` shape coming from a non-loader
 *     `Collection.get(...)` (filter object / select / orderBy / tx).
 *
 * Old test asserts on `calls.findOne.length` map to `calls.findSingle.length`.
 */
type CallLog = {
  /** Every `find` call, in order. */
  find: { filter: AnyRec; opts: AnyRec }[];
  /** Subset of `find` whose filter is `{id: {$in: [...]}}` — the IdLoader's
   *  batched flush. */
  findBatched: { filter: AnyRec; opts: AnyRec }[];
  /** Subset of `find` whose opts include `limit: 1` — the post-P9 "first
   *  matching row" shape (formerly the native `findOne` v8_method). */
  findSingle: { filter: AnyRec; opts: AnyRec }[];
};

/** A mock native that records every `find` call and returns rows from
 *  the provided row table keyed by id.
 *
 *  **P7 PR 3** — `Collection.get(N)` stringifies the numeric id on the
 *  way into the IdLoader (`String(idOrFilter)`); the mock accepts both
 *  `number` and `string` lookups by coercing via `String()` so pre-PR 3
 *  row tables keyed by `number` still match the new wire shape without
 *  rewriting every fixture. */
function makeMockNative(rows: Record<number, AnyRec>, opts?: { findThrows?: Error }) {
  const calls: CallLog = { find: [], findBatched: [], findSingle: [] };
  let beginCount = 0;
  // String-keyed view onto the same row table — the loader sends string
  // typed_id values on the wire post-PR 3, but the fixtures here key
  // by number for readability. Pre-build the string→row index once.
  const stringIndex: Record<string, AnyRec> = {};
  for (const [k, v] of Object.entries(rows)) stringIndex[k] = v;
  const native = {
    registerModel: () => Promise.resolve(),
    // P9 PR 3: native `transaction(callback)` orchestrator stub. The
    // begin tick happens when this is invoked (the bootstrap wrapper has
    // already drained the loaders + bumped `_txDepth`); the callback runs
    // with the tx active so id-reads dispatch directly (limit:1) rather
    // than batching.
    transaction: async (
      cb: (raw: unknown) => unknown,
      _o?: { isolationLevel?: string },
    ) => {
      beginCount += 1;
      return cb(undefined);
    },
    collection(_name: string) {
      return {
        async find(filter: AnyRec, o: AnyRec) {
          calls.find.push({ filter, opts: o });
          const idClause = filter.id;
          // `{id: {$in: [...]}}` — IdLoader batched path. Bucketed
          // BEFORE the throw so error-path tests can still assert on
          // the dispatch shape.
          const isBatched =
            idClause !== null &&
            typeof idClause === "object" &&
            Array.isArray((idClause as AnyRec).$in);
          if (isBatched) {
            calls.findBatched.push({ filter, opts: o });
          } else if (o && (o as AnyRec).limit === 1) {
            calls.findSingle.push({ filter, opts: o });
          }
          if (opts?.findThrows) throw opts.findThrows;
          if (isBatched) {
            const ids = (idClause as { $in: (string | number)[] }).$in;
            return ids.map((i) => stringIndex[String(i)]).filter(Boolean);
          }
          // Single-row resolve: match by id when present, otherwise
          // return the first row whose field map matches every filter
          // key. Good enough for tests that probe a small fixed row
          // table.
          if (typeof idClause === "number" || typeof idClause === "string") {
            const r = stringIndex[String(idClause)];
            return r ? [r] : [];
          }
          for (const r of Object.values(rows)) {
            let ok = true;
            for (const [k, v] of Object.entries(filter)) {
              if (r[k] !== v) { ok = false; break; }
            }
            if (ok) return [r];
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

    assert.equal(calls.findBatched.length, 1, "expected exactly one batched find");
    assert.equal(calls.findSingle.length, 0, "no single-row find expected");
    // **P7 PR 3** — the loader sends typed_id strings on the wire;
    // `Collection.get(1)` is `String(1) === "1"` going into `$in`.
    const idClause = calls.findBatched[0].filter.id as { $in: string[] };
    assert.ok(idClause && Array.isArray(idClause.$in));
    assert.deepEqual([...idClause.$in].sort(), ["1", "2"]);
  });

  test("filter object falls through to direct dispatch (find with limit:1)", async () => {
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
    assert.equal(calls.findBatched.length, 1, "one batched find for numeric ids");
    assert.equal(calls.findSingle.length, 1, "filter call uses find with limit:1");
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
    assert.equal(calls.findBatched.length, 0);
    assert.equal(calls.findSingle.length, 1, "select narrows → direct find with limit:1");
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
    assert.equal(calls.findBatched.length, 1);
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
    assert.equal(calls.findBatched.length, 1, "one batched call, all 3 rejected");
  });

  test("inside db.transaction(...) the loader is bypassed (each get uses find with limit:1)", async () => {
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
    assert.equal(calls.findSingle.length, 2, "tx-active reads dispatch directly with limit:1");
    assert.equal(calls.findBatched.length, 0);
  });

  test("pre-tx batched reads complete BEFORE the tx begin runs", async () => {
    // The race: a `get(id)` queued in the same turn as `db.transaction(...)`
    // is held by IdLoader for the next microtask. If the tx installs
    // TX_CONN before the batch flushes, the supposedly-non-tx batched read
    // leaks onto the transaction's connection. We assert the observable
    // ordering: every batched `find` settled before the begin tick. The
    // drain-before-begin lives in the bootstrap `transactionImpl` wrapper
    // (the JS-side DataLoader queues have no Rust counterpart, so the
    // drain stays in JS even though the begin moved into Rust).
    const events: string[] = [];
    const rowsByTable: Record<string, Record<number, AnyRec>> = {
      users: {
        1: { id: 1, email: "a@b.com", name: "Alice" },
        2: { id: 2, email: "b@b.com", name: "Bob" },
      },
    };
    const native = {
      registerModel: () => Promise.resolve(),
      // P9 PR 3: native `transaction(callback)` orchestrator. The "begin"
      // tick fires when the orchestrator is invoked — AFTER the bootstrap
      // wrapper drained the loaders. The callback resolving pushes
      // "commit"; throwing would push "rollback".
      async transaction(cb: (raw: unknown) => unknown, _o?: { isolationLevel?: string }) {
        events.push("begin");
        try {
          const out = await cb(undefined);
          events.push("commit");
          return out;
        } catch (e) {
          events.push("rollback");
          throw e;
        }
      },
      collection(name: string) {
        return {
          async find(filter: AnyRec, o: AnyRec) {
            events.push(`find(${name},${JSON.stringify(filter)})`);
            const idClause = filter.id as { $in?: number[] } | undefined;
            if (idClause && Array.isArray(idClause.$in)) {
              return idClause.$in.map((i) => rowsByTable[name]?.[i]).filter(Boolean);
            }
            // **P9 PR 1** — single-row `find(filter, {limit:1})`
            // replaces the old `findOne` path. Resolve to a 1-element
            // array (or empty) so the SDK's slice picks the row up.
            const id = filter.id;
            if (
              (typeof id === "number" || typeof id === "string") &&
              o && (o as AnyRec).limit === 1
            ) {
              const row = rowsByTable[name]?.[id as keyof typeof rowsByTable[string]];
              return row ? [row] : [];
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
    // bug: this queued dispatch fires AFTER the tx begin, and the find
    // lands on TX_CONN. The fix: the bootstrap wrapper awaits the loader
    // drain before invoking the native transaction(fn) (whose begin runs
    // in Rust).
    const usersCol = (db as unknown as Record<string, unknown>).users as {
      _idLoader: { load(id: string): Promise<unknown> } | null;
    };
    assert.ok(usersCol._idLoader !== null, "loader must be primed");
    // **P7 PR 3** — loader API is keyed by typed_id string. Pass "1"
    // so the underlying Map lookup matches the stringified row id.
    const preTxGet = usersCol._idLoader!.load("1");

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
    // **P7 PR 3** — wire shape is `$in:["1"]` (typed_id string), not
    // `$in:[1]`. The pre-tx loader.load(1) call below stays on the
    // number-keyed loader API; the outer Collection.get path stringifies
    // before reaching loader.load, so the events string match must
    // accept either shape during the migration window.
    const batchedFindIdx = events.findIndex(
      (e) =>
        e.startsWith("find(users,") &&
        (e.includes('"$in":[1]') || e.includes('"$in":["1"]')),
    );
    const beginIdx = events.indexOf("begin");
    assert.ok(batchedFindIdx >= 0, `expected a batched find, got events=${JSON.stringify(events)}`);
    assert.ok(beginIdx >= 0, `expected the tx begin tick, got events=${JSON.stringify(events)}`);
    assert.ok(
      batchedFindIdx < beginIdx,
      `expected batched find (idx=${batchedFindIdx}) BEFORE the tx begin (idx=${beginIdx}) — events=${JSON.stringify(events)}`,
    );
  });

  test("tx-race: get(id) queued pre-tx that flushes mid-tx is rejected", async () => {
    // Models the residual race left after `d218e54c`'s drain-before-begin:
    // a `get(id)` enqueues an entry, then `db.transaction(...)` runs.
    // `_txDepth` is bumped synchronously before the native transaction(fn)
    // is invoked, so by the time the loader's microtask fires,
    // `_txDepth > 0` and TX_CONN is live in Rust. The loader detects the
    // snapshot/current mismatch
    // and rejects with a clear error instead of routing the batched
    // find onto the tx connection.
    const { IdLoader } = await import("../src/loader.js");
    let currentDepth = 0;
    let flushCalls = 0;
    // **P7 PR 3** — IdLoader is generic over `R extends { id: string }`;
    // the test row uses a typed_id-shaped id string so the Map<string,_>
    // lookup matches.
    const loader = new IdLoader<{ id: string; v: string }>(
      async (ids) => {
        flushCalls += 1;
        const m = new Map<string, { id: string; v: string }>();
        for (const i of ids) m.set(i, { id: i, v: `row-${i}` });
        return m;
      },
      () => currentDepth,
    );
    // Enqueue at depth 0 (caller is outside any tx).
    const p = loader.load("usr_42", 0);
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
    // **P7 PR 3** — typed_id string key (see sibling test).
    const loader = new IdLoader<{ id: string; v: string }>(
      async (ids) => {
        const m = new Map<string, { id: string; v: string }>();
        for (const i of ids) m.set(i, { id: i, v: `row-${i}` });
        return m;
      },
      () => currentDepth,
    );
    // Both enqueue-time and flush-time depth are 0 — normal path.
    const r0 = await loader.load("row1", 0);
    assert.equal(r0?.v, "row-row1");
    // Entries enqueued inside a tx that flush inside the same tx are
    // honoured — the caller asked for tx routing and that's what they
    // get. (This branch is unusual in practice because Collection.get
    // bypasses the loader when _txDepth > 0, but the loader stays
    // correct under direct use.)
    currentDepth = 1;
    const r1 = await loader.load("row2", 1);
    assert.equal(r1?.v, "row-row2");
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
    // **P7 PR 3** — typed_id string wire shape.
    const ids = (calls.find[0].filter.id as { $in: string[] }).$in;
    assert.deepEqual(ids, ["7"], "duplicate ids removed before dispatch");
  });
});
