import type { HTMLAttributes } from "react";
import clsx from "clsx";

export interface SpinnerProps extends HTMLAttributes<HTMLSpanElement> {
  size?: "sm" | "md" | "lg";
}

export function Spinner({ size = "md", className, ...props }: SpinnerProps) {
  return (
    <span
      aria-hidden="true"
      className={clsx("zs-spinner", `zs-spinner--${size}`, className)}
      {...props}
    />
  );
}
