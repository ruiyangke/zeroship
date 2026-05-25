/**
 * Round-1 critique regression tests. One test per CRITICAL fix the
 * fixer-agent landed; the suite exists so a future refactor that
 * accidentally re-introduces any of these regressions fails loud
 * here rather than in a downstream user app.
 */
import { test, describe, beforeEach, afterEach } from "node:test";
import assert from "node:assert/strict";
import { env } from "zeroship";
import { installSchemaForTest } from "./_install-helper.js";
import { schema, t } from "@zeroship/db";
import { __zeroshipDbResetIndexWarnings } from "../src/collection.js";

/** Wire `env.db.openSubscription` to a mock so the `subscribe.ts`
 *  wrapper consumes our fake subs. */
function installEnv(native: unknown): void {
  (env as { db?: unknown }).db = native;
}

// ---------------------------------------------------------------------------
// CRITICAL #2 — live.ts: pending consumers must be a FIFO queue, not a
// single slot. Racing two `next()` calls used to drop the first promise.
// ---------------------------------------------------------------------------

describe("CRITICAL #2 — db.live: FIFO pendingConsumers", () => {
  type SubEvent =
    | { kind: "change"; op: "insert" | "update" | "delete"; collection: string; pk: number; columns: string[] }
    | { kind: "resync" }
    | { kind: "closed" };

  function makeFakeSub() {
    const queue: SubEvent[] = [];
    let pending: ((ev: SubEvent | null) => void) | null = null;
    let closed = false;
    return {
      async next(): Promise<SubEvent | null> {
        if (closed) return null;
        if (queue.length > 0) return queue.shift()!;
        return new Promise<SubEvent | null>((resolve) => { pending = resolve; });
      },
      close(): void {
        if (closed) return;
        closed = true;
        if (pending) { const p = pending; pending = null; p(null); }
      },
      emit(ev: SubEvent): void {
        if (closed) return;
        if (pending) { const p = pending; pending = null; p(ev); }
        else queue.push(ev);
      },
    };
  }

  test("two concurrent next() calls both resolve when an event arrives", async () => {
    const subs: ReturnType<typeof makeFakeSub>[] = [];
    const native = {
      registerModel: () => Promise.resolve(),
      collection: () => ({
        async find() { return [{ id: 1, title: "buy milk" }]; },
        // **P9 PR 1** — Collection-scoped openSubscription replaces the
        // deleted Db-level entry point.
        openSubscription: () => {
          const s = makeFakeSub();
          subs.push(s);
          return s;
        },
      }),
    } as unknown as ZeroshipDb;

    installEnv(native);
    const db = installSchemaForTest(
      { todos: { title: t.string().required() } },
      { native },
    );

    let runCount = 0;
    const live = db.live(() => {
      runCount += 1;
      return db.todos.find({});
    });

    // First next() drains the initial run.
    const first = await live.next();
    assert.equal(first.done, false);
    assert.equal((first.value as { title: string }[])[0].title, "buy milk");

    // Give the subscription wiring a tick to install.
    await new Promise((r) => setTimeout(r, 5));
    assert.ok(subs.length > 0, "expected at least one fake subscription");

    // Race two next() calls. Before the fix, the second `next()`
    // overwrote the first's resolver and the first promise leaked
    // forever.
    const a = live.next();
    const b = live.next();
    subs[0].emit({ kind: "change", op: "insert", collection: "todos", pk: 2, columns: [] });
    subs[0].emit({ kind: "change", op: "insert", collection: "todos", pk: 3, columns: [] });

    const timer = new Promise<readonly ["TIMEOUT"]>((resolve) =>
      setTimeout(() => resolve(["TIMEOUT"] as const), 1000),
    );
    const winner = await Promise.race([
      Promise.all([a, b]).then((v) => ["ok", v] as const),
      timer,
    ]);
    if (winner[0] === "TIMEOUT") {
      live.close();
      assert.fail("pending next() leaked — second call never resolved");
    }
    const [ra, rb] = winner[1];
    assert.equal(ra.done, false);
    assert.equal(rb.done, false);
    assert.ok(runCount >= 3, `expected initial + 2 reruns, got ${runCount}`);

    live.close();
  });

  test("close() wakes every pending next() with done:true", async () => {
    const native = {
      registerModel: () => Promise.resolve(),
      collection: () => ({
        async find() { return [] as unknown[]; },
        openSubscription: () => makeFakeSub(),
      }),
    } as unknown as ZeroshipDb;

    installEnv(native);
    const db = installSchemaForTest(
      { todos: { title: t.string().required() } },
      { native },
    );

    const live = db.live(() => db.todos.find({}));
    // Drain the initial result.
    await live.next();
    // Wait for subscription wiring.
    await new Promise((r) => setTimeout(r, 5));

    const a = live.next();
    const b = live.next();
    live.close();
    const [ra, rb] = await Promise.all([a, b]);
    assert.equal(ra.done, true);
    assert.equal(rb.done, true);
  });

  test("rerun error rejects EVERY pending next() (FIFO error-path drain)", async () => {
    // Round-2 regression — the original FIFO fix only rejected the
    // head consumer on an error event; subsequent pending consumers
    // would leak forever. Two racing iter.next() calls must BOTH
    // observe the failure when the producer pumps an error.
    let runCount = 0;
    const subs: ReturnType<typeof makeFakeSub>[] = [];
    const native = {
      registerModel: () => Promise.resolve(),
      collection: () => ({
        async find() {
          runCount += 1;
          // First run (initial result) succeeds; every rerun throws so
          // pump({kind:"error", error}) fires while two consumers are
          // pending.
          if (runCount === 1) return [{ id: 1, title: "ok" }];
          throw new Error("rerun failed: synthetic");
        },
        openSubscription: () => {
          const s = makeFakeSub();
          subs.push(s);
          return s;
        },
      }),
    } as unknown as ZeroshipDb;

    installEnv(native);
    const db = installSchemaForTest(
      { todos: { title: t.string().required() } },
      { native },
    );

    const live = db.live(() => db.todos.find({}));
    // Drain the initial result.
    const first = await live.next();
    assert.equal(first.done, false);

    // Give the subscription wiring a tick to install.
    await new Promise((r) => setTimeout(r, 5));
    assert.ok(subs.length > 0, "expected at least one fake subscription");

    // Race two pending consumers; emit a change so rerun() fires and
    // throws — pump({kind:"error", ...}) must reject BOTH a and b.
    const a = live.next();
    const b = live.next();
    subs[0].emit({ kind: "change", op: "insert", collection: "todos", pk: 2, columns: [] });

    const timer = new Promise<readonly ["TIMEOUT"]>((resolve) =>
      setTimeout(() => resolve(["TIMEOUT"] as const), 1000),
    );
    const winner = await Promise.race([
      Promise.allSettled([a, b]).then((v) => ["ok", v] as const),
      timer,
    ]);
    if (winner[0] === "TIMEOUT") {
      live.close();
      assert.fail("pending next() leaked — second consumer never observed the error");
    }
    const [ra, rb] = winner[1];
    assert.equal(ra.status, "rejected", "first consumer must observe the error");
    assert.equal(rb.status, "rejected", "second consumer must observe the error");
    if (ra.status === "rejected" && rb.status === "rejected") {
      assert.match((ra.reason as Error).message, /rerun failed: synthetic/);
      assert.match((rb.reason as Error).message, /rerun failed: synthetic/);
    }

    live.close();
  });
});

