// ─── LedgerRow — one row in the log ledger ──────────────────────
//
// 4-column layout: №, timestamp (mono), level (small caps), message
// (serif). Used by /p/:id/logs and admin /journal.

import type { ReactNode } from "react";
import { cn } from "../lib/utils";

export type LedgerLevel = "info" | "warn" | "error" | "deploy" | "signup" | "earn" | "live" | "retired";

export interface LedgerRowProps {
  num: number | string;
  timestamp: string;
  level: LedgerLevel;
  message: ReactNode;
  /** Optional 5th column — used by admin to show the app name. */
  app?: ReactNode;
}

export function LedgerRow({ num, timestamp, level, message, app }: LedgerRowProps) {
  const tone = toneFor(level);
  const cols = app ? "60px 100px 80px 200px 1fr" : "60px 100px 80px 1fr";

  return (
    <div
      className="grid items-baseline gap-4 py-2 border-b border-rule-2 font-serif text-[14.5px]"
      style={{ gridTemplateColumns: cols }}
      role="row"
    >
      <span className="font-serif italic text-tomato text-[12.5px]" style={{ fontFeatureSettings: '"lnum" 1' }}>
        № {String(num).padStart(3, "0")}
      </span>
      <span className="font-mono text-[12px] text-ink-soft not-italic">{timestamp}</span>
      <span
        className={cn(
          "font-sans text-[9.5px] uppercase tracking-[0.18em] self-center",
          tone === "muted"   && "text-ink-soft",
          tone === "warn"    && "text-tomato font-semibold",
          tone === "error"   && "text-tomato font-bold",
          tone === "live"    && "text-tomato font-semibold",
        )}
      >
        {level}
      </span>
      {app && <span className="font-serif italic text-ink text-[14px]">{app}</span>}
      <span className="font-serif text-ink">{message}</span>
    </div>
  );
}

function toneFor(l: LedgerLevel): "muted" | "warn" | "error" | "live" {
  if (l === "warn") return "warn";
  if (l === "error") return "error";
  if (l === "live" || l === "earn" || l === "deploy" || l === "signup") return "live";
  return "muted";
}

/** Inline `<code>` styled to match the ledger's mono insets. */
export function LedgerCode({ children }: { children: ReactNode }) {
  return (
    <code className="font-mono text-[12px] bg-paper-2 px-1 py-px rounded-[2px] text-ink">
      {children}
    </code>
  );
}
