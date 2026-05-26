import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type ReactNode,
} from "react";
import { Switch as BaseSwitch } from "@base-ui/react/switch";
import clsx from "clsx";
import { FieldFrame } from "../Field";

type BaseSwitchRootProps = ComponentPropsWithoutRef<typeof BaseSwitch.Root>;

export interface SwitchProps
  extends Omit<BaseSwitchRootProps, "className" | "children"> {
  label?: ReactNode;
  hint?: ReactNode;
  error?: ReactNode;
  className?: string;
}

export const Switch = forwardRef<HTMLElement, SwitchProps>(function Switch(
  { label, hint, error, className, disabled, ...props },
  ref,
) {
  return (
    <FieldFrame label={label} hint={hint} error={error} disabled={disabled}>
      <BaseSwitch.Root
        ref={ref}
        disabled={disabled}
        className={clsx("zs-switch", className)}
        {...props}
      >
        <BaseSwitch.Thumb className="zs-switch__thumb" />
      </BaseSwitch.Root>
    </FieldFrame>
  );
});

export const SwitchParts = BaseSwitch;