// ---------------------------------------------------------------------------
// CRITICAL #3 — _filterCoveredByIndex: multi-key filters require either
// a covering compound index OR every key to be marked. The prior rule
// silently hid scans like `find({userId, done})` when only `done` had
// a single-field marker.
// ---------------------------------------------------------------------------

describe("CRITICAL #3 — unindexed-query warning is strict for multi-key filters", () => {
  let warnings: string[] = [];
  let origWarn: typeof console.warn;

  beforeEach(() => {
    warnings = [];
    origWarn = console.warn;
    console.warn = (msg: string) => { warnings.push(String(msg)); };
    (globalThis as { __zeroshipDbWarnIndexInTest?: boolean }).__zeroshipDbWarnIndexInTest = true;
    __zeroshipDbResetIndexWarnings();
  });

  afterEach(() => {
    console.warn = origWarn;
    (globalThis as { __zeroshipDbWarnIndexInTest?: boolean }).__zeroshipDbWarnIndexInTest = false;
    __zeroshipDbResetIndexWarnings();
  });

  function makeDb() {
    const native = {
      registerModel: () => Promise.resolve(),
      collection: () => ({
        async find() { return []; },
        async findOne() { return null; },
      }),
    } as unknown as ZeroshipDb;

    return installSchemaForTest(
      {
        // `done` has a single-field `.index()` marker; `userId` does NOT.
        // Compound filter `{ userId, done }` should WARN because no
        // compound index covers it, even though `done` is marked.
        todos: schema({
          userId: t.number().required(),
          done: t.boolean().default(false).index(),
          title: t.string().required(),
        }),
      },
      { native },
    );
  }

  test("compound filter with only ONE key marked DOES warn", async () => {
    const db = makeDb();
    await db.todos.find({ userId: 7, done: true });
    assert.equal(
      warnings.length,
      1,
      `expected one warning for unindexed compound filter, got: ${warnings.join(" | ")}`,
    );
    assert.match(warnings[0], /unindexed query on "todos"/);
  });

  test("single-key filter on marked column still suppresses the warning", async () => {
    const db = makeDb();
    await db.todos.find({ done: true });
    assert.equal(warnings.length, 0, `unexpected warnings: ${warnings.join(" | ")}`);
  });

  test("compound filter where every key is marked suppresses the warning", async () => {
    const native = {
      registerModel: () => Promise.resolve(),
      collection: () => ({
        async find() { return []; },
        async findOne() { return null; },
      }),
    } as unknown as ZeroshipDb;
    const db = installSchemaForTest(
      {
        users: schema({
          // Both columns marked → compound filter is covered.
          email: t.string().required().unique(),
          tenantId: t.number().required().index(),
        }),
      },
      { native },
    );
    await db.users.find({ email: "a@b.com", tenantId: 1 });
    assert.equal(warnings.length, 0, `unexpected warnings: ${warnings.join(" | ")}`);
  });
});

