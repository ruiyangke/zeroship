// AI Streaming — async-generator RPC exports framed as SSE.
//
// `export default { rpc: { ... }, fetch }` works under raw `zeroship serve`
// without a Vite build step. The `fetchHandler` function below encodes
// async-generator results as SSE streams (Vercel AI-SDK line protocol).
//
// NOTE: The `"use server"` directive is a Vite build-time transform
// (`@zeroship/vite-plugin`) that auto-wires named async-generator exports
// as SSE streams via `POST /__zeroship/v1/<export>`. It does NOT run in
// raw `zeroship serve` — raw serve requires the explicit
// `export default { rpc: { ... }, fetch }` shape. See ISS-55.
//
// Hit endpoints with:
//   curl -N -X POST http://localhost:3000/__zeroship/v1/echoStream \
//        -H 'content-type: application/json' \
//        -d '{"json":"hello world foo"}'
//   # streams: 2:[{"token":"hello"}]\n 2:[{"token":"world"}]\n ...
//
//   curl -N -X POST http://localhost:3000/__zeroship/v1/chat \
//        -H 'content-type: application/json' \
//        -d '{"json":{"prompt":"Say hi","model":"gpt-3.5-turbo"}}'

import { env } from "zeroship";

// ── Vercel AI-SDK Data Stream Protocol encoder ────────────────────────────
// Each line is `<typeId>:<json>\n`. TypeIds emitted here:
//   0:"text"    — plain string yield
//   2:[<json>]  — typed object yield
//   e:{...}    — error envelope
//   d:{}       — done

function encodeStreamLine(value) {
  if (typeof value === "string") {
    return `0:${JSON.stringify(value)}\n`;
  }
  return `2:${JSON.stringify([value])}\n`;
}

// ── RPC handlers ──────────────────────────────────────────────────────────

/**
 * Local echo stream — no API key needed. Yields each token of `input`
 * split on whitespace. Good for smoke-testing the SSE pipe.
 *
 * Input: a plain string, or `{ prompt }` object.
 */
async function* echoStream(input) {
  const prompt = (typeof input === "string") ? input : (input?.prompt ?? "hello world");
  for (const tok of String(prompt).split(/\s+/)) {
    yield { token: tok };
  }
}
// kind: "stream" lets the kernel's dict-shape dispatcher return the
// AsyncIterator directly (not wrapped in a Promise) when invoked via
// the Rust RPC fast path. Required for correct streaming under raw serve.
echoStream.config = { kind: "stream" };

/**
 * Proxy to an OpenAI-compatible chat completions endpoint.
 * Requires `env.OPENAI_API_KEY` to be set via `zeroship secret set`.
 *
 * Input: `{ prompt, model?, apiUrl? }` or a plain string prompt.
 * Yields one `{ content }` object per delta; resolves when the upstream
 * sends `data: [DONE]`.
 *
 * Uses globalThis.fetch (the WinterCG global) to avoid shadowing.
 */
async function* chat(input) {
  const opts = (typeof input === "object" && input !== null) ? input : { prompt: String(input ?? "") };
  const prompt = opts.prompt;
  const apiKey = env.OPENAI_API_KEY;
  if (!apiKey) throw new Error("OPENAI_API_KEY not configured");

  const apiUrl = opts.apiUrl ?? "https://api.openai.com/v1/chat/completions";
  const model = opts.model ?? "gpt-3.5-turbo";

  const resp = await globalThis.fetch(apiUrl, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      authorization: `Bearer ${apiKey}`,
    },
    body: JSON.stringify({
      model,
      stream: true,
      messages: [{ role: "user", content: prompt }],
    }),
  });
  if (!resp.ok) throw new Error(`OpenAI ${resp.status}`);

  const reader = resp.body.getReader();
  const decoder = new TextDecoder();
  let buf = "";
  while (true) {
    const { value, done } = await reader.read();
    if (done) return;
    buf += decoder.decode(value, { stream: true });
    // SSE frames are separated by \n\n.
    for (;;) {
      const idx = buf.indexOf("\n\n");
      if (idx < 0) break;
      const frame = buf.slice(0, idx);
      buf = buf.slice(idx + 2);
      for (const line of frame.split("\n")) {
        if (!line.startsWith("data: ")) continue;
        const payload = line.slice(6).trim();
        if (payload === "[DONE]") return;
        try {
          const parsed = JSON.parse(payload);
          const content = parsed.choices?.[0]?.delta?.content;
          if (content) yield { content };
        } catch {
          // Ignore malformed frames — upstream may send keepalives.
        }
      }
    }
  }
}
chat.config = { kind: "stream" };

