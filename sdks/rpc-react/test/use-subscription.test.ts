/**
 * `useSubscription` placeholder stub.
 *
 * `useSubscription` is wired into `_hookRegistry.useSubscription` with
 * a stub that throws `UNIMPLEMENTED`. That gives procedures declared as
 * `kind: "subscription"` a clear error at the point of misuse instead
 * of silently breaking.
 *
 * The throw message should still make it clear that WebSocket-backed
 * subscriptions are planned rather than missing accidentally.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { __makeProcedure } from "@zeroship/rpc-client";
// Side-effect import: populates _hookRegistry, including the
// useSubscription stub.
import "../src/index.js";

describe("useSubscription — placeholder stub", () => {
  test("subscription procs throw UNIMPLEMENTED with a follow-up message", () => {
    const proc = __makeProcedure(async () => null, {
      id: "todos.changes",
      kind: "subscription",
    });
    const useSub = (proc as { useSubscription: (i: unknown) => unknown })
      .useSubscription;
    assert.equal(typeof useSub, "function");
    assert.throws(
      () => useSub({}),
      (err: Error & { code?: string }) =>
        err.code === "UNIMPLEMENTED" &&
        /follow-up/i.test(err.message),
    );
  });
});
