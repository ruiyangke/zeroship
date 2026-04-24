// AI Streaming — async-generator RPC exports that the bootstrap
// auto-wraps as SSE. Clients receive an `AsyncIterable<T>` via the
// generated stub (see `sdks/vite-plugin`'s `__rpcStream`).
//
// `"use server"` makes every export a platform-dispatched RPC method.
// No manual ReadableStream plumbing — the bootstrap handles framing,
// the runtime handles backpressure.

"use server";

import { env } from "zeroship";

/**
 * Local echo stream — no API key needed. Yields each token of `prompt`
 * separated by a newline. Good for smoke-testing the SSE pipe.
 */
export async function* echoStream(prompt = "hello world") {
  for (const tok of String(prompt).split(/\s+/)) {
    yield { token: tok };
  }
}

/**
 * Proxy to an OpenAI-compatible chat completions endpoint.
 * Requires `env.OPENAI_API_KEY` to be set via `zeroship secret set`.
 *
 * Yields one `{ content }` object per delta; resolves when the upstream
 * sends `data: [DONE]`.
 */
export async function* chat(prompt, opts = {}) {
  const apiKey = env.OPENAI_API_KEY;
  if (!apiKey) throw new Error("OPENAI_API_KEY not configured");

  const apiUrl = opts.apiUrl ?? "https://api.openai.com/v1/chat/completions";
  const model = opts.model ?? "gpt-3.5-turbo";

  const resp = await fetch(apiUrl, {
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
