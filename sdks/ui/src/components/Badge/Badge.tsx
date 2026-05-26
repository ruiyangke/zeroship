import type { HTMLAttributes } from "react";
import clsx from "clsx";

export type Tone = "neutral" | "success" | "warn" | "danger" | "info";

export interface BadgeProps extends HTMLAttributes<HTMLSpanElement> {
  tone?: Tone;
}

export function Badge({ tone = "neutral", className, ...props }: BadgeProps) {
  return (
    <span className={clsx("zs-badge", `zs-badge--${tone}`, className)} {...props} />
  );
}
