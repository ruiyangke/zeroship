// Builder chat client — wraps the server's async-generator chat
// function in the same shape useBuilderChat already consumes.
//
// Browser-side import flows through @zeroship/vite-plugin's
// `"use server"` transform: the function below calls the server's
// `chat` generator directly, and the plugin chunks each yielded
// event over the wire as an RPC stream.

import { chat as chatServer, type ChatRequest, type ChatEvent } from "../../server/chat";
import type { AgentEvent, ChatMessage } from "./types";

export const AGENT_URL = "/agent"; // unused — kept for compat

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
    // Wire shapes are aligned by design — both use the same
    // `{type, ...}` discriminated union.
    opts.onEvent(ev as AgentEvent);
  }
}

export function genId(): string {
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) return crypto.randomUUID();
  return Math.random().toString(36).slice(2) + Date.now().toString(36);
}
