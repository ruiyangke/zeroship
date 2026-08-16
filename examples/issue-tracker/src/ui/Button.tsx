import { Button as BaseButton } from "@base-ui/react/button";
import { forwardRef } from "react";

export type ButtonVariant = "filled" | "gray" | "plain";
export type ButtonIntent = "normal" | "destructive";

export interface ButtonProps extends BaseButton.Props {
  variant?: ButtonVariant;
  intent?: ButtonIntent;
}

const BASE_CLASSES =
  "relative inline-flex h-6 min-w-0 cursor-pointer select-none items-center justify-center whitespace-nowrap rounded border px-2 text-center font-sans text-sm font-medium leading-tight no-underline! transition-colors hover:no-underline!";

const VARIANT_CLASSES: Record<ButtonVariant, string> = {
  filled:
    "border-ink bg-ink text-surface! hover:border-ink-secondary hover:bg-ink-secondary active:border-ink-muted active:bg-ink-muted",
  gray:
    "border-line-strong bg-surface text-ink! hover:border-line-strong hover:bg-surface-hover active:border-line-strong active:bg-surface-sunken",
  plain:
    "border-transparent bg-transparent text-ink-secondary! hover:border-line-strong hover:bg-surface-hover hover:text-ink! active:border-line-strong active:bg-surface-sunken active:text-ink!",
};

const PRESSED_PLAIN_CLASSES =
  "border-line bg-surface-sunken text-ink! hover:border-line-strong hover:bg-surface-hover active:border-line-strong active:bg-surface-sunken";

const DESTRUCTIVE_CLASSES: Record<ButtonVariant, string> = {
  filled:
    "border-danger bg-danger text-surface! hover:border-danger hover:bg-danger active:border-danger active:bg-danger",
  gray:
    "border-line-strong bg-surface text-danger! hover:border-danger hover:bg-danger-soft hover:text-danger! active:border-danger active:bg-surface-sunken active:text-danger!",
  plain:
    "border-transparent bg-transparent text-danger! hover:border-danger hover:bg-danger-soft hover:text-danger! active:border-danger active:bg-surface-sunken active:text-danger!",
};

function visualClasses(
  variant: ButtonVariant,
  intent: ButtonIntent,
  disabled: boolean,
  pressed: boolean,
) {
  if (disabled) {
    return variant === "plain"
      ? "cursor-not-allowed border-transparent bg-transparent text-ink-disabled!"
      : "cursor-not-allowed border-line-subtle bg-surface-sunken text-ink-disabled!";
  }

  if (intent === "destructive") return DESTRUCTIVE_CLASSES[variant];
  if (variant === "plain" && pressed) return PRESSED_PLAIN_CLASSES;
  return VARIANT_CLASSES[variant];
}

export const Button = forwardRef<HTMLElement, ButtonProps>(function Button(
  {
    variant = "filled",
    intent = "normal",
    className,
    "aria-pressed": ariaPressed,
    children,
    ...props
  },
  ref,
) {
  const pressed = ariaPressed === true || ariaPressed === "true";

  return (
    <BaseButton
      {...props}
      ref={ref}
      aria-pressed={ariaPressed}
      className={(state) => {
        const consumerClasses =
          typeof className === "function" ? className(state) : className;
        return `${BASE_CLASSES} ${visualClasses(variant, intent, state.disabled, pressed)}${
          consumerClasses ? ` ${consumerClasses}` : ""
        }`;
      }}
    >
      <span className="inline-flex min-w-0 items-center justify-center gap-1">
        <span className="min-w-0 overflow-hidden text-ellipsis">{children}</span>
      </span>
    </BaseButton>
  );
});

Button.displayName = "Button";
