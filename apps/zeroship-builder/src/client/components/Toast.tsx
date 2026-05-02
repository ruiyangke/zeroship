import type { ReactNode } from "react";
import { cn } from "../lib/utils";

export type ToastTone = "info" | "success" | "warn" | "error";

export interface ToastProps {
  tone?: ToastTone;
  children: ReactNode;
  onDismiss?: () => void;
}

const TONE_CLASSES: Record<ToastTone, string> = {
  info:    "border-cobalt/30 bg-cobalt/5  text-ink",
  success: "border-ivy/30    bg-ivy/5     text-ink",
  warn:    "border-amber/40  bg-amber/5   text-ink",
  error:   "border-blood/30  bg-blood/5   text-blood",
};

export function Toast({ tone = "info", children, onDismiss }: ToastProps) {
  return (
    <div
      role="status"
      className={cn(
        "border px-4 py-2.5 rounded font-sans text-sm flex items-center gap-3",
        TONE_CLASSES[tone],
      )}
    >
      <div className="flex-1">{children}</div>
      {onDismiss && (
        <button
          type="button"
          onClick={onDismiss}
          aria-label="Dismiss"
          className="text-current opacity-60 hover:opacity-100 cursor-pointer"
        >
          ✕
        </button>
      )}
    </div>
  );
}
