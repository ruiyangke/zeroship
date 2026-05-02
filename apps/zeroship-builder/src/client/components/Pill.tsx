import type { ButtonHTMLAttributes, ReactNode } from "react";
import { cn } from "../lib/utils";

export interface PillProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  active?: boolean;
  size?: "sm" | "md";
  leadingIcon?: ReactNode;
}

export function Pill({
  active,
  size = "md",
  leadingIcon,
  className,
  children,
  ...rest
}: PillProps) {
  return (
    <button
      {...rest}
      className={cn(
        "inline-flex items-center gap-1.5 rounded-full font-sans transition-colors cursor-pointer",
        size === "sm" ? "px-2.5 py-1 text-[11px]" : "px-3 py-1.5 text-xs",
        active
          ? "bg-tomato text-paper border-0"
          : "bg-transparent text-ink-soft border border-rule hover:border-ink hover:text-ink",
        className,
      )}
    >
      {leadingIcon}
      {children}
    </button>
  );
}
