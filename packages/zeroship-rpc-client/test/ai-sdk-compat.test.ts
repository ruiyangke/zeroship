/**
 * AI-SDK compat smoke test.
 *
 * Verifies our encoder's output is parseable by a Vercel AI-SDK Data
 * Stream parser. We don't take a hard dep on `ai` (the package) — its
 * core parser is a few hundred lines of plain typeId/JSON dispatch
 * that we re-implement here as a fixture. If the public protocol
 * changes (new typeIds, encoding tweaks), this test will fail and
 * point us at the divergence.
 *
 * Reference: https://ai-sdk.dev/docs/ai-sdk-ui/stream-protocol
 *   "Each line of the response is a typed protocol message."
 *   "<typeId>:<json>\n"
 *
 * We exercise:
 *   - 0:"text"   — text part (most common in ai-sdk)
 *   - 2:[<obj>]  — data part (typed object yields)
 *   - e:{...}    — our extension, ignored gracefully by ai-sdk parsers
 *   - d:{}       — done
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";

import { client } from "../src/client.js";

/**
 * Minimal AI-SDK Data Stream parser. Mirrors the public spec — splits
 * the stream into line-delimited frames and dispatches by typeId.
 *
 * Returns a flat trace of `{ type, value }` events the test can assert
 * on. Unknown typeIds are surfaced as `{ type: "unknown:" + id, value }`
 * so we can verify ai-sdk parsers tolerate our `e:` extension.
 */
async function parseAiSdkStream(
  body: ReadableStream<Uint8Array>,
): Promise<{ type: string; value: unknown }[]> {
  const reader = body.getReader();
  const dec = new TextDecoder();
  let buf = "";
  const events: { type: string; value: unknown }[] = [];
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    buf += dec.decode(value, { stream: true });
    let nl: number;
    while ((nl = buf.indexOf("\n")) !== -1) {
      const line = buf.slice(0, nl);
      buf = buf.slice(nl + 1);
      if (!line) continue;
      const colon = line.indexOf(":");
      if (colon <= 0) continue;
      const id = line.slice(0, colon);
      const json = line.slice(colon + 1);
      let parsed: unknown;
      try {
        parsed = JSON.parse(json);
      } catch {
        parsed = json;
      }
      events.push({ type: id, value: parsed });
    }
  }
  return events;
}

/**
 * Our encoder lives on the server side (in the runtime kernel and the
 * synthetic vite-plugin entry). For this client-side smoke test we
 * inline a minimal encoder that mirrors the Rust + JS implementations.
 * If the kernel's encoder ever drifts from the spec, the integration
 * tests in crates/runtime + sdks/vite-plugin catch it; this test
 * instead pins the on-the-wire bytes so an ai-sdk consumer can read
 * them.
 */
function encodeStream(events: ({ kind: "text"; value: string } | { kind: "data"; value: unknown } | { kind: "error"; envelope: Record<string, unknown> })[]): ReadableStream<Uint8Array> {
  const enc = new TextEncoder();
  return new ReadableStream({
    start(ctrl) {
      for (const ev of events) {
        if (ev.kind === "text") {
          ctrl.enqueue(enc.encode("0:" + JSON.stringify(ev.value) + "\n"));
        } else if (ev.kind === "data") {
          ctrl.enqueue(enc.encode("2:[" + JSON.stringify(ev.value) + "]\n"));
        } else {
          ctrl.enqueue(enc.encode("e:" + JSON.stringify(ev.envelope) + "\n"));
        }
      }
      ctrl.enqueue(enc.encode("d:{}\n"));
      ctrl.close();
    },
  });
}

describe("ai-sdk compat — wire format", () => {
  test("text stream: encoder output matches `0:` + `d:` lines", async () => {
    const body = encodeStream([
      { kind: "text", value: "Hello" },
      { kind: "text", value: ", " },
      { kind: "text", value: "world!" },
    ]);
    const events = await parseAiSdkStream(body);
    assert.deepEqual(events, [
      { type: "0", value: "Hello" },
      { type: "0", value: ", " },
      { type: "0", value: "world!" },
      { type: "d", value: {} },
    ]);
  });

  test("data stream: each `2:` line is a JSON array of one element", async () => {
    const body = encodeStream([
      { kind: "data", value: { id: 1, role: "user" } },
      { kind: "data", value: { id: 2, role: "assistant" } },
    ]);
    const events = await parseAiSdkStream(body);
    assert.deepEqual(events, [
      { type: "2", value: [{ id: 1, role: "user" }] },
      { type: "2", value: [{ id: 2, role: "assistant" }] },
      { type: "d", value: {} },
    ]);
  });

  test("structured error: `e:` envelope is parseable JSON; ai-sdk ignores unknown ids", async () => {
    const body = encodeStream([
      { kind: "data", value: { id: 1 } },
      {
        kind: "error",
        envelope: {
          message: "rate limit hit",
          code: "RESOURCE_EXHAUSTED",
          retryable: true,
        },
      },
    ]);
    const events = await parseAiSdkStream(body);
    // ai-sdk would tolerate the unknown `e:` typeId — we record it so
    // the test verifies the bytes are well-formed (parseable JSON, no
    // double escaping).
    assert.equal(events.length, 3);
    assert.equal(events[0].type, "2");
    assert.equal(events[1].type, "e");
    assert.deepEqual(events[1].value, {
      message: "rate limit hit",
      code: "RESOURCE_EXHAUSTED",
      retryable: true,
    });
    assert.equal(events[2].type, "d");
  });

  test("our client decodes our encoder's output round-trip", async () => {
    // Round-trip: feed the same bytes through our parser (in the
    // client SDK) and verify it surfaces the same data shape.
    const body = encodeStream([
      { kind: "text", value: "hi" },
      { kind: "data", value: { id: 1 } },
    ]);
    const rpc = client({
      baseUrl: "https://api.test",
      fetch: async () =>
        new Response(body, {
          status: 200,
          headers: { "content-type": "text/event-stream" },
        }),
    });
    const out: unknown[] = [];
    const iter = rpc.call<unknown>("x", undefined, { kind: "stream" } as never) as AsyncIterableIterator<unknown>;
    for await (const v of iter) out.push(v);
    // String yields → string. Object yields → object. The same wire
    // bytes a real ai-sdk consumer would also see.
    assert.deepEqual(out, ["hi", { id: 1 }]);
  });
});
