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
 * Writing `async function*` yields a plain JS object per token — the
 * zeroship runtime auto-wraps the generator as a Response(text/event-stream)
 * with `event: yield` frames per yielded value and `event: return` /
 * `event: error` to close the stream.
 *
 * On the client side, the vite-plugin transform produces a stub that
 * exposes this as an `AsyncIterable<{token: string}>` — see App.tsx.
 */
export async function* chat(message: string, history: ChatMsg[] = []) {
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

  for await (const part of stream) {
    const delta = part.choices?.[0]?.delta?.content;
    if (delta) yield { token: delta };
  }
}

export function ping() {
  return "pong";
}
