/**
 * Robustness — `Subscription` iterator close semantics (Gap N / AA).
 *
 * Three edges:
 *   1. `next()` after explicit `close()` → iterator yields `{done: true}`.
 *   2. `for await` with an early `break` → underlying `close()` called once.
 *   3. `for await` body throws → underlying `close()` called once.
 */
import { test, describe, before } from "node:test";
import assert from "node:assert/strict";
import { env } from "zeroship";

type SubEvent =
  | { kind: "change"; op: "insert"; collection: string; pk: number; columns: string[] }
  | { kind: "resync" }
  | { kind: "closed" };

interface FakeSub {
  next(): Promise<SubEvent | null>;
  close(): void;
}

function makeFakeSub(events: (SubEvent | null)[]): FakeSub & { closes: number; pulls: number } {
  let i = 0;
  const state = { closes: 0, pulls: 0 };
  const sub: FakeSub & typeof state = {
    closes: 0,
    pulls: 0,
    async next() {
      state.pulls += 1;
      const ev = events[i++];
      if (ev === undefined) return null;
      return ev;
    },
    close() {
      state.closes += 1;
    },
  };
  Object.defineProperty(sub, "closes", { get: () => state.closes });
  Object.defineProperty(sub, "pulls", { get: () => state.pulls });
  return sub;
}

let subscribe: typeof import("../src/subscribe.js").subscribe;

before(async () => {
  // The subscribe module reads env.db.openSubscription lazily on each
  // call, so we just install the test env up front.
  ({ subscribe } = await import("../src/subscribe.js"));
});

describe("Subscription iterator — close semantics", () => {
  test("next() after explicit close() returns {done: true} cleanly", async () => {
    const fake = makeFakeSub([
      { kind: "change", op: "insert", collection: "m", pk: 1, columns: ["x"] },
      { kind: "change", op: "insert", collection: "m", pk: 2, columns: ["x"] },
    ]);
    (env as { db?: unknown }).db = {
      openSubscription: (_c: string) => fake,
    };
    const sub = subscribe("messages");
    const iter = sub[Symbol.asyncIterator]();

    const first = await iter.next();
    assert.equal(first.done, false);
    assert.equal((first.value as SubEvent).kind, "change");

    sub.close();
    assert.equal(fake.closes, 1);

    // Subsequent next() must terminate cleanly — no more events pulled
    // from the underlying wrapper.
    const second = await iter.next();
    assert.equal(second.done, true);
    assert.equal(fake.pulls, 1, "must not pull after close");

    // And a second close() is idempotent.
    sub.close();
    assert.equal(fake.closes, 1);
  });

  test("for await ... break triggers close() exactly once", async () => {
    const fake = makeFakeSub([
      { kind: "change", op: "insert", collection: "m", pk: 1, columns: ["x"] },
      { kind: "change", op: "insert", collection: "m", pk: 2, columns: ["x"] },
      { kind: "change", op: "insert", collection: "m", pk: 3, columns: ["x"] },
    ]);
    (env as { db?: unknown }).db = {
      openSubscription: (_c: string) => fake,
    };
    const sub = subscribe("messages");

    let seen = 0;
    for await (const _ev of sub) {
      seen += 1;
      if (seen === 1) break;
    }
    assert.equal(seen, 1);
    assert.equal(fake.closes, 1, "close called once via iterator return()");
  });

  test("for await ... throw triggers close() exactly once", async () => {
    const fake = makeFakeSub([
      { kind: "change", op: "insert", collection: "m", pk: 1, columns: ["x"] },
      { kind: "change", op: "insert", collection: "m", pk: 2, columns: ["x"] },
    ]);
    (env as { db?: unknown }).db = {
      openSubscription: (_c: string) => fake,
    };
    const sub = subscribe("messages");

    let caught: unknown = null;
    try {
      for await (const _ev of sub) {
        throw new Error("body bombed");
      }
    } catch (e) {
      caught = e;
    }
    assert.ok(caught instanceof Error);
    assert.equal((caught as Error).message, "body bombed");
    assert.equal(fake.closes, 1, "close called once via iterator throw()");
  });

  test("underlying next() returning null terminates the iterator", async () => {
    // null from the native wrapper means "the native side closed itself";
    // the JS iterator must reflect that with {done: true}.
    const fake = makeFakeSub([]); // empty event list → next() returns null
    (env as { db?: unknown }).db = {
      openSubscription: (_c: string) => fake,
    };
    const sub = subscribe("messages");
    const iter = sub[Symbol.asyncIterator]();
    const r = await iter.next();
    assert.equal(r.done, true);
  });
});
