"use server";
// Server entry — `chat` procedure that the AI SDK's `useChat` hook
// talks to. The file-level `"use server"` directive opts the file
// into the vite-plugin's RPC discovery; the wrapped `chat` export
// is exposed via the synthetic SSR entry's `default.rpc("chat",
// input, ctx)`.
//
// Wire (per AI SDK v6 spec):
//   POST /__zeroship/v1/chat
//     body:    { json: { messages: UIMessage[] } }      ← zeroship envelope
//     response: text/event-stream                       ← UI Message Stream
//                with header `x-vercel-ai-ui-message-stream: v1`
//                and SSE frames carrying { type, ... } JSON objects.
//
// `streamText({ model: openai("...") })` calls OpenAI and streams text
// deltas. `result.toUIMessageStreamResponse()` produces the canonical
// SSE wire `useChat` expects — no manual frame plumbing.

import { action } from "@zeroship/rpc/server";
import { createOpenAI } from "@ai-sdk/openai";
import { env } from "zeroship";
import { streamText, convertToModelMessages, type UIMessage } from "ai";

// The provider is built from the app's own environment, not from
// `process.env`: a creator points the app at any OpenAI-compatible endpoint
// by setting the `OPENAI_BASE_URL` app variable and the `OPENAI_API_KEY` app
// secret, and the runtime surfaces both on `env`. Locally the same two names
// arrive with a `ZS_VAR_` prefix (`.env`), which is how the test fixture
// supplies a stub without any real key.
function provider() {
  return createOpenAI({
    baseURL: env.OPENAI_BASE_URL as string | undefined,
    apiKey: env.OPENAI_API_KEY as string | undefined,
  });
}

// `action()`, not `mutation()`: mutations run inside a transaction and the
// runtime refuses an outbound `fetch` there, so the OpenAI call fails with
// `capability_violation` before a single token is produced. Actions may call
// external APIs, and a returned `Response` still streams to `useChat`.
export const chat = action(
  async (input: { messages: UIMessage[] }): Promise<Response> => {
    // `convertToModelMessages` turns the UI-shaped v6 messages
    // (`parts: [{ type: "text", text }]`) into the model-shaped form the
    // provider expects. It's async in v6, so the result must be awaited
    // before passing to streamText (otherwise streamText sees a Promise
    // and downstream `messages.some(...)` blows up).
    const result = streamText({
      model: provider().chat("gpt-5-nano"),
      system: "You are a friendly assistant.",
      messages: await convertToModelMessages(input.messages),
    });
    return result.toUIMessageStreamResponse();
  },
  { id: "chat" },
);
