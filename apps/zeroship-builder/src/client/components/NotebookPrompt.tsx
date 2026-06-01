// ─── NotebookPrompt — textarea-as-page (crystal) ────────────────
//
// An editorial prompt block: a Card surface with a leading accent
// ruler, an eyebrow label, a large textarea, an ink underline that
// draws under the field on focus, and a footer row (hint left, action
// right). The single most-used input in the app — home hero, wizard
// step 2, every empty composer.
//
// Rebuilt over @zeroship/ui (Card + Stack/Cluster layout primitives)
// plus a co-located stylesheet reading --zs-* tokens for the textarea
// surface and focus-underline motion (no DS textarea exists; Input is
// an <input>). The public prop API is unchanged.

import { type FormEvent, type KeyboardEvent, type ReactNode, type TextareaHTMLAttributes } from "react";
import { Card, Cluster, Stack } from "@zeroship/ui";
import "./NotebookPrompt.css";

export interface NotebookPromptProps
  extends Omit<TextareaHTMLAttributes<HTMLTextAreaElement>, "className"> {
  /** Eyebrow above the textarea — e.g. "PROJECT NO. 04 · DRAFT". */
  label?: string;
  /** Bottom-row hint on the left. */
  hint?: ReactNode;
  /** Bottom-row action on the right. */
  action?: ReactNode;
  /** Allow ⌘/Ctrl+Enter to fire `onSubmit` for keyboard-eager users. */
  onCmdEnter?: () => void;
  /** Optional className extension for the outer card. */
  outerClassName?: string;
}

export function NotebookPrompt({
  label,
  hint,
  action,
  onCmdEnter,
  rows = 3,
  outerClassName,
  onKeyDown,
  ...rest
}: NotebookPromptProps) {
  function handleKey(e: KeyboardEvent<HTMLTextAreaElement>) {
    onKeyDown?.(e);
    if ((e.metaKey || e.ctrlKey) && e.key === "Enter") {
      e.preventDefault();
      onCmdEnter?.();
    }
  }

  return (
    <Card
      variant="outline"
      className={
        outerClassName
          ? `zb-notebook-prompt ${outerClassName}`
          : "zb-notebook-prompt"
      }
    >
      {/* the accent ruler line */}
      <span className="zb-notebook-prompt__rule" aria-hidden="true" />

      <Stack gap={2}>
        {label && (
          <Cluster gap={2} className="zb-notebook-prompt__eyebrow">
            <span
              className="zb-notebook-prompt__eyebrow-tick"
              aria-hidden="true"
            />
            {label}
          </Cluster>
        )}

        <textarea
          rows={rows}
          onKeyDown={handleKey}
          className="zb-notebook-prompt__textarea"
          {...rest}
        />

        <div className="zb-notebook-prompt__underline" data-underline="" />
      </Stack>

      {(hint || action) && (
        <Cluster justify="between" gap={3}>
          <span className="zb-notebook-prompt__hint">{hint}</span>
          <div>{action}</div>
        </Cluster>
      )}
    </Card>
  );
}

/** Hint with kbd shortcut — used as the default `hint` prop. */
export function CmdEnterHint({ verb = "to send" }: { verb?: string }) {
  return (
    <Cluster gap={1} asChild>
      <span>
        <kbd className="zb-cmd-enter-hint__key">⌘</kbd>
        <kbd className="zb-cmd-enter-hint__key">↵</kbd>
        <span>{verb}</span>
      </span>
    </Cluster>
  );
}

export function asFormSubmitHandler(fn: () => void) {
  return (e: FormEvent) => {
    e.preventDefault();
    fn();
  };
}
