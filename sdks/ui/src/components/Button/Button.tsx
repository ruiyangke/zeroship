import { forwardRef, type ButtonHTMLAttributes, type ReactNode } from "react";

export type ButtonVariant = "filled" | "tinted" | "gray" | "plain";
export type ButtonRole = "normal" | "primary" | "cancel" | "destructive";
export type ButtonSize = "small" | "medium" | "large";

export interface ButtonProps
  extends Omit<ButtonHTMLAttributes<HTMLButtonElement>, "children"> {
  /**
   * Visual style — HIG button styles.
   * - `filled`: prominent, accent fill, white text. The "primary action" look.
   * - `tinted`: translucent accent-tinted fill, accent text. Secondary action.
   * - `gray`: neutral fill, label text. Tertiary action.
   * - `plain`: no chrome, accent text. Link-style.
   *
   * Defaults to `filled`.
   */
  variant?: ButtonVariant;

  /**
   * Semantic role — HIG button roles.
   * - `normal`: no special meaning.
   * - `primary`: the default action; emits `data-role="primary"` for
   *   assistive tech and styling hooks. Per HIG, distinguish the primary
   *   choice through Style (variant), not Role alone.
   * - `cancel`: cancels the current flow; emits `data-role="cancel"`.
   * - `destructive`: destructive action; OVERRIDES the accent palette
   *   with system-red regardless of variant. Per HIG: never combine
   *   `primary` + `destructive`.
   *
   * Defaults to `normal`.
   */
  role?: ButtonRole;

  /** Size — small 32px, medium 40px (default), large 48px. */
  size?: ButtonSize;

  /**
   * Activity indicator. Per HIG, show this for actions that don't
   * instantly complete. While loading, the button is `aria-busy`,
   * forced `disabled`, label/slots are hidden via `visibility` to
   * preserve width, and an absolutely-centered spinner is shown.
   */
  loading?: boolean;

  /** Leading element (icon, etc.). */
  startSlot?: ReactNode;

  /** Trailing element. */
  endSlot?: ReactNode;

  /** Label content. */
  children?: ReactNode;
}

function classnames(...parts: Array<string | false | null | undefined>): string {
  return parts.filter(Boolean).join(" ");
}

/**
 * Inline spinner SVG. Honors prefers-reduced-motion via Button.css.
 */
function Spinner() {
  return (
    <span className="zs-button__spinner" aria-hidden="true">
      <svg viewBox="0 0 16 16" role="presentation">
        <circle cx="8" cy="8" r="6" />
      </svg>
    </span>
  );
}

export const Button = forwardRef<HTMLButtonElement, ButtonProps>(function Button(
  {
    variant = "filled",
    role = "normal",
    size = "medium",
    loading = false,
    startSlot,
    endSlot,
    children,
    className,
    disabled,
    type = "button",
    "aria-label": ariaLabel,
    ...rest
  },
  ref,
) {
  // Per HIG: never combine primary + destructive. We enforce by ignoring
  // primary if destructive is set; data-role still reports destructive.
  const effectiveRole: ButtonRole = role === "primary" && variant === "filled"
    ? "primary"
    : role;

  const composedClassName = classnames(
    "zs-button",
    `zs-button--${variant}`,
    `zs-button--${size}`,
    effectiveRole === "destructive" && "zs-button--destructive",
    className,
  );

  const isBusy = loading === true;

  return (
    <button
      ref={ref}
      type={type}
      className={composedClassName}
      data-variant={variant}
      data-role={effectiveRole}
      data-size={size}
      aria-busy={isBusy || undefined}
      aria-label={ariaLabel}
      disabled={disabled || isBusy}
      {...rest}
    >
      {startSlot ? <span className="zs-button__start">{startSlot}</span> : null}
      <span className="zs-button__label">{children}</span>
      {endSlot ? <span className="zs-button__end">{endSlot}</span> : null}
      {isBusy ? <Spinner /> : null}
    </button>
  );
});

Button.displayName = "Button";
