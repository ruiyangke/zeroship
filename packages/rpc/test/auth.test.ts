/**
 * Auth resolver covers async functions.
 *
 *   auth: () => Promise<string>  — fetched fresh per request before the
 *                                  client kicks off the underlying fetch.
 *
 * The client awaits the resolver before constructing headers; null /
 * undefined means "skip the Authorization header".
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";

import { client } from "../src/client.js";

interface Recorded {
  headers: Record<string, string>;
}

function makeFetch(): {
  fetchFn: (i: RequestInfo | URL, init?: RequestInit) => Promise<Response>;
  calls: Recorded[];
} {
  const calls: Recorded[] = [];
  return {
    calls,
    fetchFn: async (_input, init) => {
      const headers: Record<string, string> = {};
      const headerSrc = init?.headers ?? {};
      if (headerSrc instanceof Headers) {
        headerSrc.forEach((v, k) => (headers[k.toLowerCase()] = v));
      } else {
        for (const [k, v] of Object.entries(headerSrc)) {
          headers[k.toLowerCase()] = v as string;
        }
      }
      calls.push({ headers });
      return new Response(JSON.stringify({ json: null }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    },
  };
}

describe("auth resolver", () => {
  test("async resolver awaited per request", async () => {
    const spy = makeFetch();
    let n = 0;
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
      auth: async () => {
        await new Promise((r) => setTimeout(r, 1));
        return `tok-${++n}`;
      },
    });

    await rpc.call("a", null, { kind: "query" });
    await rpc.call("b", null, { kind: "query" });

    assert.equal(spy.calls[0].headers["authorization"], "Bearer tok-1");
    assert.equal(spy.calls[1].headers["authorization"], "Bearer tok-2");
  });

  test("resolver returning null/undefined skips header", async () => {
    const spy = makeFetch();
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
      auth: async () => null,
    });

    await rpc.call("a", null, { kind: "query" });
    assert.equal(spy.calls[0].headers["authorization"], undefined);
  });

  test("synchronous resolver string works", async () => {
    const spy = makeFetch();
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
      auth: () => "static-tok",
    });

    await rpc.call("a", null, { kind: "query" });
    assert.equal(spy.calls[0].headers["authorization"], "Bearer static-tok");
  });

  test("static string auth works", async () => {
    const spy = makeFetch();
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
      auth: "literal-string",
    });
    await rpc.call("a", null, { kind: "query" });
    assert.equal(spy.calls[0].headers["authorization"], "Bearer literal-string");
  });

  test("auth resolver throwing rejects the call", async () => {
    const spy = makeFetch();
    void spy;
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
      auth: async () => {
        throw new Error("auth fetch failed");
      },
    });
    await assert.rejects(
      rpc.call("a", null, { kind: "query" }),
      /auth fetch failed/,
    );
  });
});
