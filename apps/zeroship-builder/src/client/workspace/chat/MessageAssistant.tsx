import type { ReactNode } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

export interface MessageAssistantProps {
  text: string;
  streaming?: boolean;
  /** Pre-rendered children: receipts, diffs, surveys, critic-rounds. */
  parts?: ReactNode;
}

export function MessageAssistant({ text, streaming, parts }: MessageAssistantProps) {
  return (
    <div data-testid="msg-assistant">
      <div className="font-sans text-[10px] uppercase tracking-wider text-ink-soft mb-1">
        Builder
      </div>
      <div className="font-serif text-[14.5px] leading-snug text-ink prose-headings:font-display prose-code:font-mono prose-code:text-[13px]">
        {text ? (
          <ReactMarkdown remarkPlugins={[remarkGfm]}>{text}</ReactMarkdown>
        ) : streaming ? (
          <span className="italic text-pencil">thinking…</span>
        ) : null}
        {streaming && text && (
          <span
            aria-hidden="true"
            className="inline-block ml-0.5 w-[6px] h-[14px] bg-ink/70 align-middle"
          />
        )}
      </div>
      {parts && <div className="mt-1">{parts}</div>}
    </div>
  );
}
