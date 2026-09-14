/**
 * RpcError parsing and code propagation.
 *
 * The wire ships structured error envelopes:
 *
 *   { code, message, details?, retryable, trace_id? }
 *
 * The client decodes the body (superjson or plain JSON) and constructs an
 * RpcError from those fields. The HTTP status is also surfaced for users
 * who care, but the canonical fault code lives in the envelope.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";

import { RpcError, parseErrorResponse, isRpcError } from "../src/error.js";

describe("RpcError", () => {
  test("constructs from envelope fields", () => {
    const err = new RpcError({
      code: "NOT_FOUND",
      message: "todo not found",
      details: { id: 42 },
      retryable: false,
      status: 404,
      traceId: "01HJQK",
    });

    assert.equal(err.code, "NOT_FOUND");
    assert.equal(err.message, "todo not found");
    assert.deepEqual(err.details, { id: 42 });
    assert.equal(err.retryable, false);
    assert.equal(err.status, 404);
    assert.equal(err.trace_id, "01HJQK");
    assert.equal(err.name, "RpcError");
    assert.ok(err instanceof Error);
  });

  test("isRpcError type-guard recognizes own instances", () => {
    const err = new RpcError({
      code: "INTERNAL",
      message: "oops",
      retryable: false,
    });
    assert.equal(isRpcError(err), true);
    assert.equal(isRpcError(new Error("not an rpc error")), false);
    assert.equal(isRpcError(null), false);
    assert.equal(isRpcError({ code: "INTERNAL", message: "x" }), false);
  });

  test("retryable flag follows the envelope, not the code", () => {
    // RESOURCE_EXHAUSTED is conventionally retryable, but if the server
    // explicitly marks it `retryable: false` (e.g. quota over), the
    // client respects the envelope.
    const err = new RpcError({
      code: "RESOURCE_EXHAUSTED",
      message: "quota exceeded for the day",
      retryable: false,
    });
    assert.equal(err.retryable, false);
  });
});

describe("parseErrorResponse", () => {
  test("parses zs-error+json envelope", async () => {
    const body = JSON.stringify({
      code: "INVALID_ARGUMENT",
      message: "input.text must be at least 1 character",
      details: { path: ["text"], expected: "min 1, max 500" },
      trace_id: "01HJQK",
      retryable: false,
    });
    const res = new Response(body, {
      status: 400,
      headers: { "Content-Type": "application/zs-error+json" },
    });

    const err = await parseErrorResponse(res);
    assert.equal(err.code, "INVALID_ARGUMENT");
    assert.equal(err.message, "input.text must be at least 1 character");
    assert.deepEqual(err.details, { path: ["text"], expected: "min 1, max 500" });
    assert.equal(err.retryable, false);
    assert.equal(err.trace_id, "01HJQK");
    assert.equal(err.status, 400);
  });

  test("parses plain JSON error body", async () => {
    const body = JSON.stringify({
      code: "PERMISSION_DENIED",
      message: "forbidden",
      retryable: false,
    });
    const res = new Response(body, {
      status: 403,
      headers: { "Content-Type": "application/json" },
    });

    const err = await parseErrorResponse(res);
    assert.equal(err.code, "PERMISSION_DENIED");
    assert.equal(err.status, 403);
  });

  test("falls back to status-based code when envelope absent", async () => {
    const res = new Response("Internal server error", {
      status: 500,
      headers: { "Content-Type": "text/plain" },
    });

    const err = await parseErrorResponse(res);
    assert.equal(err.code, "INTERNAL");
    assert.equal(err.status, 500);
  });

  test("maps HTTP statuses to codes when envelope omits code", async () => {
    const cases: Array<{ status: number; code: string }> = [
      { status: 401, code: "UNAUTHENTICATED" },
      { status: 403, code: "PERMISSION_DENIED" },
      { status: 404, code: "NOT_FOUND" },
      { status: 409, code: "ALREADY_EXISTS" },
      { status: 429, code: "RESOURCE_EXHAUSTED" },
      { status: 503, code: "UNAVAILABLE" },
      { status: 504, code: "TIMEOUT" },
    ];
    for (const c of cases) {
      const res = new Response(JSON.stringify({ message: "x" }), {
        status: c.status,
        headers: { "Content-Type": "application/json" },
      });
      const err = await parseErrorResponse(res);
      assert.equal(err.code, c.code, `status ${c.status} → code ${c.code}`);
    }
  });

  test("retryable defaults follow code when envelope omits", async () => {
    const res = new Response(
      JSON.stringify({ code: "TIMEOUT", message: "deadline exceeded" }),
      { status: 504, headers: { "Content-Type": "application/json" } },
    );
    const err = await parseErrorResponse(res);
    assert.equal(err.retryable, true);
  });
});
