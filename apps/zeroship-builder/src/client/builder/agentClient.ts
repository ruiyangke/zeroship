// Builder chat client — wraps the server's async-generator `chat`
// function in the same shape `useBuilderChat` already consumes.
//
// We import `chat` from the server module like any other async fn —
// the @zeroship/vite-plugin transform replaces the server file with
// RPC stubs on the client side, so this `chatServer(req)` call hits
// the wire automatically. No `fetch` plumbing leaks into customer code.

import { chat as chatServer, type ChatRequest } from "../../server/chat";
import type { AgentEvent, ChatMessage } from "./types";

export interface AgentContext {
  app_id?: string;
  app_name?: string;
}

export interface StreamOptions {
  signal?: AbortSignal;
  context?: AgentContext;
  onEvent: (ev: AgentEvent) => void;
}

export async function streamAgent(
  messages: ChatMessage[],
  _threadId: string,
  opts: StreamOptions,
): Promise<void> {
  const wireMessages = messages
    .filter((m) => m.role === "user" || m.role === "assistant")
    .map((m) => ({ role: m.role as "user" | "assistant", content: m.content }));

  const req: ChatRequest = {
    messages: wireMessages,
    context: opts.context,
  };

  for await (const ev of chatServer(req)) {
    if (opts.signal?.aborted) return;
    opts.onEvent(ev as AgentEvent);
  }
}

export function genId(): string {
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) return crypto.randomUUID();
  return Math.random().toString(36).slice(2) + Date.now().toString(36);
}
