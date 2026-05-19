/**
 * `__makeProcedure` + `_hookRegistry`.
 *
 * `docs/proposals/rpc.md` §10 defines this surface:
 *
 *   - `__makeProcedure(call, meta)` returns a callable that ALSO carries
 *     hook getters. The getters read from a closure-private hook
 *     registry that's populated as a side effect of `@zeroship/rpc-react`.
 *   - When the registry is empty (rpc-react not installed/imported),
 *     accessing `.useQuery` throws "Hooks unavailable. Install
 *     @zeroship/rpc-react and mount <ZeroshipProvider>." — never lazy
 *     panic later, so the failure is loud and obvious.
 *   - When the registry is populated, the getter returns a thunk that
 *     invokes the user-supplied hook with `{ queryKey, queryFn, ...options }`.
 *   - Hooks are kind-gated: a `query`-kind procedure has `useQuery`,
 *     `useSuspenseQuery`, `useInfiniteQuery`, `prefetch`, `invalidate`,
 *     `setData`. A `mutation` carries `useMutation`. A `stream` carries
 *     `useStream`. A `subscription` carries `useSubscription`.
 *   - The returned procedure is callable: `add({ text: "hi" })` invokes
 *     the underlying `call(input, opts)`.
 *
 * The registry pattern means `@zeroship/rpc-client` never statically
 * imports `@tanstack/react-query`. Vue / Solid / non-React consumers
 * pull the same package without React-Query bytes.
 */

import { test, describe, beforeEach } from "node:test";
import assert from "node:assert/strict";

import { __makeProcedure } from "../src/index.js";
import { _hookRegistry } from "../src/_hooks.js";

beforeEach(() => {
  // Each test starts with a clean registry.
  _hookRegistry.useQuery = undefined;
  _hookRegistry.useMutation = undefined;
  _hookRegistry.useInfiniteQuery = undefined;
  _hookRegistry.useSuspenseQuery = undefined;
  _hookRegistry.useStream = undefined;
  _hookRegistry.useSubscription = undefined;
  _hookRegistry.queryClient = undefined;
});

describe("__makeProcedure — direct call", () => {
  test("the returned thing is callable; invokes call(input, opts)", async () => {
    let received: unknown = null;
    const fn = __makeProcedure<{ x: number }, string>(
      async (input) => {
        received = input;
        return "ok";
      },
      { id: "demo.echo", kind: "query" },
    );
    const out = await fn({ x: 42 });
    assert.equal(out, "ok");
    assert.deepEqual(received, { x: 42 });
  });

  test("attaches id, kind, queryKey", () => {
    const fn = __makeProcedure(async () => null, {
      id: "todos.list",
      kind: "query",
    });
    assert.equal(fn.id, "todos.list");
    assert.equal(fn.kind, "query");
    assert.deepEqual(fn.queryKey({ limit: 10 }), ["todos.list", { limit: 10 }]);
  });
});

describe("__makeProcedure — hook getters when registry empty", () => {
  test("accessing useQuery throws clear install hint", () => {
    const fn = __makeProcedure(async () => null, {
      id: "todos.list",
      kind: "query",
    });
    assert.throws(
      () => fn.useQuery,
      /@zeroship\/rpc-react/,
    );
  });

  test("accessing useMutation throws when registry empty (kind: mutation)", () => {
    const fn = __makeProcedure(async () => null, {
      id: "todos.add",
      kind: "mutation",
    });
    assert.throws(
      () => fn.useMutation,
      /@zeroship\/rpc-react/,
    );
  });

  test("error message references mounting <ZeroshipProvider>", () => {
    const fn = __makeProcedure(async () => null, {
      id: "x",
      kind: "query",
    });
    try {
      void fn.useQuery;
      assert.fail("expected useQuery getter to throw");
    } catch (err) {
      assert.match(
        (err as Error).message,
        /ZeroshipProvider/,
      );
    }
  });
});

describe("__makeProcedure — kind gating", () => {
  test("query: only query hooks present", () => {
    const fn = __makeProcedure(async () => null, {
      id: "x",
      kind: "query",
    });
    // populate registry so the getters don't throw.
    _hookRegistry.useQuery = () => "useQuery-result";
    _hookRegistry.useSuspenseQuery = () => "useSuspenseQuery-result";
    _hookRegistry.useInfiniteQuery = () => "useInfiniteQuery-result";
    assert.equal(typeof fn.useQuery, "function");
    assert.equal(typeof fn.useSuspenseQuery, "function");
    assert.equal(typeof fn.useInfiniteQuery, "function");
    // useMutation should NOT be present on query procs.
    assert.equal((fn as { useMutation?: unknown }).useMutation, undefined);
  });

  test("mutation: only useMutation present", () => {
    const fn = __makeProcedure(async () => null, {
      id: "x",
      kind: "mutation",
    });
    _hookRegistry.useMutation = () => "useMutation-result";
    assert.equal(typeof fn.useMutation, "function");
    assert.equal((fn as { useQuery?: unknown }).useQuery, undefined);
    assert.equal((fn as { useSuspenseQuery?: unknown }).useSuspenseQuery, undefined);
  });

  test("stream: only useStream present", () => {
    const fn = __makeProcedure(async () => null, {
      id: "x",
      kind: "stream",
    });
    _hookRegistry.useStream = () => "useStream-result";
    assert.equal(typeof fn.useStream, "function");
    assert.equal((fn as { useQuery?: unknown }).useQuery, undefined);
  });

  test("subscription: only useSubscription present", () => {
    const fn = __makeProcedure(async () => null, {
      id: "x",
      kind: "subscription",
    });
    _hookRegistry.useSubscription = () => "useSubscription-result";
    assert.equal(typeof fn.useSubscription, "function");
    assert.equal((fn as { useQuery?: unknown }).useQuery, undefined);
  });
});

