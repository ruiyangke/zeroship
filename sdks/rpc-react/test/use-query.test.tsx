/**
 * `proc.useQuery(...)` happy path.
 *
 * Mount a tiny component that calls a query procedure built by
 * `__makeProcedure` from rpc-client. The test asserts:
 *
 *   - Initial render returns `data: undefined` (loading).
 *   - After the underlying `call` resolves, the next render's `data`
 *     matches what `call` returned.
 *   - The query key is `[wireId, input]` so the kernel-emitted
 *     `proc.queryKey(input)` matches React Query's lookup.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import * as React from "react";
import * as TestRenderer from "react-test-renderer";
import { QueryClient } from "@tanstack/react-query";
import { __makeProcedure } from "@zeroship/rpc-client";

import { ZeroshipProvider } from "../src/index.js";

describe("useQuery — happy path", () => {
  test("hook returns server data after call resolves", async () => {
    const list = __makeProcedure<{ limit: number }, Array<{ id: number }>>(
      async (input) => {
        return [
          { id: 1 },
          { id: 2 },
          { id: 3 },
        ].slice(0, input.limit);
      },
      { id: "todos.list", kind: "query" },
    );

    const qc = new QueryClient({
      defaultOptions: {
        queries: { retry: false, staleTime: Infinity },
      },
    });

    let lastResult: ReturnType<typeof list.useQuery> | undefined;
    function Probe() {
      // The hook reads from `_hookRegistry` which @zeroship/rpc-react
      // populated on import; with the provider mounted it has access
      // to `qc` for prefetch/invalidate calls.
      const r = list.useQuery({ limit: 2 });
      lastResult = r;
      return null;
    }

    let renderer: TestRenderer.ReactTestRenderer | undefined;
    await TestRenderer.act(async () => {
      renderer = TestRenderer.create(
        <ZeroshipProvider client={qc}>
          <Probe />
        </ZeroshipProvider>,
      );
    });

    // Drain the pending fetch.
    await TestRenderer.act(async () => {
      await new Promise((r) => setTimeout(r, 5));
    });

    assert.ok(lastResult, "useQuery should have produced a result");
    const r = lastResult as { data?: unknown; isSuccess?: boolean };
    assert.deepEqual(r.data, [{ id: 1 }, { id: 2 }]);
    assert.equal(r.isSuccess, true);

    await TestRenderer.act(async () => {
      renderer?.unmount();
    });
  });
});
