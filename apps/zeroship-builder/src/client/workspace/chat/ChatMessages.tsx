// Chat messages renderer for the workspace ChatRail.
//
// Plan 02 Phase B.2 dispatches three families of v6 message parts:
//   - text                       → MessageAssistant text (concatenated)
//   - tool-<name> / dynamic-tool → <Receipt> (one per toolCallId)
//   - data-diff                  → <DiffCard>
//
// The translator emits chunks of these types on the wire (see
// apps/zeroship-builder/src/server/_translator.ts and _middleware.ts).
// `useChat` from @ai-sdk/react reassembles them into UIMessage.parts[]
// with stable discriminants — we just type-narrow and render.
//
// Tool-call lifecycle states (per node_modules/ai/dist/index.d.ts:1694)
// arrive as a single part whose `state` cycles through input-streaming
// → input-available → output-available (or terminal output-error /
// output-denied). The Receipt component reads that lifecycle to render
// running / done / error badges.

import { useEffect, useRef, type ReactNode } from "react";
import type { UIMessage } from "ai";
import { MessageUser } from "./MessageUser";
import { MessageAssistant } from "./MessageAssistant";
import { Receipt } from "./Receipt";
import { DiffCard } from "./DiffCard";
import type { Diff } from "../../types/chat";

export interface ChatMessagesProps {
  messages: UIMessage[];
  busy: boolean;
}

export function ChatMessages({ messages, busy }: ChatMessagesProps) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const stickToBottom = useRef(true);

  useEffect(() => {
    if (!stickToBottom.current) return;
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [messages]);

  function onScroll() {
    const el = scrollRef.current;
    if (!el) return;
    const dist = el.scrollHeight - el.clientHeight - el.scrollTop;
    stickToBottom.current = dist < 60;
  }

  return (
    <div
      ref={scrollRef}
      onScroll={onScroll}
      data-testid="chat-messages"
      className="flex-1 overflow-y-auto px-5 py-4 flex flex-col gap-5 min-h-0"
    >
      {messages.length === 0 && !busy && (
        <div className="font-serif text-[14px] italic text-ink-soft">
          Tell Builder what to make.
        </div>
      )}

      {messages.map((m, idx) => {
        const text = (m.parts as Array<{ type: string; text?: string }>)
          .filter((p) => p.type === "text")
          .map((p) => p.text ?? "")
          .join("");

        if (m.role === "user") {
          return <MessageUser key={m.id} text={text} />;
        }

        const isLast = idx === messages.length - 1;
        const renderedParts = renderAssistantParts(m);
        return (
          <MessageAssistant
            key={m.id}
            text={text}
            streaming={isLast && busy}
            parts={renderedParts}
          />
        );
      })}
    </div>
  );
}

// Render non-text assistant message parts (tool invocations, custom
// data parts). Returns null if there's nothing to render so
// MessageAssistant can avoid the wrapping <div>.
function renderAssistantParts(m: UIMessage): ReactNode {
  const out: ReactNode[] = [];

  for (const part of m.parts as Array<Record<string, unknown>>) {
    const type = typeof part.type === "string" ? part.type : "";

    // Tool invocation parts: `tool-<name>` (static tools registered
    // upfront) or `dynamic-tool` (everything else — including
    // deepagents' built-in fs/exec tools, which aren't declared in
    // `tools[]` at agent-construction time).
    if (type.startsWith("tool-") || type === "dynamic-tool") {
      const toolName =
        type === "dynamic-tool"
          ? String(part.toolName ?? "tool")
          : type.slice("tool-".length);
      // Suppress write_file / edit_file receipts — those flow through
      // <DiffCard> as data-diff parts and we don't want both rendering.
      if (toolName === "write_file" || toolName === "edit_file") continue;

      const toolCallId = String(part.toolCallId ?? `${m.id}-${out.length}`);
      const state = String(part.state ?? "");
      const status: "running" | "done" | "error" =
        state === "output-available"
          ? "done"
          : state === "output-error" || state === "output-denied"
          ? "error"
          : "running";
      out.push(
        <Receipt
          key={toolCallId}
          toolName={toolName}
          status={status}
          inputJson={part.input}
          outputJson={
            state === "output-available"
              ? part.output
              : state === "output-error"
              ? part.errorText
              : undefined
          }
        />,
      );
      continue;
    }

    // Custom data parts. The wire type is `data-<NAME>` with a `data`
    // payload (per node_modules/ai/dist/index.d.ts:2055-2062).
    if (type === "data-diff") {
      const diff = (part as { data?: Diff }).data;
      if (diff && typeof diff.path === "string") {
        out.push(<DiffCard key={`diff-${out.length}`} diff={diff} />);
      }
      continue;
    }

    // data-survey, data-critic-round will land here in later phases.
  }

  return out.length > 0 ? <>{out}</> : null;
}
