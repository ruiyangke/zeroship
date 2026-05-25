/**
 * Auto-batching for queries.
 *
 *   const rpc = client({ batch: true, ... });
 *   const [a, b, c] = await Promise.all([
 *     rpc.call("listTodos", null, { kind: "query" }),
 *     rpc.call("listUsers", null, { kind: "query" }),
 *     rpc.call("getMe",     null, { kind: "query" }),
 *   ]);
 *   // → ONE POST /_zs/v1/_batch with three entries.
 *
 * Mutations and streams never batch.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";

import { client } from "../src/client.js";

interface Recorded {
  url: string;
  method: string;
  body: string | null;
  headers: Record<string, string>;
}

function batchResponseFor(req: Recorded): Response {
  // Decode the batch body and return one entry per request id.
  const arr = JSON.parse(req.body!) as Array<{
    id: string;
    name: string;
    input?: unknown;
  }>;
  const out = arr.map((e) => ({
    id: e.id,
    status: 200,
    output: { json: { name: e.name, n: arr.length } },
  }));
  return new Response(JSON.stringify(out), {
    status: 200,
    headers: { "Content-Type": "application/zs-batch+json" },
  });
}

function makeFetch(impl: (r: Recorded) => Response): {
  fetchFn: (i: RequestInfo | URL, init?: RequestInit) => Promise<Response>;
  calls: Recorded[];
} {
  const calls: Recorded[] = [];
  return {
    calls,
    fetchFn: async (input, init) => {
      const url = typeof input === "string" ? input : input.toString();
      const headers: Record<string, string> = {};
      const headerSrc = init?.headers ?? {};
      if (headerSrc instanceof Headers) {
        headerSrc.forEach((v, k) => (headers[k.toLowerCase()] = v));
      } else {
        for (const [k, v] of Object.entries(headerSrc)) {
          headers[k.toLowerCase()] = v as string;
        }
      }
      const body = typeof init?.body === "string" ? init.body : null;
      const recorded = { url, method: init?.method ?? "GET", body, headers };
      calls.push(recorded);
      return impl(recorded);
    },
  };
}

describe("auto-batching — opt-in", () => {
  test("three queries fired same tick → one /_batch POST → three resolved", async () => {
    const spy = makeFetch(batchResponseFor);
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
      batch: true,
    });

    const [a, b, c] = await Promise.all([
      rpc.call("listTodos", null, { kind: "query" }),
      rpc.call("listUsers", null, { kind: "query" }),
      rpc.call("getMe", null, { kind: "query" }),
    ]);

    // Exactly one HTTP request — the batch endpoint.
    assert.equal(spy.calls.length, 1, "one batched request");
    assert.equal(spy.calls[0].method, "POST");
    assert.equal(spy.calls[0].url, "https://api.test/_zs/v1/_batch");
    assert.equal(
      spy.calls[0].headers["content-type"],
      "application/zs-batch+json",
    );

    // All three callers see distinct outputs.
    assert.deepEqual(a, { name: "listTodos", n: 3 });
    assert.deepEqual(b, { name: "listUsers", n: 3 });
    assert.deepEqual(c, { name: "getMe", n: 3 });
  });

  test("default batch: false sends individual requests", async () => {
    const spy = makeFetch(
      () =>
        new Response(JSON.stringify({ json: null }), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        }),
    );
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
      // no `batch:` flag — defaults to false
    });
    await Promise.all([
      rpc.call("a", null, { kind: "query" }),
      rpc.call("b", null, { kind: "query" }),
    ]);
    assert.equal(spy.calls.length, 2, "no batching = N individual requests");
  });

  test("mutations are NEVER batched, even with batch: true", async () => {
    let batchCalls = 0;
    let unitaryCalls = 0;
    const spy = makeFetch((req) => {
      if (req.url.endsWith("/_batch")) {
        batchCalls++;
        return batchResponseFor(req);
      }
      unitaryCalls++;
      return new Response(JSON.stringify({ json: { ok: true } }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    });
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
      batch: true,
    });

    await Promise.all([
      rpc.call("listTodos", null, { kind: "query" }),
      rpc.call("addTodo", { text: "x" }, { kind: "mutation" }),
      rpc.call("listUsers", null, { kind: "query" }),
    ]);

    // Two queries batched into one, mutation goes alone.
    assert.equal(batchCalls, 1);
    assert.equal(unitaryCalls, 1);
  });

  test("error in one batch entry doesn't fail the others", async () => {
    const spy = makeFetch((req) => {
      const arr = JSON.parse(req.body!) as Array<{ id: string; name: string }>;
      const out = arr.map((e) => {
        if (e.name === "willFail") {
          return {
            id: e.id,
            status: 404,
            error: {
              code: "NOT_FOUND",
              message: "no such thing",
              retryable: false,
            },
          };
        }
        return { id: e.id, status: 200, output: { json: { name: e.name } } };
      });
      return new Response(JSON.stringify(out), {
        status: 200,
        headers: { "Content-Type": "application/zs-batch+json" },
      });
    });
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
      batch: true,
    });

    const results = await Promise.allSettled([
      rpc.call("ok", null, { kind: "query" }),
      rpc.call("willFail", null, { kind: "query" }),
      rpc.call("alsoOk", null, { kind: "query" }),
    ]);

    assert.equal(results[0].status, "fulfilled");
    assert.equal(results[1].status, "rejected");
    assert.equal(results[2].status, "fulfilled");

    // The rejected one carries an RpcError with the right code.
    if (results[1].status === "rejected") {
      const err = results[1].reason as Error & { code?: string };
      assert.equal(err.code, "NOT_FOUND");
    }
  });
});
