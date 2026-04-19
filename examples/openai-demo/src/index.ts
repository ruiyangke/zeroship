"use server";

import OpenAI from "openai";

const client = new OpenAI({ apiKey: process.env.OPENAI_API_KEY });

interface ChatMsg {
  role: "user" | "assistant" | "system";
  content: string;
}

/**
 * Streaming chat endpoint.
 *
 * Uses the official `openai` SDK (^6) to request a streaming completion from
 * gpt-5.4-mini, then adapts the SDK's async iterator into an
 * `SSE Response(ReadableStream)` that the zeroship runtime serves to the
 * browser as `text/event-stream` with chunked transfer encoding.
 *
 * The wire protocol is minimal:
 *   data: {"token": "Hi"}\n\n
 *   data: {"token": " there"}\n\n
 *   …
 *   data: [DONE]\n\n
 *
 * The browser side reads the stream via `response.body.getReader()` and
 * appends each token to the assistant message as it arrives.
 */
export async function chat(message: string, history: ChatMsg[] = []) {
  const safeHistory = Array.isArray(history) ? history : [];
  const messages: ChatMsg[] = [
    ...safeHistory.filter((m): m is ChatMsg => !!m && typeof m.content === "string"),
    { role: "user", content: message },
  ];

  const stream = await client.chat.completions.create({
    model: "gpt-5.4-mini",
    stream: true,
    messages,
  });

  const encoder = new TextEncoder();
  const body = new ReadableStream({
    async start(controller) {
      try {
        for await (const part of stream) {
          const delta = part.choices?.[0]?.delta?.content;
          if (delta) {
            controller.enqueue(
              encoder.encode(`data: ${JSON.stringify({ token: delta })}\n\n`)
            );
          }
        }
        controller.enqueue(encoder.encode("data: [DONE]\n\n"));
      } catch (e: any) {
        // Surface the upstream error as an SSE frame so the client can show
        // it instead of silently hanging.
        controller.enqueue(
          encoder.encode(
            `data: ${JSON.stringify({ error: e?.message ?? String(e) })}\n\n`
          )
        );
      } finally {
        controller.close();
      }
    },
  });

  return new Response(body, {
    headers: {
      "Content-Type": "text/event-stream",
      "Cache-Control": "no-cache, no-transform",
      "X-Accel-Buffering": "no",
    },
  });
}

export function ping() {
  return "pong";
}
