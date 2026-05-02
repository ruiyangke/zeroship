import { useEffect, useId, useRef, type ReactNode } from "react";

export interface ModalProps {
  open: boolean;
  onClose: () => void;
  title?: string;
  children: ReactNode;
  width?: number;
}

export function Modal({ open, onClose, title, children, width = 480 }: ModalProps) {
  const titleId = useId();
  const previousFocusRef = useRef<HTMLElement | null>(null);
  const containerRef = useRef<HTMLDivElement | null>(null);

  // Esc to close + focus management. We capture the focused element
  // before opening so it can be restored on close (WCAG 2.4.3).
  useEffect(() => {
    if (!open) return;
    previousFocusRef.current = document.activeElement as HTMLElement | null;
    // Defer focus shift one tick so the container is mounted.
    queueMicrotask(() => {
      const el = containerRef.current;
      if (!el) return;
      // Focus the first focusable child if any, else the dialog itself.
      const focusable = el.querySelector<HTMLElement>(
        'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])',
      );
      (focusable ?? el).focus();
    });
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("keydown", onKey);
      // Restore focus on close so keyboard users land back where they
      // were. Guarded — the previous element may have been removed.
      const prev = previousFocusRef.current;
      if (prev && document.body.contains(prev)) prev.focus();
    };
  }, [open, onClose]);

  if (!open) return null;
  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby={title ? titleId : undefined}
      aria-label={title ? undefined : "Dialog"}
      className="fixed inset-0 z-50 flex items-center justify-center bg-ink/40 p-4"
      onClick={onClose}
    >
      <div
        ref={containerRef}
        tabIndex={-1}
        onClick={(e) => e.stopPropagation()}
        style={{ width }}
        className="bg-paper border border-rule rounded shadow-2xl max-w-[calc(100vw-32px)] max-h-[calc(100vh-64px)] overflow-auto outline-none"
      >
        {title && (
          <div
            id={titleId}
            className="px-5 py-3 border-b border-rule font-sans font-semibold text-ink"
          >
            {title}
          </div>
        )}
        <div className="p-5">{children}</div>
      </div>
    </div>
  );
}
