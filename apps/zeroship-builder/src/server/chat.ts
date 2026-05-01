// Server entry — `chat` procedure that the AI SDK's `useChat` hook
// talks to. The vite-plugin discovers this file and the synthetic SSR
// entry exposes the export via `default.rpc("chat", input, ctx)`.
//
// Wire (per AI SDK v6 spec):
//   POST /_zs/v1/chat
//     body:    { json: { messages: UIMessage[] } }      ← zeroship envelope
//     response: text/event-stream                       ← UI Message Stream
//                with header `x-vercel-ai-ui-message-stream: v1`
//                and SSE frames carrying { type, ... } JSON objects.
//
// Plan 01.5 ships a TEXT-ONLY mock — we hand-construct a UI Message
// Stream emitting a single text-start / text-delta* / text-end
// sequence. Plan 02 replaces this body with the real Builder agent
// (deepagents → translator) and adds custom data parts (survey, diff,
// critic-round) on the same wire.

import {
  createUIMessageStream,
  createUIMessageStreamResponse,
  type UIMessage,
} from "ai";

export async function chat(input: { messages: UIMessage[] }): Promise<Response> {
  void input; // Plan 01.5 mock ignores the prompt — Plan 02 will read it.

  const stream = createUIMessageStream({
    async execute({ writer }) {
      const id = crypto.randomUUID();
      writer.write({ type: "text-start", id });

      const reply =
        "Got it — Plan 01.5 mock here. Plan 02 will wire the real Builder agent.";
      for (const ch of reply) {
        writer.write({ type: "text-delta", id, delta: ch });
        await new Promise((r) => setTimeout(r, 18));
      }

      writer.write({ type: "text-end", id });
    },
  });

  return createUIMessageStreamResponse({ stream });
}

// Marked as `mutation` — the procedure has side-effects (a model call
// in Plan 02) and returns a single Response that happens to stream.
// The manifest emitter literalizes `.config` via the AST, so no
// `as const`.
chat.config = { id: "chat", kind: "mutation" };
