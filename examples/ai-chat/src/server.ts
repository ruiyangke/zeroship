"use server";
// Server entry — `chat` procedure that the AI SDK's `useChat` hook
// talks to. The file-level `"use server"` directive opts the file
// into the vite-plugin's RPC discovery; the wrapped `chat` export
// is exposed via the synthetic SSR entry's `default.rpc("chat",
// input, ctx)`.
//
// Wire (per AI SDK v6 spec):
//   POST /_zs/v1/chat
//     body:    { json: { messages: UIMessage[] } }      ← zeroship envelope
//     response: text/event-stream                       ← UI Message Stream
//                with header `x-vercel-ai-ui-message-stream: v1`
//                and SSE frames carrying { type, ... } JSON objects.
//
// `streamText({ model: openai("...") })` calls OpenAI and streams text
// deltas. `result.toUIMessageStreamResponse()` produces the canonical
// SSE wire `useChat` expects — no manual frame plumbing.

import { mutation } from "@zeroship/rpc/server";
import { openai } from "@ai-sdk/openai";
import { streamText, convertToModelMessages, type UIMessage } from "ai";

// Marked as `mutation()` — the procedure has side-effects (a model
// call) and returns a single Response, even though that response
// happens to stream.
export const chat = mutation(
  async (input: { messages: UIMessage[] }): Promise<Response> => {
    // `convertToModelMessages` turns the UI-shaped v6 messages
    // (`parts: [{ type: "text", text }]`) into the model-shaped form the
    // provider expects. It's async in v6, so the result must be awaited
    // before passing to streamText (otherwise streamText sees a Promise
    // and downstream `messages.some(...)` blows up).
    const result = streamText({
      model: openai("gpt-5-nano"),
      system: "You are a friendly assistant.",
      messages: await convertToModelMessages(input.messages),
    });
    return result.toUIMessageStreamResponse();
  },
  { id: "chat" },
);
