// Server entry — `chat` procedure that the AI SDK's `useChat` hook
// talks to. The vite-plugin discovers this file at the path
// `src/server.ts` and the synthetic SSR entry exposes the export via
// `default.rpc("chat", input, ctx)`.
//
// Wire (per AI SDK v5 spec):
//   POST /_zs/v1/chat
//     body:    { json: { messages: UIMessage[] } }      ← zeroship envelope
//     response: text/event-stream                       ← UI Message Stream
//                with header `x-vercel-ai-ui-message-stream: v1`
//                and SSE frames carrying { type, ... } JSON objects.
//
// We return a `Response` directly (rather than yielding an async
// iterator) so the kernel forwards it verbatim — the runtime's built-in
// async-iter encoding is the older line-prefixed AI-SDK v3 format,
// which `useChat` v5 doesn't understand. Returning a Response lets us
// emit the canonical SSE frames the v5 parser expects.
//
// Streaming is hand-rolled here (no `streamText` from the `ai` package)
// so the demo is self-contained and works without any provider API
// keys. The `// REAL MODEL` block at the bottom shows how to swap in a
// genuine LLM call.

interface UIPart {
  type: string;
  text?: string;
}

interface UIMessage {
  id?: string;
  role: "system" | "user" | "assistant";
  parts?: UIPart[];
  content?: string;
}

/**
 * Pull the latest user prompt out of a UIMessage array. The AI SDK's
 * v5 message shape carries content under `parts: [{ type: "text",
 * text }]`; v4-and-earlier inlined a `content` string. We tolerate
 * both so the demo keeps working if the SDK rolls minor shapes.
 */
function lastUserText(messages: UIMessage[]): string {
  for (let i = messages.length - 1; i >= 0; i--) {
    const m = messages[i];
    if (m.role !== "user") continue;
    if (Array.isArray(m.parts)) {
      const txt = m.parts
        .filter((p): p is UIPart & { text: string } => p.type === "text" && typeof p.text === "string")
        .map((p) => p.text)
        .join("");
      if (txt) return txt;
    }
    if (typeof m.content === "string" && m.content) return m.content;
  }
  return "";
}

/** Cheap deterministic stream — pretends to be a model. Splits the
 *  reply into word-sized chunks and sends one every 30ms so the UI
 *  visibly streams. Replace with a real provider for production. */
function fakeReplyTokens(prompt: string): string[] {
  const reply =
    `You said: "${prompt}".\n\nThis is a streaming reply from a zeroship RPC procedure. ` +
    `Each word arrives as a separate \`text-delta\` SSE frame so the AI SDK's useChat ` +
    `hook can render it as it lands. ` +
    `Swap the mock generator for a real model (OpenAI, Anthropic, …) to make this useful.`;
  // Preserve whitespace so reassembly looks natural in the UI.
  return reply.match(/\S+\s*|\s+/g) ?? [reply];
}

export async function chat(input: { messages: UIMessage[] }): Promise<Response> {
  const prompt = lastUserText(input?.messages ?? []);
  const tokens = fakeReplyTokens(prompt);

  // The v5 protocol opens with `start`/`text-start`, emits one or more
  // `text-delta` frames, then closes with `text-end`/`finish`/`[DONE]`.
  // IDs are local to the message — we generate fresh ones per request.
  const messageId = `msg_${Math.random().toString(36).slice(2, 10)}`;
  const textBlockId = `txt_${Math.random().toString(36).slice(2, 10)}`;

  const enc = new TextEncoder();
  const stream = new ReadableStream<Uint8Array>({
    async start(controller) {
      const send = (obj: unknown) => {
        controller.enqueue(enc.encode(`data: ${JSON.stringify(obj)}\n\n`));
      };
      const sendDone = () => {
        controller.enqueue(enc.encode("data: [DONE]\n\n"));
      };

      try {
        send({ type: "start", messageId });
        send({ type: "text-start", id: textBlockId });
        for (const tok of tokens) {
          // Yield to the event loop so each delta lands in its own
          // network frame instead of getting coalesced.
          await new Promise((r) => setTimeout(r, 30));
          send({ type: "text-delta", id: textBlockId, delta: tok });
        }
        send({ type: "text-end", id: textBlockId });
        send({ type: "finish" });
        sendDone();
      } catch (err) {
        // The protocol's `error` frame surfaces as a thrown Error on
        // the client (useChat onError). Cheap and observable.
        send({
          type: "error",
          errorText: err instanceof Error ? err.message : String(err),
        });
        sendDone();
      } finally {
        controller.close();
      }
    },
  });

  return new Response(stream, {
    status: 200,
    headers: {
      "content-type": "text/event-stream",
      "cache-control": "no-cache, no-transform",
      // SDK v5 requires this header so the parser knows the SSE frames
      // are UI Message Stream events, not generic ones.
      "x-vercel-ai-ui-message-stream": "v1",
    },
  });
}

// Marked as `mutation` — the procedure has side-effects (talking to a
// model) and returns a single Response, even though that response
// happens to stream. `kind: "stream"` would tell the runtime to wrap
// async-iterator returns; we return a Response, not an iterator.
//
// (No `as const` here — the manifest emitter literalizes `.config` via
// the AST and TypeScript type assertions aren't part of that subset.)
chat.config = { id: "chat", kind: "mutation" };

// ── REAL MODEL drop-in ────────────────────────────────────────────────
//
// Replace `chat` above with this once you have a provider configured
// (set the API key via `zeroship secret set OPENAI_API_KEY=...` so it
// lands in `env.OPENAI_API_KEY`). Uses the `ai` package's `streamText`
// + `toUIMessageStreamResponse()` which already emits the v5 protocol
// — no manual SSE plumbing.
//
//   import { streamText, convertToModelMessages } from "ai";
//   import { createOpenAI } from "@ai-sdk/openai";
//   import { env } from "zeroship";
//
//   export async function chat(input: { messages: UIMessage[] }) {
//     const openai = createOpenAI({ apiKey: env.OPENAI_API_KEY });
//     const result = streamText({
//       model: openai("gpt-4o-mini"),
//       messages: convertToModelMessages(input.messages),
//     });
//     return result.toUIMessageStreamResponse();
//   }
//   chat.config = { id: "chat", kind: "mutation" } as const;