// ---------------------------------------------------------------------------
// CRITICAL #4 — drain-before-begin: `_txDepth` must be bumped BEFORE
// `await beginTransaction(...)` resolves. Otherwise a `get(id)` queued
// in the window between drain and BEGIN sees txDepthAtCall === 0 and
// routes through the loader onto the (now-tx-bound) connection.
// ---------------------------------------------------------------------------

describe("CRITICAL #4 — _txDepth bumped synchronously before begin resolves", () => {
  test("get(id) issued during a slow tx begin sees _txDepth > 0", async () => {
    let beginTriggered: () => void = () => { /* set below */ };
    const beginGate = new Promise<void>((resolve) => { beginTriggered = resolve; });
    const events: string[] = [];

    const native = {
      registerModel: () => Promise.resolve(),
      // P9 PR 3: native `transaction(callback)` orchestrator. The
      // bootstrap wrapper bumps `_txDepth` synchronously before invoking
      // this; we then stall (await the gate) BEFORE running the callback,
      // modelling a slow BEGIN. A `get(id)` issued during the stall sees
      // `_txDepth > 0` and bypasses the loader.
      async transaction(cb: (raw: unknown) => unknown) {
        events.push("begin:enter");
        await beginGate;
        events.push("begin:resolve");
        return cb(undefined);
      },
      collection: () => ({
        async find(filter: Record<string, unknown>, opts: Record<string, unknown>) {
          // **P9 PR 1** — split batched (`$in`) vs single (`limit:1`)
          // shapes so the same fixture covers both paths the SDK now
          // exercises.
          const idClause = filter.id as { $in?: number[] } | undefined;
          if (idClause && Array.isArray(idClause.$in)) {
            events.push(`findBatched:${JSON.stringify(filter)}`);
            return idClause.$in.map((id) => ({ id, v: `row-${id}` }));
          }
          if (opts && (opts as { limit?: number }).limit === 1) {
            events.push(`findSingle:${JSON.stringify(filter)}`);
            if (typeof filter.id === "number" || typeof filter.id === "string") {
              return [{ id: filter.id, v: `row-${String(filter.id)}` }];
            }
            return [];
          }
          events.push(`find:${JSON.stringify(filter)}`);
          return [];
        },
      }),
    } as unknown as ZeroshipDb;

    const db = installSchemaForTest(
      { users: { name: t.string().required() } },
      { native },
    );

    // Open a tx that pauses inside the native transaction(fn) before the
    // callback runs. The `_txDepth` bump happens (in the bootstrap
    // wrapper) before that, so any get(id) issued before the begin
    // resolves should see _txDepth > 0 and bypass the loader.
    const txPromise = db.transaction(async () => {
      // Body runs after begin resolves.
      return { ok: true };
    });

    // Wait for begin to be entered (we know `_txDepth` was bumped after
    // the drain settled, one microtask earlier).
    await new Promise((r) => setTimeout(r, 5));

    // Issue a get(id) WHILE begin is still pending. The pre-fix
    // behaviour would route this through the loader (because
    // `_txDepth` wasn't bumped yet). The post-fix behaviour bumps
    // synchronously, so `_txDepth > 0` at the call boundary forces
    // the single-row find path (formerly the findOne fallback).
    const getDuringBegin = db.users.get("usr_42");

    // Release begin.
    beginTriggered();
    const [getRes, txRes] = await Promise.all([getDuringBegin, txPromise]);

    assert.equal(txRes.error, null);
    assert.equal(getRes.error, null);
    // With the fix, this get goes through find with limit:1 (bypasses
    // loader) because _txDepth was already > 0 at the call boundary.
    const batchedCalls = events.filter((e) => e.startsWith("findBatched:"));
    const singleCalls = events.filter((e) => e.startsWith("findSingle:"));
    assert.equal(
      batchedCalls.length, 0,
      `expected no loader-batched find, got events=${JSON.stringify(events)}`,
    );
    assert.equal(
      singleCalls.length, 1,
      `expected one single-row find (loader bypass), got events=${JSON.stringify(events)}`,
    );
  });

  test("native transaction begin failure rolls back the _txDepth bump", async () => {
    const native = {
      registerModel: () => Promise.resolve(),
      // P9 PR 3: native orchestrator rejects (begin failed) — the callback
      // never runs. The bootstrap wrapper's `finally` must still decrement
      // `_txDepth`.
      async transaction(_cb: (raw: unknown) => unknown) {
        throw Object.assign(new Error("db.transaction: BEGIN failed: boom"), {
          code: "BEGIN_FAILED",
        });
      },
      collection: () => ({
        async find() { return []; },
        async findOne() { return null; },
      }),
    } as unknown as ZeroshipDb;

    const db = installSchemaForTest(
      { users: { name: t.string().required() } },
      { native },
    );

    const res = await db.transaction(async () => ({}));
    assert.ok(res.error, "expected the transaction to fail");
    assert.match(res.error!.message, /BEGIN failed/);
    assert.equal(
      (res.error as { code?: string }).code,
      "BEGIN_FAILED",
      "begin failure must carry code=begin_failed",
    );

    // The decrement must have happened — a subsequent get(id) sees
    // _txDepth === 0 and routes through the loader normally.
    const usersCol = (db as unknown as Record<string, unknown>).users as { _txDepth: number };
    assert.equal(usersCol._txDepth, 0, "expected _txDepth to be 0 after begin failure");
  });
});

