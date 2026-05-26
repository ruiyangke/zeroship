import { forwardRef, type ButtonHTMLAttributes } from "react";
import clsx from "clsx";
import type { Tone } from "../Badge";

export interface ChipProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  active?: boolean;
  tone?: Tone;
}

export const Chip = forwardRef<HTMLButtonElement, ChipProps>(function Chip(
  { active = false, tone = "neutral", className, type = "button", ...props },
  ref,
) {
  return (
    <button
      ref={ref}
      type={type}
      aria-pressed={active}
      className={clsx(
        "zs-chip",
        `zs-chip--${tone}`,
        active && "zs-chip--active",
        className,
      )}
      {...props}
    />
  );
});
