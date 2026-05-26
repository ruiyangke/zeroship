import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type ReactNode,
} from "react";
import { Checkbox as BaseCheckbox } from "@base-ui/react/checkbox";
import clsx from "clsx";
import { FieldFrame } from "../Field";

type BaseCheckboxRootProps = ComponentPropsWithoutRef<typeof BaseCheckbox.Root>;

export interface CheckboxProps
  extends Omit<BaseCheckboxRootProps, "className" | "children"> {
  label?: ReactNode;
  hint?: ReactNode;
  error?: ReactNode;
  className?: string;
}

export const Checkbox = forwardRef<HTMLElement, CheckboxProps>(function Checkbox(
  { label, hint, error, className, disabled, ...props },
  ref,
) {
  return (
    <FieldFrame label={label} hint={hint} error={error} disabled={disabled}>
      <BaseCheckbox.Root
        ref={ref}
        disabled={disabled}
        className={clsx("zs-checkbox", className)}
        {...props}
      >
        <BaseCheckbox.Indicator className="zs-checkbox__indicator" keepMounted>
          <svg
            className="zs-checkbox__check"
            viewBox="0 0 12 12"
            fill="none"
            aria-hidden="true"
          >
            <path
              d="M2.5 6.5 5 9l4.5-5.5"
              stroke="currentColor"
              strokeWidth="1.5"
              strokeLinecap="round"
              strokeLinejoin="round"
            />
          </svg>
          <svg
            className="zs-checkbox__dash"
            viewBox="0 0 12 12"
            fill="none"
            aria-hidden="true"
          >
            <path
              d="M3 6h6"
              stroke="currentColor"
              strokeWidth="1.5"
              strokeLinecap="round"
            />
          </svg>
        </BaseCheckbox.Indicator>
      </BaseCheckbox.Root>
    </FieldFrame>
  );
});

export const CheckboxParts = BaseCheckbox;
