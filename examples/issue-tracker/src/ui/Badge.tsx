import type { ComponentPropsWithoutRef } from "react";

export type BadgeIntent = "neutral" | "info" | "success" | "warning" | "danger";
export type BadgeVariant = "soft" | "outline";
export type BadgeSize = "sm" | "md";
export type BadgeTone = "neutral" | "danger" | "info" | "muted";

export interface BadgeProps extends ComponentPropsWithoutRef<"span"> {
  intent?: BadgeIntent;
  variant?: BadgeVariant;
  size?: BadgeSize;
  tone?: BadgeTone;
}

const INTENT_CLASSES: Record<BadgeVariant, Record<BadgeIntent, string>> = {
  soft: {
    neutral: "border-line bg-surface-sunken text-ink-secondary",
    info: "border-info bg-info-soft text-info",
    success: "border-success bg-success-soft text-success",
    warning: "border-warning bg-warning-soft text-warning",
    danger: "border-danger bg-danger-soft text-danger",
  },
  outline: {
    neutral: "border-line-strong bg-surface text-ink-secondary",
    info: "border-info bg-surface text-info",
    success: "border-success bg-surface text-success",
    warning: "border-warning bg-surface text-warning",
    danger: "border-danger bg-surface text-danger",
  },
};

const TONE_CLASSES: Record<BadgeTone, string> = {
  neutral: "bg-surface-sunken text-ink-secondary",
  danger: "bg-danger-soft text-danger",
  info: "bg-info-soft text-info",
  muted: "bg-surface-sunken text-ink-muted",
};

export function Badge({
  intent,
  variant,
  size,
  tone,
  className,
  children,
  ...props
}: BadgeProps) {
  const toneBadge =
    tone !== undefined ||
    (intent === undefined && variant === undefined && size === undefined);

  if (toneBadge) {
    const toneClasses = TONE_CLASSES[tone ?? "neutral"];
    return (
      <span
        {...props}
        className={`inline-flex items-center whitespace-nowrap rounded-full px-2 py-1 text-sm font-bold tracking-[0.01em] ${toneClasses}${className ? ` ${className}` : ""}`}
      >
        {children}
      </span>
    );
  }

  const resolvedIntent = intent ?? "neutral";
  const resolvedVariant = variant ?? "soft";
  const resolvedSize = size ?? "md";

  return (
    <span
      {...props}
      className={`inline-flex min-w-0 max-w-full items-center justify-center rounded border align-middle font-medium leading-tight no-underline ${
        resolvedSize === "sm" ? "min-h-6 px-1 text-xs" : "min-h-7 px-2 text-sm"
      } ${INTENT_CLASSES[resolvedVariant][resolvedIntent]}${className ? ` ${className}` : ""}`}
    >
      <span className="min-w-0 truncate tabular-nums">{children}</span>
    </span>
  );
}
