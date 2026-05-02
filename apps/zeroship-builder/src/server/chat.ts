"use server";
// Builder chat — routed through deepagents (server-side agent runtime,
// design §4.8 Foundation Decision #8) and translated to AI SDK v6 UI
// Message Stream Protocol on the wire. The kernel forwards the Response's
// SSE bytes verbatim to the v6 `useChat` client.
//
// Wire (per AI SDK v6 spec, identical to examples/ai-chat):
//   POST /_zs/v1/chat
//     fresh body:   { json: { messages: UIMessage[], id: string } }
//     resume body:  { json: { resume: { token, value }, id: string } }
//     response:     text/event-stream                  ← UI Message Stream
//                with header `x-vercel-ai-ui-message-stream: v1`
//                and SSE frames carrying { type, ... } JSON objects.
//
// The deepagents/LangGraph dep tree is heavy (~MB of code paths). The
// translator import is deferred to keep non-chat server functions in
// the same module bundle from paying that weight (per design §4.8.5).

import { createUIMessageStreamResponse, type UIMessage } from "ai";

// G2 (Plan 02 Phase B.0): the chat handler runs through the RPC fast
// path, which does NOT construct a Request — so we can't read
// `request.signal`. Instead we mint our own AbortController and abort
// it when the response body's ReadableStream is cancelled (which the
// V8 runtime does when the SSE consumer disconnects). The signal is
// threaded into `streamEvents`, where LangChain forwards it to the
// underlying OpenAI HTTP call.
//
// We wrap the body stream so we observe `cancel()`. If a future
// runtime change exposes `request.signal` on the RPC fast path, this
// can be simplified to just plumb that signal through — no body
// wrapping needed.
export async function chat(
  input: {
    messages?: UIMessage[];
    id?: string;
    // G3: resume payload from a tool that halted via `interruptOn`.
    // When present, the translator skips message replay and feeds the
    // value into a `Command({resume})` against the same thread.
    resume?: { token: string; value: unknown };
    // Project id — threaded through to the data-part middleware so
    // Critic-graded scorecards persist to the right KV slot
    // (ISS-16 fix path).
    appId?: string;
  },
): Promise<Response> {
  const { buildTranslatedStream } = await import("./_translator.js");

  const ac = new AbortController();
  const stream = await buildTranslatedStream(input, ac.signal);

  // Wrap the SSE Response's body to abort the in-flight LLM call when
  // the client disconnects. The default `createUIMessageStreamResponse`
  // body is a ReadableStream; we passthrough chunks but intercept
  // `cancel()` to fire the AbortController.
  const baseResponse = createUIMessageStreamResponse({ stream });
  if (!baseResponse.body) return baseResponse;

  const wrapped = new ReadableStream({
    async start(controller) {
      const reader = baseResponse.body!.getReader();
      try {
        while (true) {
          const { done, value } = await reader.read();
          if (done) break;
          controller.enqueue(value);
        }
        controller.close();
      } catch (err) {
        controller.error(err);
      }
    },
    cancel(reason) {
      // Client disconnected mid-stream — abort the OpenAI request.
      ac.abort(reason);
    },
  });

  return new Response(wrapped, {
    status: baseResponse.status,
    statusText: baseResponse.statusText,
    headers: baseResponse.headers,
  });
}

// Marked as `mutation` — the procedure has side-effects (a model call)
// and returns a single Response, even though that response happens to
// stream. The manifest emitter literalizes `.config` via the AST, so
// no `as const`.
chat.config = { id: "chat", kind: "mutation" };
