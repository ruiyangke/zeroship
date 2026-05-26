import { forwardRef, type HTMLAttributes } from "react";
import clsx from "clsx";

export interface CardProps extends HTMLAttributes<HTMLDivElement> {
  tone?: "neutral" | "accent" | "danger";
  interactive?: boolean;
}

export const Card = forwardRef<HTMLDivElement, CardProps>(function Card(
  { tone = "neutral", interactive = false, className, ...props },
  ref,
) {
  return (
    <div
      ref={ref}
      className={clsx(
        "zs-card",
        `zs-card--${tone}`,
        interactive && "zs-card--interactive",
        className,
      )}
      {...props}
    />
  );
});
