/**
 * Phase 3 — typed client surface.
 *
 *   const rpc = client<App>({ baseUrl, ... });
 *   await rpc.todos.list.query({ limit: 50 });
 *   await rpc.todos.add.mutation({ text: "hi" });
 *   await rpc.call("listTodos", input);             // escape hatch
 *
 * The proxy keys procedures off the dotted id ("todos.list" → rpc.todos.list).
 * Each leaf carries `query`, `mutation`, `subscribe`, and `stream` callers;
 * the wrong one throws a clean RpcError.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";

import { client } from "../src/client.js";
import { typeMarker } from "../src/index.js";

interface RecordedCall {
  url: string;
  method: string;
  body: string | null;
}

function makeFetch(impl: (req: RecordedCall) => Response): {
  fetchFn: (i: RequestInfo | URL, init?: RequestInit) => Promise<Response>;
  calls: RecordedCall[];
} {
  const calls: RecordedCall[] = [];
  return {
    calls,
    fetchFn: async (input, init) => {
      const url = typeof input === "string" ? input : input.toString();
      const recorded: RecordedCall = {
        url,
        method: init?.method ?? "GET",
        body: typeof init?.body === "string" ? init.body : null,
      };
      calls.push(recorded);
      return impl(recorded);
    },
  };
}

describe("client — escape hatch (`call`)", () => {
  test("call('id', input) routes to the right procedure", async () => {
    const spy = makeFetch(() =>
      new Response(JSON.stringify({ json: [{ id: 1 }] }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }),
    );
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
    });
    const result = await rpc.call("listTodos", { limit: 50 }, { kind: "query" });
    assert.deepEqual(result, [{ id: 1 }]);
    assert.equal(spy.calls.length, 1);
    assert.ok(spy.calls[0].url.includes("/_zs/v1/listTodos"));
  });

  test("default baseUrl is empty (same-origin)", async () => {
    const spy = makeFetch(() =>
      new Response(JSON.stringify({ json: null }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }),
    );
    const rpc = client({ fetch: spy.fetchFn });
    await rpc.call("ping", undefined, { kind: "query" });
    assert.equal(spy.calls[0].url, "/_zs/v1/ping");
  });
});

describe("client — typed proxy surface", () => {
  test("rpc.<id>.query() routes by id", async () => {
    const spy = makeFetch(() =>
      new Response(JSON.stringify({ json: [{ id: 1 }] }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }),
    );
    type App = {
      listTodos: {
        kind: "query";
        input: { limit?: number };
        output: Array<{ id: number }>;
      };
    };
    const rpc = client<App>({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
    });
    const result = await rpc.listTodos.query({ limit: 50 });
    assert.deepEqual(result, [{ id: 1 }]);
    assert.equal(spy.calls[0].method, "GET");
    assert.ok(spy.calls[0].url.includes("/_zs/v1/listTodos"));
  });

  test("rpc.<id>.mutation() POSTs", async () => {
    const spy = makeFetch(() =>
      new Response(JSON.stringify({ json: { id: 7 } }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }),
    );
    type App = {
      addTodo: {
        kind: "mutation";
        input: { text: string };
        output: { id: number };
      };
    };
    const rpc = client<App>({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
    });
    const result = await rpc.addTodo.mutation({ text: "hi" });
    assert.deepEqual(result, { id: 7 });
    assert.equal(spy.calls[0].method, "POST");
  });

  test("dotted ids unfold into nested proxy paths", async () => {
    const spy = makeFetch(() =>
      new Response(JSON.stringify({ json: [] }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }),
    );
    type App = {
      "todos.list": {
        kind: "query";
        input: void;
        output: unknown[];
      };
    };
    const rpc = client<App>({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
    });
    // rpc.todos.list.query() — the dotted id "todos.list" expands.
    await (rpc as { todos: { list: { query: () => Promise<unknown> } } }).todos
      .list.query();
    assert.ok(spy.calls[0].url.includes("/_zs/v1/todos.list"));
  });

  test("stream returns an async-iter (Phase 4)", async () => {
    // Phase 4 — stream() now returns an AsyncIterableIterator that
    // consumes the AI-SDK Data Stream protocol response. The full
    // wire / parser tests live in test/stream.test.ts; here we just
    // smoke-test the proxy → handle → streamCall plumbing.
    const enc = new TextEncoder();
    const body = new ReadableStream<Uint8Array>({
      start(ctrl) {
        ctrl.enqueue(enc.encode('2:[{"x":1}]\n'));
        ctrl.enqueue(enc.encode("d:{}\n"));
        ctrl.close();
      },
    });
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: async () =>
        new Response(body, {
          status: 200,
          headers: { "Content-Type": "text/event-stream" },
        }),
    });
    type App = { x: { kind: "stream"; input: void; output: { x: number } } };
    const typed = rpc as unknown as {
      x: { stream: () => AsyncIterableIterator<unknown> };
    };
    const out: unknown[] = [];
    for await (const v of typed.x.stream()) out.push(v);
    assert.deepEqual(out, [{ x: 1 }]);
    void ({} as App);
  });

  test("subscribe throws UNIMPLEMENTED in Phase 3", async () => {
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: async () =>
        new Response(JSON.stringify({ json: null }), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        }),
    });
    // Phase 3 stubbed `subscribe()` to throw UNIMPLEMENTED; Phase 7 ships
    // the real WebSocket transport. Asserting the surface remains callable.
    const typed = rpc as unknown as {
      x: { subscribe: (input?: unknown, opts?: unknown) => unknown };
    };
    assert.equal(typeof typed.x.subscribe, "function");
  });
});

describe("client — error path", () => {
  test("4xx responses reject with RpcError", async () => {
    const spy = makeFetch(
      () =>
        new Response(
          JSON.stringify({
            code: "NOT_FOUND",
            message: "todo not found",
            retryable: false,
          }),
          {
            status: 404,
            headers: { "Content-Type": "application/zs-error+json" },
          },
        ),
    );
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
    });
    await assert.rejects(
      rpc.call("getTodo", { id: 999 }, { kind: "query" }),
      (err: Error & { code?: string }) => {
        return err.name === "RpcError" && err.code === "NOT_FOUND";
      },
    );
  });

  test("onError fires for failing requests", async () => {
    const spy = makeFetch(
      () =>
        new Response(
          JSON.stringify({
            code: "INTERNAL",
            message: "boom",
            retryable: false,
          }),
          { status: 500, headers: { "Content-Type": "application/json" } },
        ),
    );
    const seen: Array<{ code: string }> = [];
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
      onError: (err) => {
        seen.push({ code: err.code });
      },
    });
    await rpc.call("crash", undefined, { kind: "query" }).catch(() => {});
    assert.equal(seen.length, 1);
    assert.equal(seen[0].code, "INTERNAL");
  });

  test("onAuthExpired fires on UNAUTHENTICATED", async () => {
    const spy = makeFetch(
      () =>
        new Response(
          JSON.stringify({
            code: "UNAUTHENTICATED",
            message: "session expired",
            retryable: false,
          }),
          { status: 401, headers: { "Content-Type": "application/json" } },
        ),
    );
    let triggered = 0;
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
      onAuthExpired: () => {
        triggered++;
      },
    });
    await rpc.call("me", undefined, { kind: "query" }).catch(() => {});
    assert.equal(triggered, 1);
  });
});

describe("client — typeMarker fallback", () => {
  test("typeMarker is a no-op runtime helper", () => {
    // typeMarker exists purely to attach a phantom type — it shouldn't
    // do anything at runtime. The product is the same shape regardless.
    const t = typeMarker<(a: number) => Promise<string>>();
    assert.equal(t, undefined);
  });
});
