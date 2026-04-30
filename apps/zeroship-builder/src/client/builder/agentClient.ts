// Builder chat client — wraps the server's `postChat` function
// in the same shape `useBuilderChat` already consumes.
//
// We import `postChat` from the server module like any other async fn —
// the @zeroship/vite-plugin transform replaces the server file with
// RPC stubs on the client side, so this `postChatServer(input)` call hits
// the wire automatically. No `fetch` plumbing leaks into customer code.

import { postChat as postChatServer, type ChatTurnInput } from "../../server/chat";
import type { AIStreamChunk } from "../../server/_shared/stream";
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
  // Extract the last user message as the text input
  const lastUserMessage = messages
    .slice()
    .reverse()
    .find((m) => m.role === "user");

  if (!lastUserMessage) {
    return;
  }

  const input: ChatTurnInput = {
    text: lastUserMessage.content,
  };

  const response = await postChatServer(input);
  const reader = response.body?.getReader();

  if (!reader) {
    return;
  }

  try {
    const decoder = new TextDecoder();
    let buffer = "";

    while (true) {
      const { done, value } = await reader.read();
      if (done) break;

      if (opts.signal?.aborted) {
        reader.cancel();
        return;
      }

      buffer += decoder.decode(value, { stream: true });
      const lines = buffer.split("\n");
      buffer = lines[lines.length - 1];

      for (let i = 0; i < lines.length - 1; i++) {
        const line = lines[i].trim();
        if (line) {
          try {
            const chunk = JSON.parse(line) as AIStreamChunk;
            const ev = chunkToAgentEvent(chunk);
            if (ev) {
              opts.onEvent(ev);
            }
          } catch {
            // Skip invalid JSON lines
          }
        }
      }
    }

    // Process any remaining buffer
    if (buffer.trim()) {
      try {
        const chunk = JSON.parse(buffer) as AIStreamChunk;
        const ev = chunkToAgentEvent(chunk);
        if (ev) {
          opts.onEvent(ev);
        }
      } catch {
        // Skip invalid JSON
      }
    }
  } finally {
    reader.releaseLock();
  }
}

/** Convert AIStreamChunk (AI SDK format) to AgentEvent (builder format). */
function chunkToAgentEvent(chunk: AIStreamChunk): AgentEvent | null {
  switch (chunk.type) {
    case "text-delta":
      return { type: "text", content: chunk.delta };
    case "tool-call":
      return { type: "tool_start", name: chunk.toolName, input: chunk.args };
    case "tool-result":
      return {
        type: "tool_end",
        name: "", // tool name not in result chunk
        output: typeof chunk.result === "string" ? chunk.result : JSON.stringify(chunk.result),
      };
    case "error":
      return { type: "error", content: chunk.message };
    case "finish":
      return { type: "done" };
    case "data-part":
      // data-part chunks are for survey, diff, critic-round — not in AgentEvent
      return null;
  }
}

export function genId(): string {
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) return crypto.randomUUID();
  return Math.random().toString(36).slice(2) + Date.now().toString(36);
}