// ---------------------------------------------------------------------------
// IMPORTANT #12 — loader.ts tx-race rejection must stamp a `code`
// for callers branching on error.code.
// ---------------------------------------------------------------------------

describe("IMPORTANT #12 — loader tx-race rejection carries error.code", () => {
  test("rejection from snapshot-vs-current mismatch has code === loader_tx_race", async () => {
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

    const p = loader.load(1, 0);
    currentDepth = 1;
    try {
      await p;
      assert.fail("expected the loader to reject");
    } catch (e) {
      const err = e as Error & { code?: string };
      assert.equal(err.code, "LOADER_TX_RACE");
      assert.match(err.message, /transaction opened before flush/);
    }
  });
});

// ---------------------------------------------------------------------------
// CRITICAL #1 — Db<T> type erosion: not directly observable at runtime,
// but a runtime sanity check on `db.users.Id` access shape and a smoke
// `find().with(...)` call confirms the wiring still resolves.
// ---------------------------------------------------------------------------

describe("CRITICAL #1 — typed collections after installSchema", () => {
  test("collection wrappers carry their name and resolve relations", async () => {
    type AnyRec = Record<string, unknown>;
    const callLog: { table: string; filter: AnyRec }[] = [];
    const rowsByTable: Record<string, AnyRec[]> = {
      users: [
        { id: "usr_1", email: "alice@example.com", name: "Alice" },
      ],
      todos: [
        { id: "todo_10", user_id: "usr_1", title: "buy milk" },
      ],
    };
    const native = {
      registerModel: () => Promise.resolve(),
      collection: (n: string) => ({
        async find(filter: AnyRec) {
          callLog.push({ table: n, filter });
          const idClause = filter.id as { $in?: string[] } | undefined;
          if (idClause && Array.isArray(idClause.$in)) {
            const wanted = new Set(idClause.$in);
            return rowsByTable[n].filter((r) => typeof r.id === "string" && wanted.has(r.id));
          }
          return rowsByTable[n] ?? [];
        },
        async findOne(filter: AnyRec) {
          callLog.push({ table: n, filter });
          if (typeof filter.id === "number" || typeof filter.id === "string") {
            return (
              rowsByTable[n]?.find((r) => String(r.id) === String(filter.id)) ?? null
            );
          }
          return rowsByTable[n]?.[0] ?? null;
        },
      }),
    } as unknown as ZeroshipDb;

    const db = installSchemaForTest(
      {
        users: { email: t.string().required(), name: t.string().required() },
        todos: { userId: t.ref("users").required(), title: t.string().required() },
      },
      { native },
    );

    // Compile-time-only: the brand exists on the type but is never
    // assigned. Accessing it at runtime is `undefined`; the assertion
    // here is the wrapper itself has the property declared (the type
    // wiring is what we actually care about — preserved by the
    // CRITICAL #1 fix).
    assert.equal(typeof db.users, "object");
    assert.equal(typeof db.todos, "object");

    const result = await db.todos.find({}, { with: { userId: true } });
    assert.equal(result.error, null);
    assert.ok(result.data);
    assert.equal(result.data!.length, 1);
    const joined = result.data![0] as { userId: { email: string } | null };
    assert.ok(joined.userId, "expected the relation to be loaded");
    assert.equal(joined.userId!.email, "alice@example.com");
  });
});
