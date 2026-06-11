import { test, describe } from "node:test";
import assert from "node:assert/strict";

import { createFetchHandler } from "../src/fetch-handler.js";

async function withDispatch<T>(fn: () => Promise<T>): Promise<T> {
  const g = globalThis as unknown as {
    __zsDispatch?: (
      rpc: Record<string, unknown>,
      name: string,
      input: unknown,
      ctx: unknown,
    ) => Promise<unknown>;
  };
  const prev = g.__zsDispatch;
  g.__zsDispatch = async (rpc, name, input, ctx) => {
    const proc = rpc[name] as ((input: unknown, ctx: unknown) => unknown) | undefined;
    if (!proc) throw Object.assign(new Error(`Method not found: ${name}`), { status: 404 });
    return proc(input, ctx);
  };
  try {
    return await fn();
  } finally {
    if (prev === undefined) delete g.__zsDispatch;
    else g.__zsDispatch = prev;
  }
}

describe("createFetchHandler — superjson wire", () => {
  test("revives rich input values before dispatch", async () => {
    await withDispatch(async () => {
      const handler = createFetchHandler(async () => ({
        userDefault: {},
        fetch: undefined,
        rpc: {
          inspect(input: unknown) {
            return {
              isDate: input instanceof Date,
              iso: input instanceof Date ? input.toISOString() : null,
            };
          },
        },
      }));

      const res = await handler(
        new Request("https://app.test/__zeroship/v1/inspect", {
          method: "POST",
          body: JSON.stringify({
            json: "2026-01-01T00:00:00.000Z",
            meta: { values: ["Date"], v: 1 },
          }),
        }),
        {},
        {},
      );
      const body = (await res.json()) as { json: unknown };

      assert.equal(res.status, 200);
      assert.deepEqual(body.json, {
        isDate: true,
        iso: "2026-01-01T00:00:00.000Z",
      });
    });
  });

  test("sanitizes 5xx RPC errors before serializing to the client", async () => {
    await withDispatch(async () => {
      const handler = createFetchHandler(async () => ({
        userDefault: {},
        fetch: undefined,
        rpc: {
          fail() {
            const err = new Error("postgres://internal/schema");
            Object.assign(err, {
              status: 500,
              code: "INTERNAL",
              details: { host: "db.internal" },
            });
            throw err;
          },
        },
      }));
      const log = console.error;
      console.error = () => {};
      try {
        const res = await handler(
          new Request("https://app.test/__zeroship/v1/fail", {
            method: "POST",
            body: JSON.stringify({ json: null }),
          }),
          {},
          {},
        );
        const body = (await res.json()) as Record<string, unknown>;

        assert.equal(res.status, 500);
        assert.equal(body.message, "internal error");
        assert.equal(body.name, "Error");
        assert.equal(typeof body.request_id, "string");
        assert.equal(JSON.stringify(body).includes("postgres://internal"), false);
        assert.equal(JSON.stringify(body).includes("db.internal"), false);
      } finally {
        console.error = log;
      }
    });
  });

  // ISS-67: a `requireUser()`-shaped throw carries an explicit `status: 401`.
  // The fetch-handler's `statusFromError` must honor it (4xx), and because the
  // body-sanitizer only blanks 5xx, the "Authentication required" message and
  // the `unauthenticated` code reach the client intact — NOT masked to 500 /
  // "internal error".
  test("honors a requireUser 401 throw and does NOT mask its message", async () => {
    await withDispatch(async () => {
      const handler = createFetchHandler(async () => ({
        userDefault: {},
        fetch: undefined,
        rpc: {
          guarded() {
            // The exact shape the kernel/SDK requireUser throws.
            throw Object.assign(new Error("Authentication required"), {
              status: 401,
              code: "unauthenticated",
            });
          },
        },
      }));

      const res = await handler(
        new Request("https://app.test/__zeroship/v1/guarded", {
          method: "POST",
          body: JSON.stringify({ json: null }),
        }),
        {},
        {},
      );
      const body = (await res.json()) as Record<string, unknown>;

      assert.equal(res.status, 401, "401 throw must surface as 401, not 500");
      assert.equal(body.message, "Authentication required", "4xx message must NOT be masked");
      assert.notEqual(body.message, "internal error");
      assert.equal(body.code, "unauthenticated");
    });
  });

  test("serializes rich output values with meta", async () => {
    await withDispatch(async () => {
      const handler = createFetchHandler(async () => ({
        userDefault: {},
        fetch: undefined,
        rpc: {
          today() {
            return new Date("2026-01-01T00:00:00.000Z");
          },
        },
      }));

      const res = await handler(
        new Request("https://app.test/__zeroship/v1/today", {
          method: "POST",
          body: JSON.stringify({ json: null }),
        }),
        {},
        {},
      );
      const text = await res.text();

      assert.equal(res.status, 200);
      assert.match(text, /"json":"2026-01-01T00:00:00\.000Z"/);
      assert.match(text, /"meta":\{"values":\["Date"\],"v":1\}/);
    });
  });
});
