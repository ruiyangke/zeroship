import type { ComponentPropsWithoutRef } from "react";

import { cn } from "./cn";

export type BadgeIntent =
  | "neutral"
  | "muted"
  | "info"
  | "success"
  | "warning"
  | "danger";
export type BadgeVariant = "soft" | "outline";

export interface BadgeProps extends ComponentPropsWithoutRef<"span"> {
  intent?: BadgeIntent;
  variant?: BadgeVariant;
}

const INTENT_CLASSES: Record<BadgeVariant, Record<BadgeIntent, string>> = {
  soft: {
    neutral: "border-line bg-surface-sunken text-ink-secondary",
    muted: "border-line bg-surface-sunken text-ink-muted",
    info: "border-info bg-info-soft text-info text-info-strong",
    success: "border-success bg-success-soft text-success text-success-strong",
    warning: "border-warning bg-warning-soft text-warning text-warning-strong",
    danger: "border-danger bg-danger-soft text-danger text-danger-strong",
  },
  outline: {
    neutral: "border-line-strong bg-surface text-ink-secondary",
    muted: "border-line-strong bg-surface text-ink-muted",
    info: "border-info bg-surface text-info text-info-strong",
    success: "border-success bg-surface text-success text-success-strong",
    warning: "border-warning bg-surface text-warning text-warning-strong",
    danger: "border-danger bg-surface text-danger text-danger-strong",
  },
};

export function Badge({
  intent = "neutral",
  variant = "soft",
  className,
  children,
  ...props
}: BadgeProps) {
  return (
    <span
      {...props}
      className={cn(
        "inline-flex min-h-6 min-w-0 max-w-full items-center justify-center rounded border px-1 align-middle text-xs font-medium leading-tight no-underline",
        INTENT_CLASSES[variant][intent],
        className,
      )}
    >
      <span className="min-w-0 truncate tabular-nums">{children}</span>
    </span>
  );
}
