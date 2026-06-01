// Hover-revealed action row used by both MessageUser and
// MessageAssistant. Lives separately so the bubble components stay
// presentational while we centralise click → callback → state-effect
// wiring (clipboard write, regenerate, edit-pencil) in one place.
//
// Per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §10.6:
//   - Assistant turns: ↻ regenerate · 📋 copy
//   - User turns:      ✎ edit prior   · 📋 copy
//
// Crystal migration: the row is a DS `Cluster` (wrapping inline group)
// and each action is a DS `Button variant="plain" size="small"` — the
// link-style, chrome-less button that matches the original ghost look.
// The two bespoke bits live in MessageActions.css over --zs-* tokens:
//   - hover-reveal: the row reserves one caption line of vertical space
//     even when hidden (so the bubble layout doesn't shift on hover) and
//     fades in on `group-hover` / `focus-within`.
//   - micro-caption: the actions render as a tiny uppercase tracked
//     caption, denser than the DS `small` default.
// Tooltips stay on the native `title` + `aria-label` (no DS Tooltip:
// that needs an app-root Provider and would change behavior).

import { useState, type ReactNode } from "react";
import { Button, Cluster } from "@zeroship/ui";
import "./MessageActions.css";

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
    <Cluster
      data-testid="msg-actions"
      gap={2}
      align="center"
      className="msg-actions"
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
    </Cluster>
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
    <Button
      variant="plain"
      size="small"
      data-testid={testid}
      title={title}
      aria-label={title}
      onClick={onClick}
      className="msg-action"
    >
      {children}
    </Button>
  );
}
