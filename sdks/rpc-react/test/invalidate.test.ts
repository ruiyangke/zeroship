/**
 * Phase 5 — `rpcInvalidate("prefix.")` for prefix-based bulk invalidation.
 *
 * Per spec §10:
 *
 *   import { rpcInvalidate } from "@zeroship/rpc-react";
 *   await rpcInvalidate("todos.");   // all queries with id starting with "todos."
 *
 * Implementation: walks the QueryClient's QueryCache and invalidates
 * every query whose first key segment (the wireId) starts with the
 * given prefix. The `[wireId, input]` shape from `__makeProcedure`
 * makes this a one-liner.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { QueryClient } from "@tanstack/react-query";
import { _hookRegistry } from "@zeroship/rpc-client/_hooks";

// Importing the package side-effect-populates the registry; we set
// `queryClient` explicitly per-test to avoid cross-test contamination.
import { rpcInvalidate } from "../src/index.js";

describe("rpcInvalidate — prefix invalidation", () => {
  test("invalidates queries whose id starts with prefix", async () => {
    const qc = new QueryClient({
      defaultOptions: { queries: { retry: false } },
    });
    _hookRegistry.queryClient = qc;

    // Seed the cache with a few entries.
    qc.setQueryData(["todos.list"], [{ id: 1 }]);
    qc.setQueryData(["todos.get", { id: 1 }], { id: 1 });
    qc.setQueryData(["users.list"], [{ id: 99 }]);
    qc.setQueryData(["app.health"], "ok");

    // Mark every query as fresh.
    const cache = qc.getQueryCache();
    for (const q of cache.getAll()) {
      // After setQueryData the query is fresh. We invalidate by prefix
      // and assert each query's `state.isInvalidated` flag.
    }

    await rpcInvalidate("todos.");

    const byKey = (k: string): boolean => {
      for (const q of cache.getAll()) {
        if (Array.isArray(q.queryKey) && q.queryKey[0] === k) {
          return q.state.isInvalidated === true;
        }
      }
      return false;
    };

    assert.equal(byKey("todos.list"), true, "todos.list should be invalidated");
    assert.equal(byKey("todos.get"), true, "todos.get should be invalidated");
    assert.equal(byKey("users.list"), false, "users.list must NOT be invalidated");
    assert.equal(byKey("app.health"), false, "app.health must NOT be invalidated");
  });

  test("rpcInvalidate('') invalidates everything", async () => {
    const qc = new QueryClient({
      defaultOptions: { queries: { retry: false } },
    });
    _hookRegistry.queryClient = qc;

    qc.setQueryData(["todos.list"], [{ id: 1 }]);
    qc.setQueryData(["users.list"], [{ id: 1 }]);

    await rpcInvalidate("");

    for (const q of qc.getQueryCache().getAll()) {
      assert.equal(q.state.isInvalidated, true, `${q.queryKey} should be invalidated`);
    }
  });

  test("throws when no QueryClient is mounted", async () => {
    // Save and clear the registry.
    const saved = _hookRegistry.queryClient;
    _hookRegistry.queryClient = undefined;
    await assert.rejects(
      rpcInvalidate("todos."),
      (err) => err instanceof Error && /ZeroshipProvider/.test(err.message),
    );
    _hookRegistry.queryClient = saved;
  });
});
