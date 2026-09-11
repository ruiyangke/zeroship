// @vitest-environment node
import { afterEach, describe, expect, it, vi } from "vitest";
import { call } from "../tests/rpc";

afterEach(() => vi.unstubAllGlobals());

function respond(body: unknown, status: number) {
  vi.stubGlobal("fetch", vi.fn(async () => new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  })));
}

describe("acceptance RPC response parsing", () => {
  it.each([null, false, 0, [], { id: "todo_result", title: "saved" }])(
    "preserves a successful JSON result: %j",
    async (json) => {
      respond({ json }, 200);
      await expect(call("http://example.test", "todos.get")).resolves.toEqual({ json });
    },
  );

  it.each(["FOREIGN_KEY_VIOLATION", "UNIQUE_VIOLATION"])(
    "captures a top-level HTTP failure without losing its fields: %s",
    async (code) => {
      const error = {
        message: "The database rejected the write",
        name: "Error",
        code,
        request_id: "request_probe",
        hint: "Check the referenced row or unique field",
      };
      respond(error, 409);
      await expect(call("http://example.test", "todos.create")).resolves.toEqual({ error });
    },
  );

  it("accepts a generic server error without a code", async () => {
    const error = { message: "internal error", request_id: "request_probe" };
    respond(error, 500);
    await expect(call("http://example.test", "todos.get")).resolves.toEqual({ error });
  });

  it.each([
    { status: 200, body: {} },
    { status: 200, body: { message: "unwrapped error" } },
    { status: 200, body: { error: { message: "wrapped error" } } },
    { status: 200, body: { json: null, error: { message: "ambiguous" } } },
    { status: 409, body: { json: null } },
    { status: 409, body: { error: { message: "old wrapper", code: "UNIQUE_VIOLATION" } } },
    { status: 500, body: { message: "ambiguous", json: null } },
    { status: 500, body: { message: "ambiguous", error: {} } },
    { status: 500, body: { code: "INTERNAL_ERROR" } },
    { status: 500, body: { message: null } },
    { status: 500, body: { message: "failed", code: 500 } },
    { status: 500, body: { message: "failed", name: {} } },
    { status: 500, body: { message: "failed", request_id: false } },
  ])("rejects an invalid envelope: $status $body", async ({ status, body }) => {
    respond(body, status);
    await expect(call("http://example.test", "todos.get")).rejects.toThrow(
      `todos.get: invalid response ${status}`,
    );
  });

  it.each([null, [], "not an envelope"])("rejects a non-object body: %j", async (body) => {
    respond(body, 500);
    await expect(call("http://example.test", "todos.get")).rejects.toThrow("Expected object");
  });

  it("rejects a non-JSON HTTP response", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => new Response("upstream unavailable", { status: 502 })));
    await expect(call("http://example.test", "todos.get")).rejects.toBeInstanceOf(SyntaxError);
  });

  it("propagates transport failures", async () => {
    const error = new TypeError("connection refused");
    vi.stubGlobal("fetch", vi.fn(async () => { throw error; }));
    await expect(call("http://example.test", "todos.get")).rejects.toBe(error);
  });
});
