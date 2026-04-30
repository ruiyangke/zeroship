/**
 * Phase 4 — client stream() async-iterator API.
 *
 * Surface:
 *
 *   const stream = rpc.todos.search.stream({ query: "..." }, { signal });
 *   for await (const chunk of stream) {
 *     console.log(chunk);   // typed: T per output schema
 *   }
 *
 * Wire:
 *   POST /_zs/v1/<id> with Accept: text/event-stream + JSON-encoded body.
 *   Response body is the AI-SDK Data Stream Protocol — line-prefixed:
 *
 *     0:"text"\n          string yield
 *     2:[<json>]\n        object yield
 *     3:"err msg"\n       error message string
 *     e:{...}\n           structured error envelope (zeroship ext)
 *     d:{}\n              done
 *
 * Also: `rpc.todos.search.streamUrl({ query })` returns the URL with
 * the input as `?input=<base64url>` so consumers can hand it to
 * ai-sdk's `useChat` directly.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";

import { client } from "../src/client.js";
import { RpcError } from "../src/error.js";

// ── Helpers ────────────────────────────────────────────────────────────

interface StreamRecorded {
  url: string;
  method: string;
  headers: Record<string, string>;
  body: string | null;
  signal?: AbortSignal;
}

/**
 * Build a Response whose body is a ReadableStream the test can drive
 * frame-by-frame. Returns the response + a `push(line)` and `done()`
 * controller. Each call to `push("0:\"x\"")` enqueues `"0:\"x\"\n"`.
 */
function makeStreamResponse(): {
  response: Response;
  push: (line: string) => void;
  pushBytes: (bytes: Uint8Array) => void;
  finish: () => void;
  abort: (reason?: unknown) => void;
} {
  const encoder = new TextEncoder();
  let pushFn: (line: string) => void = () => {};
  let pushBytesFn: (bytes: Uint8Array) => void = () => {};
  let finishFn: () => void = () => {};
  let abortFn: (reason?: unknown) => void = () => {};

  const body = new ReadableStream<Uint8Array>({
    start(ctrl) {
      pushFn = (line) => ctrl.enqueue(encoder.encode(line + "\n"));
      pushBytesFn = (bytes) => ctrl.enqueue(bytes);
      finishFn = () => ctrl.close();
      abortFn = (reason) => ctrl.error(reason ?? new Error("aborted"));
    },
  });
  const response = new Response(body, {
    status: 200,
    headers: { "content-type": "text/event-stream" },
  });
  return {
    response,
    push: (l) => pushFn(l),
    pushBytes: (b) => pushBytesFn(b),
    finish: () => finishFn(),
    abort: (r) => abortFn(r),
  };
}

function recordingFetch(impl: (req: StreamRecorded) => Promise<Response>): {
  fetch: (input: RequestInfo | URL, init?: RequestInit) => Promise<Response>;
  calls: StreamRecorded[];
} {
  const calls: StreamRecorded[] = [];
  return {
    calls,
    fetch: async (input, init) => {
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
      const recorded: StreamRecorded = {
        url,
        method: init?.method ?? "GET",
        headers,
        body: typeof init?.body === "string" ? init.body : null,
        signal: init?.signal ?? undefined,
      };
      calls.push(recorded);
      return impl(recorded);
    },
  };
}

async function collect<T>(iter: AsyncIterable<T>): Promise<T[]> {
  const out: T[] = [];
  for await (const x of iter) out.push(x);
  return out;
}

// ── stream() — basic shapes ────────────────────────────────────────────

