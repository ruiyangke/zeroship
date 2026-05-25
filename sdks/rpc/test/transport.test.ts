/**
 * Transport behavior.
 *
 *   query     → GET /_zs/v1/<id>?input=<base64url-superjson>
 *   query (>6KB)
 *             → POST /_zs/v1/<id> with X-Method: GET header
 *   mutation  → POST /_zs/v1/<id> with superjson body
 *
 * superjson round-trips Date / BigInt / Map / Set faithfully; the client
 * uses it by default but can be set to plain JSON for legacy servers.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";

import { client } from "../src/client.js";

interface RecordedCall {
  url: string;
  method: string;
  headers: Record<string, string>;
  body: string | null;
}

function makeFetchSpy(impl: (req: RecordedCall) => Response): {
  fetchFn: (input: RequestInfo | URL, init?: RequestInit) => Promise<Response>;
  calls: RecordedCall[];
} {
  const calls: RecordedCall[] = [];
  return {
    calls,
    fetchFn: async (input, init) => {
      const url = typeof input === "string" ? input : input.toString();
      const headers: Record<string, string> = {};
      const headerSrc = init?.headers ?? {};
      if (headerSrc instanceof Headers) {
        headerSrc.forEach((v, k) => (headers[k.toLowerCase()] = v));
      } else if (Array.isArray(headerSrc)) {
        for (const [k, v] of headerSrc) headers[k.toLowerCase()] = v;
      } else {
        for (const [k, v] of Object.entries(headerSrc)) {
          headers[k.toLowerCase()] = v as string;
        }
      }
      const body = typeof init?.body === "string" ? init.body : null;
      const recorded: RecordedCall = {
        url,
        method: init?.method ?? "GET",
        headers,
        body,
      };
      calls.push(recorded);
      return impl(recorded);
    },
  };
}

function jsonResponse(body: unknown, init?: ResponseInit): Response {
  // Default body shape — superjson envelope { json, meta? }.
  return new Response(JSON.stringify({ json: body }), {
    status: 200,
    headers: { "Content-Type": "application/json" },
    ...init,
  });
}

describe("transport — query (GET)", () => {
  test("query encodes input via base64url superjson and uses GET", async () => {
    const spy = makeFetchSpy(() => jsonResponse([{ id: 1 }]));
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
      transformer: "superjson",
    });

    const result = await rpc.call("listTodos", { limit: 50 }, { kind: "query" });

    assert.deepEqual(result, [{ id: 1 }]);
    assert.equal(spy.calls.length, 1);
    const c = spy.calls[0];
    assert.equal(c.method, "GET");
    assert.ok(c.url.startsWith("https://api.test/_zs/v1/listTodos?input="), `got ${c.url}`);

    // The encoded input should be base64url-encoded superjson, decodable
    // back to the original value.
    const u = new URL(c.url);
    const inputParam = u.searchParams.get("input");
    assert.ok(inputParam);
    // base64url → base64 → JSON
    const b64 = inputParam!.replace(/-/g, "+").replace(/_/g, "/");
    const padded = b64 + "=".repeat((4 - (b64.length % 4)) % 4);
    const decoded = JSON.parse(Buffer.from(padded, "base64").toString("utf8"));
    // Either { json: { limit: 50 } } (superjson) or { limit: 50 } (json).
    const inner = decoded.json ?? decoded;
    assert.deepEqual(inner, { limit: 50 });
  });

  test("query without input omits the query parameter", async () => {
    const spy = makeFetchSpy(() => jsonResponse(null));
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
    });

    await rpc.call("ping", undefined, { kind: "query" });
    assert.equal(spy.calls[0].method, "GET");
    // No `?input=...` for undefined input.
    assert.equal(spy.calls[0].url.includes("?input="), false);
  });

  test("oversize input falls back to POST with X-Method: GET", async () => {
    const spy = makeFetchSpy(() => jsonResponse({ ok: true }));
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
    });
    // 8 KB string — well over the 6 KB threshold.
    const big = { blob: "x".repeat(8 * 1024) };
    await rpc.call("hugeQuery", big, { kind: "query" });

    const c = spy.calls[0];
    assert.equal(c.method, "POST");
    assert.equal(c.url, "https://api.test/_zs/v1/hugeQuery");
    assert.equal(c.headers["x-method"], "GET");
    assert.ok(c.body && c.body.length > 100);
    // The body parses as superjson { json: ... }.
    const bodyParsed = JSON.parse(c.body!);
    assert.equal(bodyParsed.json.blob.length, 8 * 1024);
  });
});

describe("transport — mutation (POST)", () => {
  test("mutation POSTs with superjson body", async () => {
    const spy = makeFetchSpy(() => jsonResponse({ id: 5, text: "hi" }));
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
    });

    const result = await rpc.call("addTodo", { text: "hi" }, { kind: "mutation" });

    assert.deepEqual(result, { id: 5, text: "hi" });
    const c = spy.calls[0];
    assert.equal(c.method, "POST");
    assert.equal(c.url, "https://api.test/_zs/v1/addTodo");
    assert.equal(c.headers["content-type"], "application/json");
    assert.ok(c.body);
    const bodyParsed = JSON.parse(c.body!);
    assert.deepEqual(bodyParsed.json, { text: "hi" });
  });
});

describe("transport — superjson round-trip", () => {
  test("Date values survive the envelope", async () => {
    const inputDate = new Date("2026-01-15T10:00:00Z");
    let serverSeesValid = false;
    const spy = makeFetchSpy((req) => {
      // Decode body (mutation) and verify it's a Date when superjson
      // deserializes it back.
      const body = JSON.parse(req.body!);
      // body has shape { json: { ... }, meta?: { ... } } — the meta
      // table tells superjson which fields were Dates. superjson v2
      // encodes per-key types as arrays: meta.values.when === ["Date"].
      const valEntry = body.meta?.values?.when;
      const tag = Array.isArray(valEntry) ? valEntry[0] : valEntry;
      if (tag === "Date") {
        serverSeesValid = true;
      }
      return jsonResponse({ ok: true });
    });
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
      transformer: "superjson",
    });

    await rpc.call("logEvent", { when: inputDate }, { kind: "mutation" });
    assert.equal(serverSeesValid, true, "superjson meta should mark `when` as Date");
  });

  test("plain JSON transformer omits meta", async () => {
    const spy = makeFetchSpy(() => jsonResponse({ ok: true }));
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
      transformer: "json",
    });

    await rpc.call("plain", { hello: "world" }, { kind: "mutation" });
    const c = spy.calls[0];
    const body = JSON.parse(c.body!);
    // In plain JSON mode the wire is the bare value, no { json, meta }
    // wrapping — just `{"hello":"world"}`.
    assert.deepEqual(body, { hello: "world" });
  });
});

describe("transport — auth", () => {
  test("static auth string sets Authorization header", async () => {
    const spy = makeFetchSpy(() => jsonResponse(null));
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
      auth: "tok-static",
    });
    await rpc.call("listTodos", undefined, { kind: "query" });
    assert.equal(spy.calls[0].headers["authorization"], "Bearer tok-static");
  });

  test("function auth resolves before each call", async () => {
    const spy = makeFetchSpy(() => jsonResponse(null));
    let n = 0;
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
      auth: () => `tok-${++n}`,
    });
    await rpc.call("a", undefined, { kind: "query" });
    await rpc.call("b", undefined, { kind: "query" });
    assert.equal(spy.calls[0].headers["authorization"], "Bearer tok-1");
    assert.equal(spy.calls[1].headers["authorization"], "Bearer tok-2");
  });

  test("missing auth returns null — header omitted", async () => {
    const spy = makeFetchSpy(() => jsonResponse(null));
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
    });
    await rpc.call("listTodos", undefined, { kind: "query" });
    assert.equal(spy.calls[0].headers["authorization"], undefined);
  });
});

describe("transport — request id and per-call options", () => {
  test("X-Request-Id is auto-generated per request", async () => {
    const spy = makeFetchSpy(() => jsonResponse(null));
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
    });
    await rpc.call("a", undefined, { kind: "query" });
    await rpc.call("b", undefined, { kind: "query" });
    const id1 = spy.calls[0].headers["x-request-id"];
    const id2 = spy.calls[1].headers["x-request-id"];
    assert.ok(id1 && id1.length > 0);
    assert.ok(id2 && id2.length > 0);
    assert.notEqual(id1, id2);
  });

  test("per-call headers merge with built-ins", async () => {
    const spy = makeFetchSpy(() => jsonResponse(null));
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
    });
    await rpc.call("a", undefined, {
      kind: "query",
      headers: { "X-Trace": "trace-123", "X-Custom": "yes" },
    });
    assert.equal(spy.calls[0].headers["x-trace"], "trace-123");
    assert.equal(spy.calls[0].headers["x-custom"], "yes");
  });

  test("per-call signal aborts the request", async () => {
    const spy = makeFetchSpy(() => jsonResponse(null));
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: async (_url, init) => {
        // Honor the signal.
        if (init?.signal?.aborted) {
          throw Object.assign(new Error("aborted"), { name: "AbortError" });
        }
        // Wait for abort.
        return new Promise((_resolve, reject) => {
          init?.signal?.addEventListener("abort", () => {
            reject(Object.assign(new Error("aborted"), { name: "AbortError" }));
          });
        });
      },
    });
    void spy;
    const ctrl = new AbortController();
    const p = rpc.call("a", undefined, { kind: "query", signal: ctrl.signal });
    ctrl.abort();
    await assert.rejects(p, (err: Error) => {
      // Either the AbortError surfaces directly or it's wrapped as
      // CANCELLED RpcError.
      return (
        err.name === "AbortError" ||
        ("code" in err && (err as { code: string }).code === "CANCELLED")
      );
    });
  });
});

describe("transport — response body failures", () => {
  test("malformed success body throws RpcError(INTERNAL)", async () => {
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: async () =>
        new Response("{not json", {
          status: 200,
          headers: { "Content-Type": "application/json" },
        }),
      transformer: "json",
    });

    await assert.rejects(
      rpc.call("badBody", undefined, { kind: "query" }),
      (err: Error & { code?: string }) =>
        err.name === "RpcError" &&
        err.code === "INTERNAL" &&
        err.message.includes("invalid rpc response body"),
    );
  });

  test("timeout covers response body reads after headers arrive", async () => {
    const rpc = client({
      baseUrl: "https://api.test",
      timeout: 1,
      fetch: async (_url, init) =>
        new Response(
          new ReadableStream({
            start(controller) {
              init?.signal?.addEventListener("abort", () => {
                controller.error(Object.assign(new Error("aborted"), { name: "AbortError" }));
              });
            },
          }),
          {
            status: 200,
            headers: { "Content-Type": "application/json" },
          },
        ),
      transformer: "json",
    });

    await assert.rejects(
      rpc.call("slowBody", undefined, { kind: "query" }),
      (err: Error & { code?: string }) => err.name === "RpcError" && err.code === "TIMEOUT",
    );
  });
});

describe("transport — retry policy", () => {
  test("retryable query failures retry before surfacing the result", async () => {
    let n = 0;
    const spy = makeFetchSpy(() => {
      n++;
      if (n === 1) {
        return new Response(
          JSON.stringify({
            code: "UNAVAILABLE",
            message: "temporary",
            retryable: true,
          }),
          { status: 503, headers: { "Content-Type": "application/json" } },
        );
      }
      return jsonResponse({ ok: true });
    });
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
      retry: { attempts: 2, baseDelayMs: 0, jitter: false },
    });

    const result = await rpc.call("listTodos", undefined, { kind: "query" });

    assert.deepEqual(result, { ok: true });
    assert.equal(spy.calls.length, 2);
    assert.equal(spy.calls[0].method, "GET");
    assert.equal(spy.calls[1].method, "GET");
  });

  test("idempotent mutation retries reuse one Idempotency-Key", async () => {
    let n = 0;
    const spy = makeFetchSpy(() => {
      n++;
      if (n === 1) {
        return new Response(
          JSON.stringify({
            code: "UNAVAILABLE",
            message: "temporary",
            retryable: true,
          }),
          { status: 503, headers: { "Content-Type": "application/json" } },
        );
      }
      return jsonResponse({ ok: true });
    });
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
      retry: { attempts: 2, baseDelayMs: 0, jitter: false },
    });

    await rpc.call("saveTodo", { text: "hi" }, {
      kind: "mutation",
      idempotent: true,
    });

    assert.equal(spy.calls.length, 2);
    assert.ok(spy.calls[0].headers["idempotency-key"]);
    assert.equal(
      spy.calls[1].headers["idempotency-key"],
      spy.calls[0].headers["idempotency-key"],
    );
  });

  test("non-idempotent mutations do not retry by default", async () => {
    const spy = makeFetchSpy(() =>
      new Response(
        JSON.stringify({
          code: "UNAVAILABLE",
          message: "temporary",
          retryable: true,
        }),
        { status: 503, headers: { "Content-Type": "application/json" } },
      ),
    );
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
      retry: { attempts: 3, baseDelayMs: 0, jitter: false },
    });

    await assert.rejects(
      rpc.call("saveTodo", { text: "hi" }, { kind: "mutation" }),
      (err: Error & { code?: string }) => err.name === "RpcError" && err.code === "UNAVAILABLE",
    );
    assert.equal(spy.calls.length, 1);
  });
});
