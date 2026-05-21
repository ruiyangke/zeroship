/**
 * `db.live(queryFn)` — reactive query layer over the coarse per-table
 * subscription broker.
 *
 * Coverage:
 *   1. Initial result yielded synchronously after the first `next()`.
 *   2. An event on a watched table fires a rerun; the new array yields.
 *   3. An event on an UNRELATED table does NOT trigger a rerun.
 *   4. `live.close()` terminates the iterator with `{done: true}`.
 *   5. `for await ... break` calls `close()` automatically.
 *   6. Multiple parallel `db.live` calls don't interfere with each
 *      other (stack-discipline tracker).
 *   7. Calling `db.live` inside `db.transaction(...)` rejects with
 *      `code = "live_in_transaction"`.
 *   8. Explicit `{tables: [...]}` bypasses auto-detection.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { env } from "zeroship";
import { _installSchema } from "../src/db.js";
import { t } from "../src/types.js";

type AnyRec = Record<string, unknown>;
type SubEvent =
  | { kind: "change"; op: "insert" | "update" | "delete"; collection: string; pk: number; columns: string[] }
  | { kind: "resync" }
  | { kind: "closed" };

interface FakeSub {
  next(): Promise<SubEvent | null>;
  close(): void;
  emit(ev: SubEvent): void;
}

/** Build a queue-backed fake Subscription whose `next()` resolves to
 *  the next event pushed via `emit()`, blocking until one arrives. */
function makeFakeSub(): FakeSub & { closes: number } {
  const queue: SubEvent[] = [];
  let pending: ((ev: SubEvent | null) => void) | null = null;
  let closed = false;
  const state = { closes: 0 };
  const sub = {
    async next(): Promise<SubEvent | null> {
      if (closed) return null;
      if (queue.length > 0) return queue.shift()!;
      return new Promise<SubEvent | null>((resolve) => { pending = resolve; });
    },
    close(): void {
      if (closed) return;
      closed = true;
      state.closes += 1;
      if (pending) {
        const p = pending;
        pending = null;
        p(null);
      }
    },
    emit(ev: SubEvent): void {
      if (closed) return;
      if (pending) {
        const p = pending;
        pending = null;
        p(ev);
      } else {
        queue.push(ev);
      }
    },
  };
  Object.defineProperty(sub, "closes", { get: () => state.closes });
  return sub as FakeSub & { closes: number };
}

/** Mock native + a row table mutated by tests directly. The `find`
 *  callback returns a deep-snapshot of the current rows so a rerun
 *  picks up the latest mutation. */
function makeMockNative() {
  const rowsByTable: Record<string, AnyRec[]> = {};
  const subs: Record<string, FakeSub[]> = {};
  const calls = { find: 0, openSubscription: 0 };

  const native = {
    registerModel: async () => undefined,
    beginTransaction: async () => ({
      commit: async () => undefined,
      rollback: async () => undefined,
    }),
    collection(name: string) {
      return {
        async findOne(_f: AnyRec, _o: AnyRec) { return null; },
        async find(_filter: AnyRec, _opts: AnyRec) {
          calls.find += 1;
          return [...(rowsByTable[name] ?? [])];
        },
        async insert(row: AnyRec) {
          (rowsByTable[name] ??= []).push(row);
          return row;
        },
      };
    },
    openSubscription(name: string): FakeSub {
      calls.openSubscription += 1;
      const sub = makeFakeSub();
      (subs[name] ??= []).push(sub);
      return sub;
    },
  };
  return {
    native: native as unknown as ZeroshipDb,
    rowsByTable,
    subs,
    calls,
    /** Push an event to every subscriber of `table`. */
    fire(table: string, ev?: Partial<SubEvent>): void {
      const list = subs[table] ?? [];
      const event: SubEvent =
        ev && ev.kind ? (ev as SubEvent) : { kind: "change", op: "insert", collection: table, pk: 0, columns: [] };
      for (const s of list) s.emit(event);
    },
  };
}

/** Wire `env.db.openSubscription` to a mock so the `subscribe.ts`
 *  wrapper consumes our fake subs. */
function installEnv(native: { openSubscription: (n: string) => FakeSub }): void {
  (env as { db?: unknown }).db = native;
}

