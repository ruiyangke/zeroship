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
import { createDb } from "../src/db.js";
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
    const db = createDb(
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
    const db = createDb(
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
    const db = createDb(
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
    const db = createDb(
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
    const db = createDb(
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
    const db = createDb(
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
    const db = createDb(
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
    const db = createDb(
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

  test("Query thenable resolves to data array (Result unwrap)", async () => {
    // The Query builder's awaited form returns Result<T[]>. db.live
    // detects that shape and yields the data (or throws on error).
    const ctx = makeMockNative();
    installEnv(ctx.native as unknown as { openSubscription: (n: string) => FakeSub });
    const db = createDb(
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
