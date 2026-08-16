import { Checkbox as BaseCheckbox } from "@base-ui/react/checkbox";
import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type ReactNode,
  type Ref,
} from "react";

export type CheckboxSize = "sm" | "md" | "lg";
export type CheckboxVariant = "default" | "tinted";

export interface CheckboxProps extends Omit<
  BaseCheckbox.Root.Props,
  "children" | "className" | "render"
> {
  size?: CheckboxSize;
  variant?: CheckboxVariant;
  className?: string;
  label?: ReactNode;
  fieldClassName?: string;
  fieldProps?: ComponentPropsWithoutRef<"label">;
}

function IndicatorGlyph({ kind }: { kind: "check" | "minus" }) {
  return (
    <svg
      aria-hidden="true"
      data-glyph={kind}
      data-size="md"
      data-slot="icon"
      fill="none"
      focusable="false"
      stroke="currentColor"
      strokeLinecap="round"
      strokeLinejoin="round"
      strokeWidth="2"
      viewBox="0 0 24 24"
    >
      <path d={kind === "check" ? "M20 6 9 17l-5-5" : "M5 12h14"} />
    </svg>
  );
}

export const Checkbox = forwardRef<HTMLSpanElement, CheckboxProps>(
  function Checkbox(
    {
      size = "md",
      variant = "default",
      className,
      label,
      fieldClassName,
      fieldProps,
      disabled,
      ...props
    },
    ref,
  ) {
    const control = (
      <BaseCheckbox.Root
        {...props}
        ref={ref as Ref<HTMLElement>}
        className={className}
        disabled={disabled}
        data-slot="checkbox"
        data-size={size}
        data-variant={variant}
      >
        <BaseCheckbox.Indicator
          keepMounted
          data-slot="checkbox-indicator"
        >
          <IndicatorGlyph kind="check" />
          <IndicatorGlyph kind="minus" />
        </BaseCheckbox.Indicator>
      </BaseCheckbox.Root>
    );

    if (label == null) return control;

    return (
      <label
        {...fieldProps}
        className={[fieldClassName, fieldProps?.className]
          .filter(Boolean)
          .join(" ") || undefined}
        data-slot="checkbox-field"
        data-size={size}
        data-disabled={disabled || undefined}
      >
        {control}
        <span data-slot="checkbox-field-text">{label}</span>
      </label>
    );
  },
);

Checkbox.displayName = "Checkbox";