describe("db.live — reactive query layer", () => {
  test("initial query result is yielded on first next()", async () => {
    const { native } = makeMockNative();
    installEnv(native as unknown as { openSubscription: (n: string) => FakeSub });
    const db = _installSchema(
      { todos: { title: t.string().required() } },
      { native },
    );
    await db.todos.insert({ title: "first" });

    const live = db.live(() => db.todos.find({}));
    const first = await live.next();
    assert.equal(first.done, false);
    assert.equal(Array.isArray(first.value), true);
    assert.equal(first.value!.length, 1);
    assert.equal((first.value![0] as AnyRec).title, "first");
    live.close();
  });

  test("an event on a watched table triggers a rerun", async () => {
    const ctx = makeMockNative();
    installEnv(ctx.native as unknown as { openSubscription: (n: string) => FakeSub });
    const db = _installSchema(
      { todos: { title: t.string().required() } },
      { native: ctx.native },
    );
    await db.todos.insert({ title: "a" });

    const live = db.live(() => db.todos.find({}));
    const first = await live.next();
    assert.equal((first.value as AnyRec[]).length, 1);

    // Insert + fire event — the rerun should pick up both rows.
    await db.todos.insert({ title: "b" });
    ctx.fire("todos");

    const second = await live.next();
    assert.equal(second.done, false);
    assert.equal((second.value as AnyRec[]).length, 2);
    live.close();
  });

  test("an event on an unrelated table does NOT trigger a rerun", async () => {
    const ctx = makeMockNative();
    installEnv(ctx.native as unknown as { openSubscription: (n: string) => FakeSub });
    const db = _installSchema(
      {
        todos: { title: t.string().required() },
        users: { name: t.string().required() },
      },
      { native: ctx.native },
    );
    await db.todos.insert({ title: "x" });

    const live = db.live(() => db.todos.find({}));
    await live.next(); // drain initial

    // Wait one microtask for the subscriptions to register.
    await new Promise((r) => setTimeout(r, 5));
    assert.equal((ctx.subs["users"] ?? []).length, 0, "users should not be subscribed");
    assert.equal((ctx.subs["todos"] ?? []).length, 1, "todos must be subscribed");

    // Firing an event on `users` is a no-op for the broker fan-out
    // because nothing subscribed; the live query should NOT see a rerun.
    ctx.fire("users");
    const findsBefore = ctx.calls.find;
    await new Promise((r) => setTimeout(r, 20));
    assert.equal(ctx.calls.find, findsBefore, "no rerun on unrelated table");
    live.close();
  });

  test("live.close() terminates the iterator with {done: true}", async () => {
    const ctx = makeMockNative();
    installEnv(ctx.native as unknown as { openSubscription: (n: string) => FakeSub });
    const db = _installSchema(
      { todos: { title: t.string().required() } },
      { native: ctx.native },
    );
    const live = db.live(() => db.todos.find({}));
    await live.next();
    live.close();
    const r = await live.next();
    assert.equal(r.done, true);
    // Sub close was propagated.
    await new Promise((r) => setTimeout(r, 5));
    assert.equal((ctx.subs["todos"] ?? []).every((s) => s.closes >= 1), true);
  });

  test("for await ... break triggers close()", async () => {
    const ctx = makeMockNative();
    installEnv(ctx.native as unknown as { openSubscription: (n: string) => FakeSub });
    const db = _installSchema(
      { todos: { title: t.string().required() } },
      { native: ctx.native },
    );
    await db.todos.insert({ title: "x" });
    const live = db.live(() => db.todos.find({}));

    let seen = 0;
    for await (const _rows of live) {
      seen += 1;
      if (seen === 1) break;
    }
    assert.equal(seen, 1);
    // Subs should be closed via the iterator's return().
    await new Promise((r) => setTimeout(r, 5));
    assert.equal((ctx.subs["todos"] ?? []).every((s) => s.closes >= 1), true);
  });

  test("multiple parallel db.live calls don't interfere", async () => {
    const ctx = makeMockNative();
    installEnv(ctx.native as unknown as { openSubscription: (n: string) => FakeSub });
    const db = _installSchema(
      {
        todos: { title: t.string().required() },
        users: { name: t.string().required() },
      },
      { native: ctx.native },
    );
    await db.todos.insert({ title: "t1" });
    await db.users.insert({ name: "u1" });

    const liveA = db.live(() => db.todos.find({}));
    const liveB = db.live(() => db.users.find({}));

    const [a, b] = await Promise.all([liveA.next(), liveB.next()]);
    assert.equal((a.value as AnyRec[])[0].title, "t1");
    assert.equal((b.value as AnyRec[])[0].name, "u1");
    // Each live should have subscribed to exactly one table.
    await new Promise((r) => setTimeout(r, 5));
    assert.equal((ctx.subs["todos"] ?? []).length, 1);
    assert.equal((ctx.subs["users"] ?? []).length, 1);
    liveA.close();
    liveB.close();
  });

  test("calling db.live inside db.transaction rejects synchronously", async () => {
    const ctx = makeMockNative();
    installEnv(ctx.native as unknown as { openSubscription: (n: string) => FakeSub });
    const db = _installSchema(
      { todos: { title: t.string().required() } },
      { native: ctx.native },
    );
    let caught: unknown = null;
    const { error } = await db.transaction(async (_tx) => {
      try {
        db.live(() => db.todos.find({}));
      } catch (e) {
        caught = e;
        throw e;
      }
      return null;
    });
    assert.ok(caught instanceof Error);
    assert.equal((caught as { code?: string }).code, "live_in_transaction");
    assert.ok(error instanceof Error);
  });

  test("explicit { tables } bypasses auto-detection", async () => {
    const ctx = makeMockNative();
    installEnv(ctx.native as unknown as { openSubscription: (n: string) => FakeSub });
    const db = _installSchema(
      { todos: { title: t.string().required() } },
      { native: ctx.native },
    );
    // Use a queryFn that doesn't touch any Collection — auto-detection
    // would find no tables. With explicit { tables: ["todos"] }, the
    // subscription still opens.
    const live = db.live<{ x: number }>(
      async () => [{ x: 1 }],
      { tables: ["todos"] },
    );
    const first = await live.next();
    assert.deepEqual(first.value, [{ x: 1 }]);
    await new Promise((r) => setTimeout(r, 5));
    assert.equal((ctx.subs["todos"] ?? []).length, 1, "todos sub opened via explicit tables");
    live.close();
  });

  test("queue is bounded — fast producer doesn't grow memory unbounded", async () => {
    // A live query with no consumer should NOT accumulate every rerun
    // forever. The internal queue is capped (currently 64) and overflow
    // drops the oldest value to make room for the newest.
    const ctx = makeMockNative();
    installEnv(ctx.native as unknown as { openSubscription: (n: string) => FakeSub });
    const db = _installSchema(
      { todos: { title: t.string().required() } },
      { native: ctx.native },
    );
    await db.todos.insert({ title: "x" });
    const live = db.live(() => db.todos.find({}));

    // Drain the initial result so subsequent reruns land in the queue
    // (we never call .next() again, simulating a stalled consumer).
    await live.next();

    // Fire 1000 events without consuming. The internal queue should not
    // grow past the cap. We probe the iterator's underlying state via
    // a draining loop after all events are scheduled — if the cap is
    // working, only a bounded number of entries remain queued.
    for (let i = 0; i < 1000; i += 1) {
      ctx.fire("todos");
      // Yield to the microtask queue so rerun() actually runs and
      // populates the queue. Without this all 1000 rerun chains stay
      // pending and the cap can't kick in.
      await new Promise<void>((r) => setTimeout(r, 0));
    }

    // Now drain the queue and count items received. The cap is 64, and
    // we drop oldest-value on overflow, so a bounded number of values
    // ≤ cap+initial should be drainable before next() blocks.
    let drained = 0;
    while (drained < 1000) {
      const tick = await Promise.race([
        live.next(),
        new Promise<{ done: true; value: undefined }>((r) =>
          setTimeout(() => r({ done: true, value: undefined }), 20),
        ),
      ]);
      if (tick.done) break;
      drained += 1;
    }
    // Cap is 64. We expect strictly less than the 1000 events we fired —
    // the cap kept the queue bounded. (Some events also collapse into
    // pending resolvers, which is fine: the property is "bounded", not
    // "exactly 64".)
    assert.ok(
      drained <= 128,
      `expected drained <= 128 with cap=64, got ${drained}`,
    );
    live.close();
  });

  test("Result-shape detection is strict — rows that happen to have {data, error} columns are preserved", async () => {
    // Regression for the loose `"data" in obj && "error" in obj` check.
    // A user queryFn that returns rows containing both `data` and
    // `error` columns (e.g. an event-log table) used to be
    // mis-interpreted as a Result<R[]> and unwrapped to the value of
    // `data`. Strict detection (exactly 2 keys, both `data` and `error`)
    // preserves the rows verbatim.
    const ctx = makeMockNative();
    installEnv(ctx.native as unknown as { openSubscription: (n: string) => FakeSub });
    const db = _installSchema(
      { todos: { title: t.string().required() } },
      { native: ctx.native },
    );
    const live = db.live<{ data: number; error: null; extra: string }>(
      async () => [
        { data: 1, error: null, extra: "first row preserved" },
        { data: 2, error: null, extra: "second row preserved" },
      ],
      { tables: ["todos"] },
    );
    const first = await live.next();
    assert.equal(Array.isArray(first.value), true);
    assert.equal((first.value as AnyRec[]).length, 2);
    assert.equal((first.value as AnyRec[])[0].extra, "first row preserved");
    assert.equal((first.value as AnyRec[])[0].data, 1);
    live.close();
  });

  test("live + with: { fk: true } re-runs on BOTH watched and joined-target tables", async () => {
    // Locks the contract: a queryFn that joins `todos` to `users` via
    // `with: { userId: true }` must auto-detect BOTH tables and trigger
    // a rerun when EITHER mutates. The relation loader calls
    // `targetCol.find(...)` which in turn calls
    // `trackCollectionAccess(this._name)` — so the tracker picks up
    // `users` even though the queryFn never names it.
    const ctx = makeMockNative();
    installEnv(ctx.native as unknown as { openSubscription: (n: string) => FakeSub });
    const db = _installSchema(
      {
        users: { name: t.string().required() },
        todos: { userId: t.ref("users"), title: t.string().required() },
      },
      { native: ctx.native },
    );
    await db.users.insert({ id: 1, name: "Alice" });
    await db.todos.insert({ id: 100, userId: 1, title: "buy milk" });

    const live = db.live(() => db.todos.find({}, { with: { userId: true } }));

    // Drain initial result.
    const first = await live.next();
    assert.equal(Array.isArray(first.value), true);

    // Both tables should have been auto-subscribed. The `users` sub is
    // the key claim — it's only opened if the relation loader's
    // `targetCol.find` fired through `trackCollectionAccess`.
    await new Promise((r) => setTimeout(r, 5));
    assert.equal(
      (ctx.subs["todos"] ?? []).length,
      1,
      "live + with must subscribe to the parent table (todos)",
    );
    assert.equal(
      (ctx.subs["users"] ?? []).length,
      1,
      "live + with must subscribe to the joined-target table (users) — if this fails, _loadRelations is not tracking",
    );

    // Mutation on `todos` triggers a rerun.
    await db.todos.insert({ id: 101, userId: 1, title: "write tests" });
    ctx.fire("todos");
    const afterTodos = await live.next();
    assert.equal(afterTodos.done, false);

    // Mutation on `users` (the joined target) ALSO triggers a rerun.
    const findsBefore = ctx.calls.find;
    ctx.fire("users");
    // Wait for the rerun to land in the queue.
    const afterUsers = await live.next();
    assert.equal(afterUsers.done, false);
    assert.ok(
      ctx.calls.find > findsBefore,
      "an event on the joined-target table must trigger a queryFn rerun",
    );
    live.close();
  });

  test("tables: [] subscribes to nothing (static one-shot) — R3 IMPORTANT-4", async () => {
    // R3 IMPORTANT-4 regression. `tables: []` used to silently fall
    // back to auto-tracking (because `length > 0` was the guard).
    // The fix treats `tables: []` as "subscribe to nothing": the
    // initial result yields, then the iterator stalls (no subscriptions
    // open), waiting for an explicit `close()`.
    const ctx = makeMockNative();
    installEnv(ctx.native as unknown as { openSubscription: (n: string) => FakeSub });
    const db = _installSchema(
      { todos: { title: t.string().required() } },
      { native: ctx.native },
    );
    await db.todos.insert({ title: "static" });

    // Capture console.warn — we expect one warning the first time
    // tables: [] is used in this process.
    const warnings: string[] = [];
    const origWarn = console.warn;
    console.warn = (msg: string) => { warnings.push(String(msg)); };
    try {
      // queryFn touches db.todos — auto-tracking WOULD pick it up. With
      // tables: [], auto-tracking is bypassed entirely.
      const live = db.live(() => db.todos.find({}), { tables: [] });
      const first = await live.next();
      assert.equal(first.done, false);
      assert.equal((first.value as AnyRec[])[0].title, "static");

      // After the first result, no subscriptions must be open.
      await new Promise((r) => setTimeout(r, 5));
      assert.equal(
        (ctx.subs["todos"] ?? []).length,
        0,
        "tables: [] must NOT open any subscriptions",
      );
      // No rerun on a todos change (the table is auto-tracked but
      // we ignored it).
      ctx.fire("todos");
      const findsBefore = ctx.calls.find;
      await new Promise((r) => setTimeout(r, 10));
      assert.equal(ctx.calls.find, findsBefore, "no rerun without subscriptions");

      // One-time warning fired.
      assert.ok(
        warnings.some((w) => w.includes("tables: []")),
        `expected an empty-tables warning, got: ${JSON.stringify(warnings)}`,
      );
      live.close();
    } finally {
      console.warn = origWarn;
    }
  });

  test("Query thenable resolves to data array (Result unwrap)", async () => {
    // The Query builder's awaited form returns Result<T[]>. db.live
    // detects that shape and yields the data (or throws on error).
    const ctx = makeMockNative();
    installEnv(ctx.native as unknown as { openSubscription: (n: string) => FakeSub });
    const db = _installSchema(
      { todos: { title: t.string().required() } },
      { native: ctx.native },
    );
    await db.todos.insert({ title: "via-query" });
    const live = db.live(() => db.todos.find({}).limit(10));
    const first = await live.next();
    assert.equal(Array.isArray(first.value), true);
    assert.equal((first.value as AnyRec[])[0].title, "via-query");
    live.close();
  });
});
