import { useState } from "react";
import { useChat } from "@ai-sdk/react";
import type { UIMessage } from "ai";
import { ChatComposer } from "./ChatComposer";
import { ChatMessages } from "./ChatMessages";
import type { SurveyResponse } from "../../types/chat";

const CHAT_API_URL = "/_zs/server/server/chat.ts/postChat";

// ---------------------------------------------------------------------------
// Protocol translation: server NDJSON → AI SDK data-stream protocol
//
// The server emits newline-delimited JSON chunks in a custom format:
//   {"type":"text-delta","delta":"..."}
//   {"type":"tool-call","toolCallId":"...","toolName":"...","args":{...}}
//   {"type":"tool-result","toolCallId":"...","result":{...}}
//   {"type":"data-part","partName":"...","payload":{...}}
//   {"type":"finish",...}
//
// useChat with streamProtocol:"data" expects AI SDK's prefixed line format:
//   0:"text delta"
//   9:{"toolCallId":"...","toolName":"...","args":{...}}
//   a:{"toolCallId":"...","result":{...}}
//   2:[{...}]           ← custom data; lands in useChat's `data` array
//   d:{finishReason:"stop",usage:{...}}
//
// This fetch wrapper performs the translation transparently.
// ---------------------------------------------------------------------------

type ServerChunk =
  | { type: "text-delta"; delta: string }
  | { type: "tool-call"; toolCallId: string; toolName: string; args: unknown }
  | { type: "tool-result"; toolCallId: string; result: unknown }
  | { type: "data-part"; partName: string; payload: unknown }
  | { type: "error"; message: string }
  | { type: "finish"; usage?: { inputTokens?: number; outputTokens?: number } };

function translateChunk(chunk: ServerChunk): string {
  switch (chunk.type) {
    case "text-delta":
      return `0:${JSON.stringify(chunk.delta)}\n`;
    case "tool-call":
      return `9:${JSON.stringify({ toolCallId: chunk.toolCallId, toolName: chunk.toolName, args: chunk.args })}\n`;
    case "tool-result":
      return `a:${JSON.stringify({ toolCallId: chunk.toolCallId, result: chunk.result })}\n`;
    case "data-part":
      // Land in useChat's `data` array as a JSONValue.
      return `2:${JSON.stringify([{ partName: chunk.partName, payload: chunk.payload }])}\n`;
    case "error":
      return `3:${JSON.stringify(chunk.message)}\n`;
    case "finish":
      return `d:${JSON.stringify({
        finishReason: "stop",
        usage: {
          promptTokens: chunk.usage?.inputTokens ?? 0,
          completionTokens: chunk.usage?.outputTokens ?? 0,
        },
      })}\n`;
    default:
      return "";
  }
}

async function translatingFetch(
  input: RequestInfo | URL,
  init?: RequestInit,
): Promise<Response> {
  const upstream = await fetch(input, init);
  if (!upstream.ok || !upstream.body) return upstream;

  const reader = upstream.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";

  const translated = new ReadableStream<Uint8Array>({
    async pull(controller) {
      const encoder = new TextEncoder();
      while (true) {
        const { done, value } = await reader.read();
        if (done) {
          // Flush any remaining buffer.
          if (buffer.trim()) {
            try {
              const chunk = JSON.parse(buffer.trim()) as ServerChunk;
              const line = translateChunk(chunk);
              if (line) controller.enqueue(encoder.encode(line));
            } catch { /* ignore malformed */ }
          }
          controller.close();
          return;
        }
        buffer += decoder.decode(value, { stream: true });
        const lines = buffer.split("\n");
        buffer = lines.pop() ?? "";
        for (const line of lines) {
          const trimmed = line.trim();
          if (!trimmed) continue;
          try {
            const chunk = JSON.parse(trimmed) as ServerChunk;
            const out = translateChunk(chunk);
            if (out) controller.enqueue(encoder.encode(out));
          } catch { /* ignore malformed */ }
        }
        return; // Yield control to the caller; it will pull again.
      }
    },
    cancel() {
      reader.cancel();
    },
  });

  return new Response(translated, {
    status: upstream.status,
    headers: {
      "Content-Type": "text/plain; charset=utf-8",
      "x-vercel-ai-data-stream": "v1",
    },
  });
}

// ---------------------------------------------------------------------------

export interface ChatRailProps {
  appName?: string;
}

export function ChatRail({ appName }: ChatRailProps) {
  const [input, setInput] = useState("");

  const { messages, append, status, stop, error, data } = useChat({
    api: CHAT_API_URL,
    streamProtocol: "data",
    fetch: translatingFetch,
  });

  const busy = status === "submitted" || status === "streaming";

  function handleSubmit(text: string, _attachments: File[]) {
    void append(
      {
        role: "user",
        content: text,
      },
      {
        body: {
          // Server function expects { text, images? }.
          // Plan 01: ignore attachments to keep wire simple. Plan 02 wires images.
          text,
        },
      },
    );
  }

  function handleSurveySubmit(_response: SurveyResponse) {
    // Plan 02 will round-trip the response via a follow-up append.
    // For Plan 01 the mock doesn't wait — the survey just collapses.
  }

  return (
    <div data-testid="chat-rail" className="flex flex-col h-full bg-paper-2">
      <div className="px-5 pt-4 pb-2 border-b border-rule flex items-baseline justify-between">
        <h3 className="font-display italic font-medium text-base">Notes &amp; thoughts</h3>
        <span className="font-sans text-[10px] uppercase tracking-wider text-pencil">
          {messages.length} {messages.length === 1 ? "turn" : "turns"}
        </span>
      </div>

      <ChatMessages
        messages={messages as UIMessage[]}
        data={data}
        busy={busy}
        onSurveySubmit={handleSurveySubmit}
      />

      {error && (
        <div className="px-5 py-2 border-t border-blood/30 bg-blood/5 font-sans text-[12px] text-blood">
          {error.message}
        </div>
      )}

      <ChatComposer
        value={input}
        onChange={setInput}
        onSubmit={handleSubmit}
        onStop={() => stop()}
        busy={busy}
        placeholder={appName ? `Tell ${appName} what to make.` : "Describe what to make."}
      />
    </div>
  );
}
