/**
 * `__makeServerProcedure` SSR adapter.
 *
 * The vite-plugin's SSR-enabled-app variant wraps each procedure with
 * `__makeServerProcedure(impl, meta)` so the same `list.useQuery(...)`
 * call works on both server and client. The SSR shape is documented in
 * `docs/proposals/rpc-v2.md` §5 and §10. On the server, the hook must
 * call `impl(input)` directly (no HTTP) and stash the result in the
 * per-request QueryClient so `dehydrate(qc)` ships it to the client
 * for hydration.
 *
 * Tests cover:
 *
 *   1. The wrapped procedure is callable + carries id / kind /
 *      queryKey, matching `__makeProcedure` on the client.
 *   2. `useQuery(input)` calls `impl(input)` directly (no HTTP) and
 *      returns the React Query result with `data` populated.
 *   3. `prefetch(input, qc)` populates the QueryClient cache; a
 *      subsequent `useQuery(sameInput)` is a hit (zero extra calls
 *      to `impl`).
 *   4. Mutations / streams / subscriptions throw INTERNAL during SSR
 *      (they're user-action triggered or long-lived and don't make
 *      sense to render server-side).
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import * as React from "react";
import * as TestRenderer from "react-test-renderer";
import {
  QueryClient,
  QueryClientProvider,
} from "@tanstack/react-query";
import { __makeServerProcedure } from "../src/index.js";

describe("__makeServerProcedure — common surface", () => {
  test("returned object is callable; id, kind, queryKey present", async () => {
    const impl = async (input: { limit: number }): Promise<number[]> =>
      [1, 2, 3].slice(0, input.limit);
    const list = __makeServerProcedure(impl, {
      id: "todos.list",
      kind: "query",
    });
    const result = await list({ limit: 2 });
    assert.deepEqual(result, [1, 2]);
    assert.equal(list.id, "todos.list");
    assert.equal(list.kind, "query");
    assert.deepEqual(list.queryKey({ limit: 2 }), ["todos.list", { limit: 2 }]);
  });
});

describe("__makeServerProcedure — query kind", () => {
  test("useQuery calls impl directly (no HTTP) — result lands as `data`", async () => {
    let invocations = 0;
    const impl = async (input: { limit: number }): Promise<{ id: number }[]> => {
      invocations++;
      return [{ id: 1 }, { id: 2 }, { id: 3 }].slice(0, input.limit);
    };
    const list = __makeServerProcedure(impl, {
      id: "todos.list",
      kind: "query",
    });

    const qc = new QueryClient({
      defaultOptions: { queries: { retry: false } },
    });

    let lastResult: ReturnType<typeof list.useQuery> | undefined;
    function Probe() {
      // The server-side useQuery — wraps React Query's useQuery with a
      // queryKey/queryFn pair that calls impl directly.
      const r = list.useQuery({ limit: 2 });
      lastResult = r;
      return null;
    }

    let renderer: TestRenderer.ReactTestRenderer | undefined;
    await TestRenderer.act(async () => {
      renderer = TestRenderer.create(
        <QueryClientProvider client={qc}>
          <Probe />
        </QueryClientProvider>,
      );
    });
    await TestRenderer.act(async () => {
      await new Promise((r) => setTimeout(r, 5));
    });

    assert.equal(invocations, 1, "impl should be called once");
    const r = lastResult as { data?: unknown; isSuccess?: boolean };
    assert.deepEqual(r.data, [{ id: 1 }, { id: 2 }]);
    assert.equal(r.isSuccess, true);

    await TestRenderer.act(async () => {
      renderer?.unmount();
    });
  });

  test("prefetch populates the cache; subsequent useQuery is a cache hit", async () => {
    let invocations = 0;
    const impl = async (input: { limit: number }) => {
      invocations++;
      return [{ id: 42 }].slice(0, input.limit);
    };
    const list = __makeServerProcedure(impl, {
      id: "todos.list",
      kind: "query",
    });

    const qc = new QueryClient({
      defaultOptions: {
        queries: { retry: false, staleTime: Infinity },
      },
    });

    // Prefetch (no React tree yet) — populates the QueryClient cache.
    await list.prefetch({ limit: 1 }, qc);
    assert.equal(invocations, 1, "prefetch should call impl once");

    let lastResult: ReturnType<typeof list.useQuery> | undefined;
    function Probe() {
      const r = list.useQuery({ limit: 1 });
      lastResult = r;
      return null;
    }
    let renderer: TestRenderer.ReactTestRenderer | undefined;
    await TestRenderer.act(async () => {
      renderer = TestRenderer.create(
        <QueryClientProvider client={qc}>
          <Probe />
        </QueryClientProvider>,
      );
    });

    // Cache hit — should NOT trigger a second impl invocation.
    assert.equal(invocations, 1, "prefetched value must hit; impl stays at 1");
    const r = lastResult as { data?: unknown; isSuccess?: boolean };
    assert.deepEqual(r.data, [{ id: 42 }]);
    assert.equal(r.isSuccess, true);

    await TestRenderer.act(async () => {
      renderer?.unmount();
    });
  });
});

describe("__makeServerProcedure — non-query kinds", () => {
  test("mutations: useMutation throws if invoked during SSR", () => {
    const impl = async () => ({ id: 1 });
    const add = __makeServerProcedure(impl, {
      id: "todos.add",
      kind: "mutation",
    });
    // useMutation should throw — `mutate` happens on user action,
    // not during render.
    assert.throws(() => {
      const useMutation = (add as { useMutation: () => unknown }).useMutation;
      useMutation();
    }, /SSR/i);
  });

  test("streams: useStream throws if invoked during SSR", () => {
    const impl = async function* () {
      yield { x: 1 };
    };
    const search = __makeServerProcedure(impl as never, {
      id: "todos.search",
      kind: "stream",
    });
    assert.throws(() => {
      const useStream = (search as { useStream: (i: unknown) => unknown })
        .useStream;
      useStream({ q: "x" });
    }, /SSR/i);
  });

  test("subscriptions: useSubscription throws if invoked during SSR", () => {
    const impl = async () => null;
    const proc = __makeServerProcedure(impl, {
      id: "todos.changes",
      kind: "subscription",
    });
    assert.throws(() => {
      const useSubscription = (proc as { useSubscription: (i: unknown) => unknown })
        .useSubscription;
      useSubscription({});
    }, /SSR/i);
  });
});