describe("__makeProcedure — populated registry plumbing", () => {
  test("useQuery delegates to registry hook with [id, input] queryKey + bound queryFn", () => {
    const callImpl = async (input: unknown) => ({ from: "call", input });
    let receivedConfig: { queryKey?: unknown; queryFn?: () => Promise<unknown> } = {};
    _hookRegistry.useQuery = (config: unknown) => {
      receivedConfig = config as typeof receivedConfig;
      return "OK";
    };
    const fn = __makeProcedure(callImpl, { id: "demo.list", kind: "query" });
    const result = fn.useQuery({ limit: 5 });
    assert.equal(result, "OK");
    assert.deepEqual(receivedConfig.queryKey, ["demo.list", { limit: 5 }]);
    assert.equal(typeof receivedConfig.queryFn, "function");
  });

  test("invalidate calls queryClient.invalidateQueries with the id key", () => {
    let receivedKey: unknown = null;
    _hookRegistry.queryClient = {
      invalidateQueries: ({ queryKey }: { queryKey: unknown }) => {
        receivedKey = queryKey;
        return Promise.resolve();
      },
    };
    const fn = __makeProcedure(async () => null, {
      id: "todos.list",
      kind: "query",
    });
    fn.invalidate();
    assert.deepEqual(receivedKey, ["todos.list"]);
  });

  test("invalidate(input) targets a specific [id, input] key", () => {
    let receivedKey: unknown = null;
    _hookRegistry.queryClient = {
      invalidateQueries: ({ queryKey }: { queryKey: unknown }) => {
        receivedKey = queryKey;
        return Promise.resolve();
      },
    };
    const fn = __makeProcedure(async () => null, {
      id: "todos.list",
      kind: "query",
    });
    fn.invalidate({ limit: 50 });
    assert.deepEqual(receivedKey, ["todos.list", { limit: 50 }]);
  });

  // ── docs/proposals/rpc.md §10 "Idempotency × retry interaction" ──
  //
  // React Query retries call the mutationFn multiple times for one
  // logical `mutate(input)` call. The transport must reuse a SINGLE
  // Idempotency-Key across all attempts so gateway dedupe can replay
  // the first attempt's response on retries 2..N.
  //
  // We assert this by capturing the `idempotencyKey` option that the
  // wrapped mutationFn passes to `call(...)` on each retry.
  test("useMutation: retries reuse the same Idempotency-Key (idempotent: true)", async () => {
    const passedKeys: Array<string | undefined> = [];
    const callImpl = async (
      _input: unknown,
      opts?: { idempotencyKey?: string },
    ) => {
      passedKeys.push(opts?.idempotencyKey);
      return { ok: true };
    };

    let mutationFnRef: ((input: unknown) => Promise<unknown>) | null = null;
    _hookRegistry.useMutation = (config: { mutationFn: (input: unknown) => Promise<unknown> }) => {
      mutationFnRef = config.mutationFn;
      return null;
    };

    const fn = __makeProcedure(callImpl, {
      id: "todos.add",
      kind: "mutation",
      idempotent: true,
    });
    fn.useMutation();
    assert.ok(mutationFnRef, "mutationFn captured");

    // Simulate React Query retry semantics: same input reference is
    // passed to the mutationFn multiple times within one logical
    // mutate() call.
    const input = { text: "hi" };
    await mutationFnRef!(input);
    await mutationFnRef!(input);
    await mutationFnRef!(input);
    await mutationFnRef!(input);

    assert.equal(passedKeys.length, 4);
    for (const k of passedKeys) assert.equal(typeof k, "string", "key set");
    assert.equal(
      new Set(passedKeys).size,
      1,
      `all 4 attempts should reuse the same key, got: ${JSON.stringify(passedKeys)}`,
    );
  });

  test("useMutation: idempotent: false → per-attempt key absent", async () => {
    const passedKeys: Array<string | undefined> = [];
    const callImpl = async (
      _input: unknown,
      opts?: { idempotencyKey?: string },
    ) => {
      passedKeys.push(opts?.idempotencyKey);
      return null;
    };

    let mutationFnRef: ((input: unknown) => Promise<unknown>) | null = null;
    _hookRegistry.useMutation = (config: { mutationFn: (input: unknown) => Promise<unknown> }) => {
      mutationFnRef = config.mutationFn;
      return null;
    };

    const fn = __makeProcedure(callImpl, {
      id: "todos.delete",
      kind: "mutation",
      // idempotent omitted → false
    });
    fn.useMutation();
    assert.ok(mutationFnRef, "mutationFn captured");

    const input = { id: "x" };
    await mutationFnRef!(input);
    await mutationFnRef!(input);

    assert.equal(passedKeys.length, 2);
    for (const k of passedKeys) assert.equal(k, undefined, "no key set");
  });
});