// ── Dict-shape RPC map ────────────────────────────────────────────────────
const rpc = { echoStream, chat };

// ── Inline SSE fetch handler ──────────────────────────────────────────────
// In a Vite-built app the synthetic SSR entry's createFetchHandler handles
// the `/__zeroship/v1/<id>` → SSE encoding path automatically. For raw
// `zeroship serve` (no build step) we inline a minimal equivalent here.
async function fetchHandler(request) {
  const url = new URL(request.url);
  if (!url.pathname.startsWith("/__zeroship/v1/")) {
    return new Response("Not Found", { status: 404 });
  }
  if (request.method !== "POST") {
    return new Response("Method Not Allowed", { status: 405 });
  }

  const name = url.pathname.slice("/__zeroship/v1/".length);
  const rpcFn = rpc[name];
  if (typeof rpcFn !== "function") {
    return Response.json(
      { message: `Method not found: ${name}`, code: "NOT_FOUND" },
      { status: 404 },
    );
  }

  // Decode wire input: `{ "json": <value> }` envelope or raw JSON.
  let input;
  try {
    const text = await request.text();
    if (text) {
      const parsed = JSON.parse(text);
      input = (parsed && typeof parsed === "object" && "json" in parsed)
        ? parsed.json
        : parsed;
    }
  } catch {
    return Response.json(
      { message: "Invalid JSON body", code: "INVALID_ARGUMENT" },
      { status: 400 },
    );
  }

  // Call the handler. Async generators return an AsyncIterator synchronously.
  let result;
  try {
    result = rpcFn(input);
  } catch (e) {
    return Response.json(
      { message: e.message ?? "Internal error", code: e.code ?? "INTERNAL" },
      { status: e.status ?? 500 },
    );
  }

  // Non-generator (unary): resolve and JSON-encode the result.
  if (!result || typeof result[Symbol.asyncIterator] !== "function") {
    try {
      const resolved = await result;
      return new Response(JSON.stringify({ json: resolved }), {
        headers: { "content-type": "application/json" },
      });
    } catch (e) {
      return Response.json(
        { message: e.message ?? "Internal error", code: e.code ?? "INTERNAL" },
        { status: e.status ?? 500 },
      );
    }
  }

  // Async-generator: stream as Vercel AI-SDK Data Stream Protocol.
  const encoder = new TextEncoder();
  const body = new ReadableStream({
    async start(controller) {
      try {
        for await (const chunk of result) {
          controller.enqueue(encoder.encode(encodeStreamLine(chunk)));
        }
        controller.enqueue(encoder.encode("d:{}\n"));
      } catch (e) {
        const errLine = `e:${JSON.stringify({
          message: e.message ?? "Stream error",
          code: e.code ?? "INTERNAL",
        })}\n`;
        controller.enqueue(encoder.encode(errLine));
      } finally {
        controller.close();
      }
    },
  });

  return new Response(body, {
    headers: {
      "content-type": "text/event-stream",
      "cache-control": "no-cache",
    },
  });
}

// Dict-shape RPC + fetch handler: works under raw `zeroship serve`.
// The `fetch` export handles the SSE encoding for streaming results.
// In a Vite-built app the `"use server"` transform + synthetic SSR entry
// handle this path automatically.
export default { rpc, fetch: fetchHandler };
