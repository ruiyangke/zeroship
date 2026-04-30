/**
 * Phase 5 — `useSubscription` placeholder stub.
 *
 * Subscriptions land in Phase 7 (parallel work). For Phase 5 the
 * `useSubscription` slot is wired into `_hookRegistry.useSubscription`
 * with a stub that throws UNIMPLEMENTED — that way procedure handles
 * declared as `kind: "subscription"` get a clear error message at the
 * point of misuse instead of silently breaking.
 *
 * The throw message MUST mention follow-up landing so users know it's
 * tracked.
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
