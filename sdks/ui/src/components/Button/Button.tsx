import { forwardRef, type ComponentPropsWithoutRef } from "react";
import { Button as BaseButton } from "@base-ui/react/button";
import clsx from "clsx";
import { Spinner } from "../Spinner";

type BaseButtonProps = ComponentPropsWithoutRef<typeof BaseButton>;

export interface ButtonProps extends Omit<BaseButtonProps, "className"> {
  variant?: "primary" | "secondary" | "ghost" | "danger";
  size?: "sm" | "md" | "lg";
  loading?: boolean;
  className?: string;
}

export const Button = forwardRef<HTMLElement, ButtonProps>(function Button(
  {
    variant = "primary",
    size = "md",
    loading = false,
    disabled,
    children,
    className,
    type = "button",
    ...props
  },
  ref,
) {
  return (
    <BaseButton
      ref={ref}
      type={type}
      disabled={disabled || loading}
      className={clsx(
        "zs-button",
        `zs-button--${variant}`,
        `zs-button--${size}`,
        loading && "zs-button--loading",
        className,
      )}
      aria-busy={loading || undefined}
      {...props}
    >
      {loading && <Spinner size="sm" />}
      <span className="zs-button__label">{children}</span>
    </BaseButton>
  );
});

export const ButtonParts = BaseButton;