describe("client.stream() — happy path", () => {
  test("yields object chunks parsed from `2:` lines", async () => {
    const stream = makeStreamResponse();
    const spy = recordingFetch(async () => stream.response);
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetch,
    });

    // Drive the stream from a parallel task so the iter has frames to
    // consume. Producer pushes 3 chunks then closes; consumer collects.
    const consumePromise = collect(
      rpc.call<unknown>("search.todos", { q: "build" }, { kind: "stream" } as never) as never,
    );
    // Yield to allow request setup to complete, then push frames.
    await Promise.resolve();
    stream.push('2:[{"id":1,"text":"first"}]');
    stream.push('2:[{"id":2,"text":"second"}]');
    stream.push("d:{}");
    stream.finish();

    const out = await consumePromise;
    assert.deepEqual(out, [
      { id: 1, text: "first" },
      { id: 2, text: "second" },
    ]);

    // Verify request shape.
    const c = spy.calls[0];
    assert.equal(c.method, "POST");
    assert.equal(c.url, "https://api.test/_zs/v1/search.todos");
    assert.equal(c.headers["accept"], "text/event-stream");
    assert.equal(c.headers["content-type"], "application/json");
    // Body is superjson-wrapped input.
    const parsed = JSON.parse(c.body!);
    assert.deepEqual(parsed.json, { q: "build" });
  });

  test("yields string chunks parsed from `0:` lines", async () => {
    const stream = makeStreamResponse();
    const spy = recordingFetch(async () => stream.response);
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetch,
    });

    const consumePromise = collect(
      rpc.call<unknown>("greet", undefined, { kind: "stream" } as never) as never,
    );
    await Promise.resolve();
    stream.push('0:"Hi"');
    stream.push('0:" there"');
    stream.push("d:{}");
    stream.finish();
    const out = (await consumePromise) as string[];
    assert.deepEqual(out, ["Hi", " there"]);
  });

  test("frames split across chunks reassemble correctly", async () => {
    // The transport splits TCP-level chunks however the server flushes
    // them. The line parser must buffer partial lines until a \\n
    // arrives.
    const stream = makeStreamResponse();
    const spy = recordingFetch(async () => stream.response);
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetch,
    });

    const consumePromise = collect(
      rpc.call<unknown>("split", undefined, { kind: "stream" } as never) as never,
    );
    await Promise.resolve();
    const enc = new TextEncoder();
    // First chunk: a partial line.
    stream.pushBytes(enc.encode('2:[{"id":'));
    // Second chunk: rest of the line + start of the next one.
    stream.pushBytes(enc.encode("1}]\n2:[{"));
    stream.pushBytes(enc.encode('"id":2}]\nd:{}\n'));
    stream.finish();
    const out = await consumePromise;
    assert.deepEqual(out, [{ id: 1 }, { id: 2 }]);
  });
});

// ── stream() — error paths ─────────────────────────────────────────────

