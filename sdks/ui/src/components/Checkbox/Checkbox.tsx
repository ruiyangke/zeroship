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
        <BaseCheckbox.Indicator className="zs-checkbox__indicator" keepMounted />
      </BaseCheckbox.Root>
    </FieldFrame>
  );
});

export const CheckboxParts = BaseCheckbox;
