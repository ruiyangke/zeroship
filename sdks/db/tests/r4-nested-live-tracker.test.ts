/**
 * R4 IMPORTANT-2 regression — nested `db.live` whose `queryFn` touches
 * Collections must NOT leak reads into an enclosing live's tracker
 * when the inner live opts out of auto-tracking via explicit
 * `{ tables: [...] }`. Pre-fix the explicit-tables branch in
 * `firstRun()` skipped the `liveTracker.current` save/replace — child
 * reads pushed into the OUTER live's collections set, inflating
 * subscriptions and triggering spurious reruns. The fix installs a
 * throwaway sink tracker for the duration of the explicit-tables
 * queryFn so its child reads land in /dev/null.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { env } from "zeroship";
import { installSchemaForTest } from "./_install-helper.js";
import { t } from "@zeroship/db";
import type { LiveQuery } from "@zeroship/db";
import type { NativeDb } from "../src/native.js";

type AnyRec = Record<string, unknown>;
type SubEvent =
  | { kind: "change"; op: "insert" | "update" | "delete"; collection: string; pk: number; columns: string[] }
  | { kind: "resync" }
  | { kind: "closed" };

interface FakeSub {
  ready(): Promise<void>;
  next(): Promise<SubEvent | null>;
  close(): void;
  emit(ev: SubEvent): void;
}

function makeFakeSub(): FakeSub {
  const queue: SubEvent[] = [];
  let pending: ((ev: SubEvent | null) => void) | null = null;
  let closed = false;
  return {
    async ready() {},
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

function installEnv(native: unknown): void {
  (env as { db?: unknown }).db = native;
}

function makeMockNative() {
  const rowsByTable: Record<string, AnyRec[]> = {};
  const subs: Record<string, FakeSub[]> = {};
  const native = {
    collection(name: string) {
      return {
        async find(_f: AnyRec, _o: AnyRec) {
          return [...(rowsByTable[name] ?? [])];
        },
        async insert(row: AnyRec) {
          (rowsByTable[name] ??= []).push(row);
          return row;
        },
        // **P9 PR 1** — Collection-scoped openSubscription replaces
        // the deleted Db-level entry point.
        openSubscription(): FakeSub {
          const sub = makeFakeSub();
          (subs[name] ??= []).push(sub);
          return sub;
        },
      };
    },
  };
  return { native: native as unknown as NativeDb, rowsByTable, subs };
}

describe("R4 IMPORTANT-2 — nested live tracker isolation (explicit tables)", () => {
  test("inner db.live({tables}) inside outer db.live() must NOT leak reads upward", async () => {
    const ctx = makeMockNative();
    installEnv(ctx.native);
    const db = installSchemaForTest(
      {
        todos: { title: t.string().required() },
        users: { name: t.string().required() },
      },
      { native: ctx.native },
    );
    await db.todos.insert({ title: "outer-row" });
    await db.users.insert({ name: "inner-row" });

    // Outer live's queryFn does TWO things:
    //   1. Touches db.todos (should be tracked → outer watches "todos").
    //   2. Constructs an inner live with `tables: ["users"]` and queryFn
    //      that touches db.users.
    // Pre-fix, the inner's `db.users.find()` ran while the OUTER tracker
    // was still installed (explicit-tables branch skipped the stack
    // save/restore). Outer ended up watching ["todos", "users"] and
    // would spuriously rerun on every users mutation.
    // `db.live` is an overloaded call signature (see DbExtensions.live in
    // src/db-types.ts); instantiating it via `typeof db.live<AnyRec>`
    // without a call resolved to `never` for `ReturnType`. `LiveQuery<R>`
    // is the actual named return type - reference it directly.
    let innerLive: LiveQuery<AnyRec> | null = null;
    const outer = db.live(async () => {
      const rows = await db.todos.find({});
      if (innerLive === null) {
        innerLive = db.live(
          () => db.users.find({}),
          { tables: ["users"] },
        );
      }
      return rows;
    });

    // Drain the outer's initial result to ensure firstRun completes.
    const first = await outer.next();
    assert.equal(first.done, false);

    // Give the wiring a tick.
    await new Promise((r) => setTimeout(r, 5));

    // The outer must have subscribed to ONLY `todos` (the table its
    // queryFn directly touched). It must NOT have subscribed to `users`
    // — the inner's queryFn touched `users`, but the inner's explicit-
    // tables contract says "do my own bookkeeping; don't leak."
    assert.equal(
      (ctx.subs["todos"] ?? []).length,
      1,
      "outer live must subscribe to todos (its own auto-tracked read)",
    );
    const usersSubs = ctx.subs["users"] ?? [];
    // The inner live opens exactly one users sub (its explicit tables).
    // The outer must NOT have opened a second one.
    assert.equal(
      usersSubs.length,
      1,
      `outer must NOT inherit inner's reads — expected exactly 1 users sub (from inner only), got ${usersSubs.length}`,
    );

    outer.close();
    // `innerLive` is only ever reassigned inside the `db.live(async () =>
    // {...})` queryFn closure above - TS's control-flow narrowing does not
    // track assignments made inside a callback whose invocation it can't
    // see, so by this point it treats the variable as narrowed away from
    // its declared type entirely (`never`), not as the declared `LiveQuery
    // <AnyRec> | null`. The awaited `outer.next()` above guarantees the
    // closure ran at least once by this line; re-asserting the declared
    // type is correct, not a weakened check.
    (innerLive as LiveQuery<AnyRec> | null)?.close();
  });
});