describe("client.stream() — errors", () => {
  test("e: envelope mid-stream throws an RpcError with the right code", async () => {
    const stream = makeStreamResponse();
    const spy = recordingFetch(async () => stream.response);
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetch,
    });

    const consumePromise = (async () => {
      const out: unknown[] = [];
      try {
        for await (const chunk of rpc.call<unknown>(
          "x",
          undefined,
          { kind: "stream" } as never,
        ) as never) {
          out.push(chunk);
        }
      } catch (e) {
        return { thrown: e, collected: out };
      }
      return { thrown: null, collected: out };
    })();
    await Promise.resolve();
    stream.push('2:[{"id":1}]');
    stream.push(
      'e:{"message":"upstream gone","code":"UNAVAILABLE","details":{"hint":"demo"},"retryable":true}',
    );
    stream.push("d:{}");
    stream.finish();
    const { thrown, collected } = await consumePromise;
    // First yield made it through; the error stops iteration.
    assert.deepEqual(collected, [{ id: 1 }]);
    assert.ok(thrown instanceof RpcError, `expected RpcError, got: ${String(thrown)}`);
    const err = thrown as RpcError;
    assert.equal(err.code, "UNAVAILABLE");
    assert.equal(err.message, "upstream gone");
    assert.equal(err.retryable, true);
    assert.deepEqual(err.details, { hint: "demo" });
  });

  test("3: error message line throws an RpcError with INTERNAL code", async () => {
    // The bare `3:` typeId carries a string error message (per ai-sdk).
    // Without a structured envelope we default to INTERNAL.
    const stream = makeStreamResponse();
    const spy = recordingFetch(async () => stream.response);
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetch,
    });

    const p = (async () => {
      try {
        for await (const _ of rpc.call<unknown>(
          "x",
          undefined,
          { kind: "stream" } as never,
        ) as never) {
          // empty
        }
        return null;
      } catch (e) {
        return e;
      }
    })();
    await Promise.resolve();
    stream.push('3:"plain text error"');
    stream.push("d:{}");
    stream.finish();
    const thrown = (await p) as RpcError;
    assert.ok(thrown instanceof RpcError);
    assert.equal(thrown.code, "INTERNAL");
    assert.equal(thrown.message, "plain text error");
  });

  test("HTTP error status (4xx/5xx) before streaming throws RpcError", async () => {
    const errBody = JSON.stringify({
      code: "INVALID_ARGUMENT",
      message: "bad input",
      retryable: false,
    });
    const spy = recordingFetch(
      async () =>
        new Response(errBody, {
          status: 400,
          headers: { "content-type": "application/json" },
        }),
    );
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetch,
    });

    await assert.rejects(
      collect(
        rpc.call<unknown>("x", { bad: 1 }, { kind: "stream" } as never) as never,
      ),
      (e: unknown) => {
        const err = e as RpcError;
        assert.ok(err instanceof RpcError);
        assert.equal(err.code, "INVALID_ARGUMENT");
        assert.equal(err.message, "bad input");
        return true;
      },
    );
  });

  test("AbortSignal cancels mid-stream, iter rejects with CANCELLED", async () => {
    const stream = makeStreamResponse();
    const spy = recordingFetch(async () => stream.response);
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: spy.fetch,
    });

    const ctrl = new AbortController();
    const p = (async () => {
      const out: unknown[] = [];
      try {
        for await (const chunk of rpc.call<unknown>(
          "x",
          undefined,
          { kind: "stream", signal: ctrl.signal } as never,
        ) as never) {
          out.push(chunk);
          if (out.length === 1) ctrl.abort();
        }
        return { thrown: null, out };
      } catch (e) {
        return { thrown: e, out };
      }
    })();
    await Promise.resolve();
    stream.push('2:[{"id":1}]');
    // Don't call finish() — the consumer aborts after the first yield.
    // After abort, the stream's `controller.error` is fired by the
    // mock to mimic fetch's AbortError behavior.
    await new Promise((r) => setTimeout(r, 5));
    stream.abort(Object.assign(new Error("aborted"), { name: "AbortError" }));
    const { thrown, out } = await p;
    assert.deepEqual(out, [{ id: 1 }]);
    assert.ok(thrown instanceof RpcError, `expected RpcError, got ${String(thrown)}`);
    assert.equal((thrown as RpcError).code, "CANCELLED");
  });
});

// ── streamUrl() — for ai-sdk hand-off ─────────────────────────────────

describe("client.streamUrl()", () => {
  test("returns a URL with base64url-encoded input (async via superjson)", async () => {
    // streamUrl returns a Promise<string> when input requires async
    // serialization (the default when transformer is "superjson").
    // For "json" transformer (synchronous), it can return a plain
    // string — but the public API stays Promise<string> | string for
    // forward-compat.
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: async () => new Response(),
    });
    const handle = (
      rpc as unknown as {
        chat: { completion: { streamUrl(input: unknown): string | Promise<string> } };
      }
    ).chat.completion;
    const res = await handle.streamUrl({
      messages: [{ role: "user", content: "hi" }],
    });
    assert.ok(
      res.startsWith("https://api.test/_zs/v1/chat.completion?input="),
      `expected /_zs/v1/chat.completion?input=..., got: ${res}`,
    );
    const u = new URL(res);
    const enc = u.searchParams.get("input");
    assert.ok(enc, "input param present");
    const b64 = enc!.replace(/-/g, "+").replace(/_/g, "/");
    const padded = b64 + "=".repeat((4 - (b64.length % 4)) % 4);
    const decoded = JSON.parse(Buffer.from(padded, "base64").toString("utf8"));
    const inner = decoded.json ?? decoded;
    assert.deepEqual(inner, { messages: [{ role: "user", content: "hi" }] });
  });

  test("streamUrl with no input omits the query parameter", async () => {
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: async () => new Response(),
    });
    const res = (
      (rpc as unknown as { ping: { streamUrl(input?: unknown): string } }).ping.streamUrl
    )();
    assert.equal(res, "https://api.test/_zs/v1/ping");
  });
});
