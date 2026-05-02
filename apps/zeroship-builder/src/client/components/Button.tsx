import type { ButtonHTMLAttributes, ReactNode } from "react";
import { cn } from "../lib/utils";

type Variant = "primary" | "secondary" | "ghost" | "destructive" | "link";
type Size = "sm" | "md" | "lg";

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: Variant;
  size?: Size;
  loading?: boolean;
  leadingIcon?: ReactNode;
  trailingIcon?: ReactNode;
}

const VARIANT_CLASSES: Record<Variant, string> = {
  primary:
    "bg-tomato text-paper border-0 hover:bg-tomato-2 active:translate-y-px " +
    "shadow-[0_2px_0_-1px_var(--color-tomato-2)]",
  secondary:
    "bg-paper-2 text-ink border border-rule hover:border-ink",
  ghost:
    "bg-transparent text-ink-soft border border-rule hover:border-ink hover:text-ink",
  destructive:
    "bg-blood text-paper border-0 hover:opacity-90 active:translate-y-px",
  link:
    "bg-transparent text-cobalt border-0 underline-offset-2 hover:underline px-0 py-0",
};

const SIZE_CLASSES: Record<Size, string> = {
  sm: "px-3 py-1.5 text-xs",
  md: "px-4 py-2 text-sm",
  lg: "px-5 py-2.5 text-base",
};

export function Button({
  variant = "primary",
  size = "md",
  loading,
  disabled,
  leadingIcon,
  trailingIcon,
  className,
  children,
  ...rest
}: ButtonProps) {
  return (
    <button
      {...rest}
      disabled={disabled || loading}
      className={cn(
        "inline-flex items-center gap-2 font-sans font-medium",
        "transition-[transform,opacity,background-color,border-color] duration-200",
        "disabled:opacity-50 disabled:cursor-not-allowed cursor-pointer rounded",
        VARIANT_CLASSES[variant],
        SIZE_CLASSES[size],
        className,
      )}
    >
      {loading ? (
        <span
          className="inline-block size-3.5 rounded-full border-2 border-current border-t-transparent spin"
          aria-hidden="true"
        />
      ) : leadingIcon}
      {children}
      {trailingIcon}
    </button>
  );
}
