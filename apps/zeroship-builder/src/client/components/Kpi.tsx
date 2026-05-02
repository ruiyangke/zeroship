// ─── KpiCell — admin overview KPI ───────────────────────────────

import type { ReactNode } from "react";
import { cn } from "../lib/utils";

export interface KpiProps {
  label: string;
  value: ReactNode;
  delta?: ReactNode;
  /** When true, delta renders muted instead of tomato. */
  flat?: boolean;
}

export function Kpi({ label, value, delta, flat }: KpiProps) {
  return (
    <div className="border border-rule p-5 flex flex-col gap-1.5 bg-white">
      <span className="label-uc tracking-[0.2em]">{label}</span>
      <span
        className="font-serif text-[36px] font-medium leading-none -tracking-[0.02em]"
        style={{ fontVariationSettings: '"opsz" 96' }}
      >
        {value}
      </span>
      {delta && (
        <span className={cn("font-serif italic text-[13px]", flat ? "text-pencil" : "text-tomato")}>
          {delta}
        </span>
      )}
    </div>
  );
}

/** Status pill — live/draft/suspended in admin tables. */
export function StatusPill({ status }: { status: "live" | "draft" | "suspended" | "errored" }) {
  const map = {
    live:      { color: "var(--color-tomato)",   bg: "var(--color-tomato-3)",  label: "live" },
    draft:     { color: "var(--color-ink-soft)", bg: "var(--color-paper-3)",   label: "draft" },
    suspended: { color: "var(--color-ink-soft)", bg: "var(--color-paper-3)",   label: "suspended" },
    errored:   { color: "var(--color-tomato)",   bg: "var(--color-tomato-3)",  label: "errored" },
  } as const;
  const v = map[status];
  return (
    <span
      className="inline-flex items-center gap-1.5 px-2 py-0.5 font-sans text-[9.5px] uppercase tracking-[0.16em] rounded-full"
      style={{ color: v.color, backgroundColor: v.bg }}
    >
      <span className="size-[5px] rounded-full" style={{ background: v.color, opacity: 0.8 }} aria-hidden="true" />
      {v.label}
    </span>
  );
}
