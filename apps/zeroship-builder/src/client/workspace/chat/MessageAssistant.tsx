import type { ReactNode } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { MessageActions } from "./MessageActions";

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
  return (
    <div data-testid="msg-assistant" className="group">
      <div className="font-sans text-[10px] uppercase tracking-wider text-ink-soft mb-1">
        Builder
      </div>
      <div className="font-serif text-[14.5px] leading-snug text-ink prose-headings:font-display prose-code:font-mono prose-code:text-[13px] prose-pre:bg-paper-2 prose-pre:border prose-pre:border-rule prose-pre:rounded prose-pre:p-2.5 prose-pre:text-[12.5px] prose-a:text-cobalt">
        {text ? (
          <ReactMarkdown
            remarkPlugins={[remarkGfm]}
            components={{
              // Tables get a thin rule + small mono digits so they
              // read in the narrow rail without breaking the editorial
              // typography elsewhere.
              table: ({ children }) => (
                <table className="border-collapse text-[12.5px] my-2">
                  {children}
                </table>
              ),
              th: ({ children }) => (
                <th className="border border-rule px-2 py-1 text-left font-sans font-medium">
                  {children}
                </th>
              ),
              td: ({ children }) => (
                <td className="border border-rule px-2 py-1 align-top">{children}</td>
              ),
              // Inline `code` stays subtle; block code uses `pre` from
              // the prose- styles above. shiki-driven highlighting is
              // ISS-27 — see plan §10.4 / spec §4.8.4.
              code: ({ children, className }) => {
                const isBlock = (className ?? "").startsWith("language-");
                if (isBlock) {
                  return <code className={className}>{children}</code>;
                }
                return (
                  <code className="px-1 py-[1px] bg-paper-2 border border-rule rounded text-[12.5px]">
                    {children}
                  </code>
                );
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
      {text && (
        <MessageActions
          copyText={text}
          onRegenerate={onRegenerate}
          busy={streaming}
        />
      )}
    </div>
  );
}
