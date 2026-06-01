import type { ReactNode } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { Stack } from "@zeroship/ui";
import { MessageActions } from "./MessageActions";
import "./MessageAssistant.css";

export interface MessageAssistantProps {
  text: string;
  streaming?: boolean;
  /** Pre-rendered children: receipts, diffs, surveys, critic-rounds. */
  parts?: ReactNode;
  /** Called when the user clicks ↻ regenerate on this turn. ChatRail
   *  wires this to useChat.regenerate(). The button is hidden while
   *  the parent reports `streaming`, so the consumer can pass the
   *  same handler unconditionally. */
  onRegenerate?: () => void;
}

export function MessageAssistant({
  text,
  streaming,
  parts,
  onRegenerate,
}: MessageAssistantProps) {
  // `group` stays on the root: MessageActions reveals its hover row via
  // the parent group-hover state. Crystal presentation lives in the
  // co-located CSS keyed off `.msg-assistant`.
  return (
    <Stack
      data-testid="msg-assistant"
      className="group msg-assistant"
      gap={1}
    >
      <div className="msg-assistant__label">Builder</div>
      <div className="msg-assistant__prose">
        {text ? (
          <ReactMarkdown
            remarkPlugins={[remarkGfm]}
            components={{
              // Tables get a thin rule + small mono digits so they
              // read in the narrow rail without breaking the prose
              // typography elsewhere.
              table: ({ children }) => (
                <table className="msg-assistant__table">{children}</table>
              ),
              th: ({ children }) => (
                <th className="msg-assistant__th">{children}</th>
              ),
              td: ({ children }) => (
                <td className="msg-assistant__td">{children}</td>
              ),
              // Inline `code` stays subtle; block code keeps its
              // language-* class so the `pre` styles in the CSS apply.
              code: ({ children, className }) => {
                const isBlock = (className ?? "").startsWith("language-");
                if (isBlock) {
                  return <code className={className}>{children}</code>;
                }
                return <code className="msg-assistant__code">{children}</code>;
              },
              a: ({ href, children }) => (
                <a href={href} target="_blank" rel="noreferrer noopener">
                  {children}
                </a>
              ),
            }}
          >
            {text}
          </ReactMarkdown>
        ) : streaming ? (
          <span className="msg-assistant__thinking">thinking…</span>
        ) : null}
        {streaming && text && (
          <span aria-hidden="true" className="msg-assistant__cursor" />
        )}
      </div>
      {parts && <div>{parts}</div>}
      {text && (
        <MessageActions
          copyText={text}
          onRegenerate={onRegenerate}
          busy={streaming}
        />
      )}
    </Stack>
  );
}
