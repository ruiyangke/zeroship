// ─── Agent SSE client ───────────────────────────────────────────
//
// Streams events from the deepagents service (default :4444). The
// caller passes a per-event callback; we parse SSE frames, decode
// JSON payloads, and ignore malformed lines (the wire is best-effort).

import type { AgentEvent, ChatMessage } from "./types";

export const AGENT_URL =
  (import.meta.env.VITE_AGENT_URL as string | undefined) ?? "http://localhost:4444";

export interface AgentContext {
  app_id?: string;
  app_name?: string;
}

export interface StreamOptions {
  signal?: AbortSignal;
  context?: AgentContext;
  onEvent: (ev: AgentEvent) => void;
}

/** POST /chat with the conversation; surface every parsed event. */
export async function streamAgent(
  messages: ChatMessage[],
  threadId: string,
  opts: StreamOptions,
): Promise<void> {
  const wireMessages = messages
    .filter((m) => m.role === "user" || m.role === "assistant")
    .map((m) => ({ role: m.role, content: m.content }));

  const res = await fetch(`${AGENT_URL}/chat`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      messages: wireMessages,
      thread_id: threadId,
      context: opts.context,
    }),
    signal: opts.signal,
  });

  if (!res.ok || !res.body) {
    throw new Error(`agent /chat → ${res.status}: ${await res.text().catch(() => "")}`);
  }

  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";

  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });

    let nl: number;
    while ((nl = buffer.indexOf("\n")) >= 0) {
      const line = buffer.slice(0, nl).trim();
      buffer = buffer.slice(nl + 1);
      if (!line.startsWith("data:")) continue;
      const json = line.slice(5).trim();
      if (!json) continue;
      try {
        const ev = JSON.parse(json) as AgentEvent;
        opts.onEvent(ev);
      } catch {
        // Skip malformed payload — keep the stream alive.
      }
    }
  }
}

/** Generate a stable id usable as React key + localStorage row id. */
export function genId(): string {
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
    return crypto.randomUUID();
  }
  return Math.random().toString(36).slice(2) + Date.now().toString(36);
}
