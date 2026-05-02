// Hover-revealed action row used by both MessageUser and
// MessageAssistant. Lives separately so the bubble components stay
// presentational while we centralise click → callback → state-effect
// wiring (clipboard write, regenerate, edit-pencil) in one place.
//
// Per spec §10.6:
//   - Assistant turns: ↻ regenerate · 📋 copy
//   - User turns:      ✎ edit prior   · 📋 copy
//
// Buttons are visible only when the parent group is hovered (Tailwind
// `group-hover:opacity-100`). The row reserves one line of vertical
// space even when hidden so the bubble layout doesn't shift on hover.

import { useState, type ReactNode } from "react";
import { cn } from "../../lib/utils";

export interface MessageActionsProps {
  /** Plain-text representation copied to clipboard on Copy. */
  copyText: string;
  /** Show ↻ Regenerate (assistant only). Hidden during in-flight turn. */
  onRegenerate?: () => void;
  /** Show ✎ Edit (user only). */
  onEdit?: () => void;
  /** When true, hides Regenerate (turn is streaming). */
  busy?: boolean;
}

export function MessageActions({
  copyText,
  onRegenerate,
  onEdit,
  busy,
}: MessageActionsProps) {
  const [copied, setCopied] = useState(false);

  async function copy() {
    try {
      await navigator.clipboard.writeText(copyText);
      setCopied(true);
      // Revert the badge after a beat so a second copy still gives
      // visual feedback. 1.4 s matches the cursor blink rhythm we use
      // for the streaming indicator.
      window.setTimeout(() => setCopied(false), 1400);
    } catch {
      // Clipboard can fail in non-secure contexts (file://, http on
      // non-localhost). Swallowing keeps the UI quiet — the worst case
      // is the badge doesn't flip. If we ever need a louder signal,
      // the surface-wide toast is a one-liner addition.
    }
  }

  return (
    <div
      data-testid="msg-actions"
      className={cn(
        "mt-1 flex items-center gap-2 h-5",
        "opacity-0 group-hover:opacity-100 focus-within:opacity-100",
        "transition-opacity duration-150",
      )}
    >
      <ActionButton
        onClick={copy}
        testid="msg-action-copy"
        title="Copy message"
      >
        {copied ? "✓ copied" : "📋 copy"}
      </ActionButton>
      {onRegenerate && !busy && (
        <ActionButton
          onClick={onRegenerate}
          testid="msg-action-regenerate"
          title="Regenerate this reply"
        >
          ↻ regenerate
        </ActionButton>
      )}
      {onEdit && (
        <ActionButton
          onClick={onEdit}
          testid="msg-action-edit"
          title="Edit and resend"
        >
          ✎ edit
        </ActionButton>
      )}
    </div>
  );
}

function ActionButton({
  onClick,
  testid,
  title,
  children,
}: {
  onClick: () => void;
  testid: string;
  title: string;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      data-testid={testid}
      title={title}
      aria-label={title}
      onClick={onClick}
      className={cn(
        "font-sans text-[10px] uppercase tracking-wider text-pencil",
        "hover:text-ink cursor-pointer",
      )}
    >
      {children}
    </button>
  );
}
