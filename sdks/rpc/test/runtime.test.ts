import { test, describe } from "node:test";
import assert from "node:assert/strict";

import {
  __callProcedure,
  configureRpcClient,
  createRpcClient,
  defineRpcProcedures,
} from "../src/runtime.js";
import type { Query, Stream } from "../src/types.js";

interface RecordedCall {
  url: string;
  method: string;
  headers: Record<string, string>;
  body: string | null;
}

function makeFetch(): {
  fetchFn: (input: RequestInfo | URL, init?: RequestInit) => Promise<Response>;
  calls: RecordedCall[];
} {
  const calls: RecordedCall[] = [];
  return {
    calls,
    fetchFn: async (input, init) => {
      const headers: Record<string, string> = {};
      const src = init?.headers ?? {};
      if (src instanceof Headers) {
        src.forEach((v, k) => (headers[k.toLowerCase()] = v));
      } else if (Array.isArray(src)) {
        for (const [k, v] of src) headers[k.toLowerCase()] = v;
      } else {
        for (const [k, v] of Object.entries(src)) headers[k.toLowerCase()] = v as string;
      }
      calls.push({
        url: typeof input === "string" ? input : input.toString(),
        method: init?.method ?? "GET",
        headers,
        body: typeof init?.body === "string" ? init.body : null,
      });
      return new Response(JSON.stringify({ ok: true }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    },
  };
}

describe("generated direct-call runtime", () => {
  test("createRpcClient builds manual callable procedures without Vite", async () => {
    const spy = makeFetch();
    const rpc = createRpcClient({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
      transformer: "json",
    });
    const listTodos = rpc.query<{ limit: number }, { ok: true }>("todos.list");
    const saveTodo = rpc.mutation<{ text: string }, { ok: true }>("todos.save", {
      idempotent: true,
    });

    await listTodos({ limit: 10 });
    await saveTodo({ text: "hi" }, { idempotencyKey: "idem-manual" });

    assert.equal(spy.calls[0].method, "GET");
    assert.ok(spy.calls[0].url.startsWith("https://api.test/__zeroship/v1/todos.list"));
    assert.equal(spy.calls[1].method, "POST");
    assert.equal(spy.calls[1].url, "https://api.test/__zeroship/v1/todos.save");
    assert.equal(spy.calls[1].headers["idempotency-key"], "idem-manual");
    assert.equal(listTodos.id, "todos.list");
    assert.equal(saveTodo.kind, "mutation");
  });

  test("configureRpcClient customizes baseUrl, auth, headers, and transformer", async () => {
    const spy = makeFetch();
    const restore = configureRpcClient({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
      auth: async () => "tok",
      headers: { "X-App": "demo" },
      transformer: "json",
    });
    try {
      const out = await __callProcedure("todos.list", "query", { limit: 1 });

      assert.deepEqual(out, { ok: true });
      assert.equal(spy.calls.length, 1);
      assert.equal(spy.calls[0].method, "GET");
      assert.ok(spy.calls[0].url.startsWith("https://api.test/__zeroship/v1/todos.list"));
      assert.equal(spy.calls[0].headers.authorization, "Bearer tok");
      assert.equal(spy.calls[0].headers["x-app"], "demo");
    } finally {
      restore();
    }
  });

  test("direct calls forward explicit idempotency keys", async () => {
    const spy = makeFetch();
    const restore = configureRpcClient({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
      transformer: "json",
    });
    try {
      await __callProcedure(
        "todos.save",
        "mutation",
        { text: "hi" },
        { idempotencyKey: "idem-123" },
        { idempotent: true },
      );

      assert.equal(spy.calls[0].headers["idempotency-key"], "idem-123");
    } finally {
      restore();
    }
  });

  test("registry-backed call() dispatches by procedure metadata", async () => {
    const spy = makeFetch();
    type AppRpc = {
      "todos.list": Query<{ limit?: number }, { ok: true }>;
      "chat.ask": Stream<{ prompt: string }, { token: string }>;
    };
    const procedures = defineRpcProcedures<AppRpc>()({
      "todos.list": { kind: "query" },
      "chat.ask": { kind: "stream" },
    });
    const rpc = createRpcClient<AppRpc>({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
      transformer: "json",
      procedures,
    });

    await rpc.call("todos.list", { limit: 5 });

    assert.equal(spy.calls.length, 1);
    assert.equal(spy.calls[0].method, "GET");
    assert.ok(spy.calls[0].url.startsWith("https://api.test/__zeroship/v1/todos.list"));
  });

  test("call() can read procedure metadata from global configuration", async () => {
    const spy = makeFetch();
    const restore = configureRpcClient({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
      transformer: "json",
      procedures: {
        "todos.list": { kind: "query" },
      },
    });
    try {
      const rpc = createRpcClient();

      await rpc.call("todos.list", { limit: 5 });

      assert.equal(spy.calls.length, 1);
      assert.equal(spy.calls[0].method, "GET");
      assert.ok(spy.calls[0].url.startsWith("https://api.test/__zeroship/v1/todos.list"));
    } finally {
      restore();
    }
  });

  test("stream procedures expose streamUrl for AI SDK transports", async () => {
    const rpc = createRpcClient();
    const chat = rpc.stream<{ prompt: string }, { token: string }>("chat.ask");
    const url = await chat.streamUrl({ prompt: "hello" });

    assert.equal(typeof url, "string");
    assert.ok(url.startsWith("/__zeroship/v1/chat.ask?input="), url);
  });
});
