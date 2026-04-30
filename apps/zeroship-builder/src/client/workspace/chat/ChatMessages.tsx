import { useEffect, useRef } from "react";
import type { UIMessage } from "ai";
import type { ToolInvocationUIPart } from "@ai-sdk/ui-utils";
import { MessageUser } from "./MessageUser";
import { MessageAssistant } from "./MessageAssistant";
import { Receipt } from "./Receipt";
import { DiffCard } from "./DiffCard";
import { SurveyCard } from "./SurveyCard";
import { CriticRoundCard } from "./CriticRoundCard";
import type { Diff, Survey, CriticRound, SurveyResponse, CustomDataPart } from "../../types/chat";
import type { JSONValue } from "ai";

export interface ChatMessagesProps {
  messages: UIMessage[];
  /** Custom data parts from useChat's `data` array — indexed by arrival order. */
  data?: JSONValue[];
  busy: boolean;
  onSurveySubmit?: (response: SurveyResponse) => void;
}

export function ChatMessages({ messages, data, busy, onSurveySubmit }: ChatMessagesProps) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const stickToBottom = useRef(true);

  useEffect(() => {
    if (!stickToBottom.current) return;
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [messages, data]);

  function onScroll() {
    const el = scrollRef.current;
    if (!el) return;
    const dist = el.scrollHeight - el.clientHeight - el.scrollTop;
    stickToBottom.current = dist < 60;
  }

  // Parse custom data parts sent via the data stream's `2:` protocol lines.
  // The server emits { partName, payload } objects; we collect them here.
  const customParts: CustomDataPart[] = [];
  if (data) {
    for (const item of data) {
      if (
        item !== null &&
        typeof item === "object" &&
        !Array.isArray(item) &&
        "partName" in item &&
        "payload" in item
      ) {
        const partName = (item as { partName: unknown }).partName;
        const payload = (item as { payload: unknown }).payload;
        if (partName === "survey") {
          customParts.push({ kind: "survey", payload: payload as Survey });
        } else if (partName === "diff") {
          customParts.push({ kind: "diff", payload: payload as Diff });
        } else if (partName === "critic-round") {
          customParts.push({ kind: "critic-round", payload: payload as CriticRound });
        }
      }
    }
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
        if (m.role === "user") {
          // Concatenate any text parts into a single user-text body.
          const text = m.parts
            .filter((p): p is Extract<typeof p, { type: "text" }> => p.type === "text")
            .map((p) => (p as { type: "text"; text: string }).text)
            .join("");
          return <MessageUser key={m.id} text={text} />;
        }

        if (m.role === "assistant") {
          const isLast = idx === messages.length - 1;
          let text = "";
          const renderedParts: React.ReactNode[] = [];

          for (const p of m.parts) {
            if (p.type === "text") {
              text += (p as { type: "text"; text: string }).text;
            } else if (p.type === "tool-invocation") {
              // Rendered below after the text loop.
            }
            // reasoning, source, file, step-start parts are ignored for Plan 01.
          }

          // Render tool-invocation parts as Receipts.
          const toolParts = m.parts.filter(
            (p): p is ToolInvocationUIPart => p.type === "tool-invocation",
          );
          for (const tp of toolParts) {
            const inv = tp.toolInvocation;
            const isDone = inv.state === "result";
            const args = "args" in inv ? inv.args : undefined;
            const result = "result" in inv ? inv.result : undefined;
            renderedParts.push(
              <Receipt
                key={inv.toolCallId}
                toolName={inv.toolName}
                status={isDone ? "done" : "running"}
                summary={summarizeTool(inv.toolName, args)}
                inputJson={args}
                outputJson={result}
              />,
            );
          }

          // For the last assistant message, also render accumulated custom data parts.
          if (isLast) {
            for (let i = 0; i < customParts.length; i++) {
              const cp = customParts[i];
              if (cp.kind === "diff") {
                renderedParts.push(
                  <DiffCard key={`data-${i}`} diff={cp.payload} />,
                );
              } else if (cp.kind === "survey") {
                renderedParts.push(
                  <SurveyCard
                    key={`data-${i}`}
                    surveyId={`${m.id}-survey-${i}`}
                    survey={cp.payload}
                    onSubmit={(r) => onSurveySubmit?.(r)}
                    onSkip={() =>
                      onSurveySubmit?.({
                        survey_id: `${m.id}`,
                        answers: {},
                        skipped: true,
                      })
                    }
                  />,
                );
              } else if (cp.kind === "critic-round") {
                renderedParts.push(
                  <CriticRoundCard key={`data-${i}`} round={cp.payload} />,
                );
              }
            }
          }

          return (
            <MessageAssistant
              key={m.id}
              text={text}
              streaming={isLast && busy}
              parts={renderedParts.length > 0 ? <>{renderedParts}</> : null}
            />
          );
        }

        return null;
      })}
    </div>
  );
}

function summarizeTool(name: string, args: unknown): string {
  if (name === "write_file") {
    const path = (args as { path?: string } | undefined)?.path;
    return path ? `Wrote \`${path}\`.` : "Wrote a file.";
  }
  if (name === "ask_survey") return "Asked a clarifying question.";
  return `Ran ${name}.`;
}
