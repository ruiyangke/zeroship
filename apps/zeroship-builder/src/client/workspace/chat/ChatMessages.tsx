// Chat messages renderer for the workspace ChatRail.
//
// Plan 01.5 consumes the v6 UIMessage shape from `@ai-sdk/react`:
//   { id, role, parts: [{ type: "text", text }, ...] }
// and renders user / assistant turns. Custom data parts (survey, diff,
// critic-round) and tool-invocation parts are not yet emitted by the
// mock — Plan 02 will dispatch `data-*` parts into the SurveyCard /
// DiffCard / CriticRoundCard components that still live in this folder.

import { useEffect, useRef } from "react";
import type { UIMessage } from "ai";
import { MessageUser } from "./MessageUser";
import { MessageAssistant } from "./MessageAssistant";

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
        const text = m.parts
          .filter((p): p is Extract<typeof p, { type: "text" }> => p.type === "text")
          .map((p) => p.text)
          .join("");

        if (m.role === "user") {
          return <MessageUser key={m.id} text={text} />;
        }

        const isLast = idx === messages.length - 1;
        // Plan 01.5: only text is rendered. Plan 02 dispatches data-*
        // parts to SurveyCard / DiffCard / CriticRoundCard via `parts`.
        return (
          <MessageAssistant
            key={m.id}
            text={text}
            streaming={isLast && busy}
            parts={null}
          />
        );
      })}
    </div>
  );
}
