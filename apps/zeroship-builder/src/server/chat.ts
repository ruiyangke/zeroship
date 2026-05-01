"use server";
// Builder chat — routed through deepagents (server-side agent runtime,
// design §4.8 Foundation Decision #8) and translated to AI SDK v6 UI
// Message Stream Protocol on the wire. The kernel forwards the Response's
// SSE bytes verbatim to the v6 `useChat` client.
//
// Wire (per AI SDK v6 spec, identical to examples/ai-chat):
//   POST /_zs/v1/chat
//     body:    { json: { messages: UIMessage[] } }      ← zeroship envelope
//     response: text/event-stream                       ← UI Message Stream
//                with header `x-vercel-ai-ui-message-stream: v1`
//                and SSE frames carrying { type, ... } JSON objects.
//
// The deepagents/LangGraph dep tree is heavy (~MB of code paths). The
// translator import is deferred to keep non-chat server functions in
// the same module bundle from paying that weight (per design §4.8.5).

import { createUIMessageStreamResponse, type UIMessage } from "ai";

export async function chat(input: { messages: UIMessage[] }): Promise<Response> {
  const { buildTranslatedStream } = await import("./_translator.js");
  const stream = await buildTranslatedStream(input);
  return createUIMessageStreamResponse({ stream });
}

// Marked as `mutation` — the procedure has side-effects (a model call)
// and returns a single Response, even though that response happens to
// stream. The manifest emitter literalizes `.config` via the AST, so
// no `as const`.
chat.config = { id: "chat", kind: "mutation" };
