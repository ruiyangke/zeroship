// ─── StampButton — the tomato action ────────────────────────────
//
// The single primary action style across the app. Slightly rotated,
// tomato-on-paper, with a soft press-down on click. Trailing italic
// arrow renders inline.

import { forwardRef, type ButtonHTMLAttributes, type ReactNode } from "react";
import { cn } from "../lib/utils";

export interface StampButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  loading?: boolean;
  children: ReactNode;
  /** Hide the trailing arrow if the button text already implies forward motion. */
  noArrow?: boolean;
}

export const StampButton = forwardRef<HTMLButtonElement, StampButtonProps>(function StampButton(
  { loading, children, noArrow, className, type = "button", disabled, ...rest },
  ref,
) {
  return (
    <button
      ref={ref}
      type={type}
      disabled={disabled || loading}
      className={cn("stamp-btn", className)}
      {...rest}
    >
      <span>{children}</span>
      {!noArrow && !loading && <span className="arrow">→</span>}
      {loading && <span className="arrow">…</span>}
    </button>
  );
});
