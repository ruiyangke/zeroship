"use server";

import { stream as streamRpc, query } from "@zeroship/rpc/server";
import OpenAI from "openai";

const client = new OpenAI({ apiKey: process.env.OPENAI_API_KEY });

interface ChatMsg {
  role: "user" | "assistant" | "system";
  content: string;
}

/**
 * Streaming chat endpoint.
 *
 * Wrapped in `stream()` (the marker that opts the export into RPC
 * discovery). The async-generator body yields a plain JS object per
 * token — the zeroship runtime wraps the generator as a
 * Response(text/event-stream) with the AI-SDK Data Stream format.
 *
 * On the client side, the vite-plugin transform produces a stub that
 * exposes this as an `AsyncIterable<{token: string}>` — see App.tsx.
 */
export const chat = streamRpc(
  async function* ({
    message,
    history = [],
  }: {
    message: string;
    history?: ChatMsg[];
  }) {
    const safeHistory = Array.isArray(history) ? history : [];
    const messages: ChatMsg[] = [
      ...safeHistory.filter((m): m is ChatMsg => !!m && typeof m.content === "string"),
      { role: "user", content: message },
    ];

    const s = await client.chat.completions.create({
      model: "gpt-5.4-mini",
      stream: true,
      messages,
    });

    for await (const part of s) {
      const delta = part.choices?.[0]?.delta?.content;
      if (delta) yield { token: delta };
    }
  },
  { id: "chat" },
);

export const ping = query(() => "pong", { id: "ping" });
