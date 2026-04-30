// ─── FilterPill — square chip for filter rows ───────────────────

import type { ButtonHTMLAttributes } from "react";
import { cn } from "../lib/utils";

export interface FilterPillProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  active?: boolean;
}

export function FilterPill({ active, className, type = "button", ...rest }: FilterPillProps) {
  return (
    <button
      type={type}
      className={cn(
        "px-3.5 py-1.5 border font-sans text-[10.5px] uppercase tracking-[0.18em] transition-colors",
        active
          ? "bg-ink text-paper border-ink"
          : "bg-transparent border-rule text-ink-soft hover:border-ink hover:text-ink",
        className,
      )}
      {...rest}
    />
  );
}
