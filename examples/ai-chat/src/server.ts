// Server entry — `chat` procedure that the AI SDK's `useChat` hook
// talks to. The vite-plugin discovers this file at `src/server.ts` and
// the synthetic SSR entry exposes the export via `default.rpc("chat",
// input, ctx)`.
//
// Wire (per AI SDK v5 spec):
//   POST /_zs/v1/chat
//     body:    { json: { messages: UIMessage[] } }      ← zeroship envelope
//     response: text/event-stream                       ← UI Message Stream
//                with header `x-vercel-ai-ui-message-stream: v1`
//                and SSE frames carrying { type, ... } JSON objects.
//
// `streamText({ model: openai("...") })` calls OpenAI and streams text
// deltas. `result.toUIMessageStreamResponse()` produces the canonical
// SSE wire `useChat` expects — no manual frame plumbing.
//
// Returning a `Response` (rather than yielding an async iterator) lets
// the kernel forward the SSE bytes verbatim through `inspect_response`.
// Async-iter returns get re-encoded as the older line-prefixed v3
// protocol that v5 `useChat` doesn't parse.

import { openai } from "@ai-sdk/openai";
import { streamText, convertToModelMessages, type UIMessage } from "ai";

export async function chat(input: { messages: UIMessage[] }): Promise<Response> {
  // The OpenAI provider reads `OPENAI_API_KEY` from `process.env` by
  // default. The runtime forwards the host process env into V8;
  // `pnpm dev` sources the local `.env` file via the vite-plugin's
  // dev-server. For `zeroship serve` directly, run with the var set
  // (e.g. `OPENAI_API_KEY=$(grep OPENAI_API_KEY .env | cut -d= -f2)
  // zeroship serve dist/server/index.js`).
  const result = streamText({
    model: openai("gpt-4o-mini"),
    system: "You are a friendly assistant.",
    // `convertToModelMessages` turns the UI-shaped v5 messages
    // (`parts: [{ type: "text", text }]`) into the model-shaped form
    // the provider expects.
    messages: convertToModelMessages(input.messages),
  });

  // Emits the v5 UI Message Stream Protocol with the right SSE frames
  // and `x-vercel-ai-ui-message-stream: v1` header. The kernel sees a
  // Response in `classify_rpc_return`, calls `inspect_response`, and
  // forwards the body as a streaming HTTP response.
  return result.toUIMessageStreamResponse();
}

// Marked as `mutation` — the procedure has side-effects (a model call)
// and returns a single Response, even though that response happens to
// stream. The manifest emitter literalizes `.config` via the AST, so
// no `as const`.
chat.config = { id: "chat", kind: "mutation" };
