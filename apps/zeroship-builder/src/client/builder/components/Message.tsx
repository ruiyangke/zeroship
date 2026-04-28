// ─── Message ────────────────────────────────────────────────────
// Single chat message. Light formatting (line breaks, code blocks).
// Tool-call cards render inline below the assistant text.

import { Bot, User } from "lucide-react";
import type { ChatMessage } from "../types";
import { ToolCall } from "./ToolCall";

interface Props {
  message: ChatMessage;
  /** True only for the actively-streaming assistant message. */
  streaming?: boolean;
}

export function Message({ message, streaming }: Props) {
  const isUser = message.role === "user";
  const Icon = isUser ? User : Bot;

  return (
    <div className="flex gap-3 px-3 py-3">
      <div className="shrink-0 size-7 flex items-center justify-center border border-border bg-muted">
        <Icon className="size-3.5 text-muted-foreground" />
      </div>

      <div className="flex-1 min-w-0">
        <div className="text-[10px] uppercase tracking-wider text-muted-foreground mb-1">
          {isUser ? "you" : "assistant"}
        </div>

        {message.content && (
          <div className="text-sm leading-relaxed text-foreground whitespace-pre-wrap break-words">
            {renderInline(message.content)}
            {streaming && <span className="inline-block ml-0.5 w-1.5 h-3.5 bg-foreground/70 animate-pulse align-middle" />}
          </div>
        )}

        {!message.content && streaming && (
          <div className="text-sm text-muted-foreground italic">thinking…</div>
        )}

        {message.tools && message.tools.length > 0 && (
          <div className="mt-2 space-y-1">
            {message.tools.map((t) => (
              <ToolCall key={t.id} tool={t} />
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

/**
 * Render the message content with minimal markdown:
 *  - triple-backtick fenced code blocks → <pre>
 *  - inline single-backticks → <code>
 *  - everything else as plain text (whitespace preserved by parent <div>)
 *
 * Kept tiny on purpose; agent output is typically conversational +
 * occasional fenced code. Full markdown is a polish-pass concern.
 */
function renderInline(text: string): React.ReactNode {
  const parts: React.ReactNode[] = [];
  let i = 0;
  let key = 0;

  while (i < text.length) {
    const fenceStart = text.indexOf("```", i);
    if (fenceStart < 0) {
      parts.push(<span key={key++}>{renderInlineCode(text.slice(i), key)}</span>);
      key += 2;
      break;
    }
    if (fenceStart > i) {
      parts.push(<span key={key++}>{renderInlineCode(text.slice(i, fenceStart), key)}</span>);
      key += 2;
    }
    const fenceEnd = text.indexOf("```", fenceStart + 3);
    if (fenceEnd < 0) {
      // Unclosed — render rest as code.
      const block = text.slice(fenceStart + 3);
      parts.push(
        <pre key={key++} className="my-2 px-2 py-1.5 border border-border bg-muted text-[11px] font-mono overflow-auto whitespace-pre">
          {stripLangLine(block)}
        </pre>,
      );
      break;
    }
    const block = text.slice(fenceStart + 3, fenceEnd);
    parts.push(
      <pre key={key++} className="my-2 px-2 py-1.5 border border-border bg-muted text-[11px] font-mono overflow-auto whitespace-pre">
        {stripLangLine(block)}
      </pre>,
    );
    i = fenceEnd + 3;
  }
  return parts;
}

function renderInlineCode(text: string, baseKey: number): React.ReactNode {
  const parts: React.ReactNode[] = [];
  let i = 0;
  let key = baseKey;
  while (i < text.length) {
    const tickStart = text.indexOf("`", i);
    if (tickStart < 0) {
      parts.push(<span key={key++}>{text.slice(i)}</span>);
      break;
    }
    if (tickStart > i) parts.push(<span key={key++}>{text.slice(i, tickStart)}</span>);
    const tickEnd = text.indexOf("`", tickStart + 1);
    if (tickEnd < 0) {
      parts.push(<span key={key++}>{text.slice(tickStart)}</span>);
      break;
    }
    parts.push(
      <code key={key++} className="px-1 py-0.5 bg-muted border border-border text-[12px] font-mono">
        {text.slice(tickStart + 1, tickEnd)}
      </code>,
    );
    i = tickEnd + 1;
  }
  return parts;
}

function stripLangLine(block: string): string {
  // Drop the optional "javascript\n" language hint on the first line.
  const nl = block.indexOf("\n");
  if (nl > 0 && /^[a-zA-Z0-9_+-]+$/.test(block.slice(0, nl).trim())) {
    return block.slice(nl + 1);
  }
  return block;
}
