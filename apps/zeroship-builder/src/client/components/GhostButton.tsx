// ─── GhostButton — outlined paper alternative ───────────────────

import { forwardRef, type ButtonHTMLAttributes, type ReactNode } from "react";
import { cn } from "../lib/utils";

export interface GhostButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  danger?: boolean;
  children: ReactNode;
}

export const GhostButton = forwardRef<HTMLButtonElement, GhostButtonProps>(function GhostButton(
  { danger, className, type = "button", children, ...rest },
  ref,
) {
  return (
    <button
      ref={ref}
      type={type}
      className={cn("ghost-btn", danger && "danger", className)}
      {...rest}
    >
      {children}
    </button>
  );
});
