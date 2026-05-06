/**
 * Idempotency-key generation.
 *
 *   - Mutations whose App type marks `idempotent: true` get an
 *     `Idempotency-Key: <uuidv7>` header on every `mutation()` call.
 *   - Queries never get the header.
 *   - Mutations not marked idempotent never get the header.
 *
 * The client itself does not retry, but the gateway can dedupe by
 * idempotency key when present, so the header is still load-bearing.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";

import { client } from "../src/client.js";
import { newUuidV7 } from "../src/idempotency.js";

interface RecordedCall {
  url: string;
  method: string;
  headers: Record<string, string>;
}

function makeFetch(): {
  fetchFn: (i: RequestInfo | URL, init?: RequestInit) => Promise<Response>;
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
      } else {
        for (const [k, v] of Object.entries(headerSrc)) {
          headers[k.toLowerCase()] = v as string;
        }
      }
      calls.push({ url, method: init?.method ?? "GET", headers });
      return new Response(JSON.stringify({ json: null }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    },
  };
}

describe("UUIDv7 generator", () => {
  test("generates a uuid-shaped string", () => {
    const id = newUuidV7();
    assert.match(id, /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
  });

  test("each call returns a fresh value", () => {
    const a = newUuidV7();
    const b = newUuidV7();
    assert.notEqual(a, b);
  });

  test("monotonic timestamp prefix encodes the current ms", () => {
    const before = Date.now();
    const id = newUuidV7();
    const after = Date.now();
    // First 12 hex chars = 48-bit ms timestamp.
    const tsHex = id.replace(/-/g, "").slice(0, 12);
    const ts = parseInt(tsHex, 16);
    assert.ok(ts >= before - 5 && ts <= after + 5, `timestamp ${ts} not in [${before}, ${after}]`);
  });
});

describe("client — idempotency header", () => {
  test("mutations with idempotent: true carry Idempotency-Key", async () => {
    const spy = makeFetch();
    type App = {
      addTodo: {
        kind: "mutation";
        idempotent: true;
        input: { text: string };
        output: { id: number };
      };
    };
    const rpc = client<App>({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
      procedures: {
        addTodo: { kind: "mutation", idempotent: true },
      },
    });
    await rpc.addTodo.mutation({ text: "hi" });
    const k = spy.calls[0].headers["idempotency-key"];
    assert.ok(k && k.length > 0, "Idempotency-Key header must be present");
    assert.match(k!, /^[0-9a-f]{8}-/);
  });

  test("mutations NOT marked idempotent: omit the header", async () => {
    const spy = makeFetch();
    type App = {
      logEvent: {
        kind: "mutation";
        input: { type: string };
        output: void;
      };
    };
    const rpc = client<App>({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
      procedures: { logEvent: { kind: "mutation" } },
    });
    await rpc.logEvent.mutation({ type: "click" });
    assert.equal(spy.calls[0].headers["idempotency-key"], undefined);
  });

  test("queries never carry Idempotency-Key", async () => {
    const spy = makeFetch();
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
    });
    await rpc.call("listTodos", undefined, { kind: "query", idempotent: true });
    assert.equal(spy.calls[0].headers["idempotency-key"], undefined);
  });

  test("escape-hatch call() with idempotent: true carries header", async () => {
    const spy = makeFetch();
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetchFn,
    });
    await rpc.call("addTodo", { text: "hi" }, { kind: "mutation", idempotent: true });
    const k = spy.calls[0].headers["idempotency-key"];
    assert.ok(k && k.length > 0);
  });
});
