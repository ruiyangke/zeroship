// ─── NotebookPrompt — textarea-as-page ──────────────────────────
//
// White slip on cream, red ruler line on the left, ink underline
// that draws under the field on focus. The single most-used input
// in the app — home hero, wizard step 2, every empty composer.

import { type FormEvent, type KeyboardEvent, type ReactNode, type TextareaHTMLAttributes } from "react";
import { cn } from "../lib/utils";

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
    <div
      className={cn(
        "notebook relative bg-white border border-rule px-9 py-6 shadow-[0_22px_28px_-16px_rgba(34,22,12,0.10),0_4px_8px_-4px_rgba(34,22,12,0.06)]",
        outerClassName,
      )}
    >
      {/* the red ruler line */}
      <span
        className="absolute top-0 bottom-0 w-px"
        style={{ left: "22px", background: "var(--color-tomato)", opacity: 0.35 }}
        aria-hidden="true"
      />
      {label && (
        <div className="label-uc mb-2 flex items-center gap-2">
          <span className="inline-block h-px w-3.5 bg-ink" aria-hidden="true" />
          {label}
        </div>
      )}
      <textarea
        rows={rows}
        onKeyDown={handleKey}
        className="w-full resize-none border-0 bg-transparent text-ink outline-none font-serif placeholder:text-pencil placeholder:italic"
        style={{ fontSize: "20px", lineHeight: "1.4" }}
        {...rest}
      />
      <div
        className="h-[2px] origin-left scale-x-0 bg-ink transition-transform duration-[480ms]"
        style={{ transitionTimingFunction: "cubic-bezier(.2,.7,.2,1)" }}
        data-underline=""
      />
      {(hint || action) && (
        <div className="mt-3 flex items-center justify-between gap-3">
          <div className="font-sans text-[11px] text-pencil tracking-wide flex items-center gap-1.5">{hint}</div>
          <div>{action}</div>
        </div>
      )}
      {/* draw the underline on focus-within (CSS via :has). Tailwind doesn't expose this so we handle inline. */}
      <style>{`
        .notebook:focus-within > [data-underline] { transform: scaleX(1) !important; }
      `}</style>
    </div>
  );
}

/** Hint with kbd shortcut — used as the default `hint` prop. */
export function CmdEnterHint({ verb = "to send" }: { verb?: string }) {
  return (
    <>
      <kbd className="font-sans text-[10px] px-1.5 py-px border border-rule rounded-sm bg-paper-2">⌘</kbd>
      <kbd className="font-sans text-[10px] px-1.5 py-px border border-rule rounded-sm bg-paper-2">↵</kbd>
      <span>{verb}</span>
    </>
  );
}

export function asFormSubmitHandler(fn: () => void) {
  return (e: FormEvent) => {
    e.preventDefault();
    fn();
  };
}
