import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type ReactNode,
} from "react";
import { Fieldset as BaseFieldset } from "@base-ui/react/fieldset";
import { Radio as BaseRadio } from "@base-ui/react/radio";
import { RadioGroup as BaseRadioGroup } from "@base-ui/react/radio-group";
import clsx from "clsx";

type BaseRadioRootProps = ComponentPropsWithoutRef<typeof BaseRadio.Root>;
type BaseRadioGroupProps = ComponentPropsWithoutRef<typeof BaseRadioGroup>;

export interface RadioProps
  extends Omit<BaseRadioRootProps, "className" | "children"> {
  label?: ReactNode;
  className?: string;
}

export const Radio = forwardRef<HTMLElement, RadioProps>(function Radio(
  { label, className, disabled, ...props },
  ref,
) {
  return (
    <label className={clsx("zs-radio-option", disabled && "zs-radio-option--disabled")}>
      <BaseRadio.Root
        ref={ref}
        disabled={disabled}
        className={clsx("zs-radio", className)}
        {...props}
      >
        <BaseRadio.Indicator className="zs-radio__indicator" keepMounted />
      </BaseRadio.Root>
      {label && <span className="zs-radio-option__label">{label}</span>}
    </label>
  );
});

export interface RadioGroupItem {
  value: string;
  label: ReactNode;
  disabled?: boolean;
}

export interface RadioGroupProps
  extends Omit<BaseRadioGroupProps, "className" | "children"> {
  label?: ReactNode;
  hint?: ReactNode;
  error?: ReactNode;
  items?: ReadonlyArray<RadioGroupItem>;
  children?: ReactNode;
  className?: string;
}

export function RadioGroup({
  label,
  hint,
  error,
  items,
  children,
  className,
  disabled,
  ...props
}: RadioGroupProps) {
  return (
    <BaseFieldset.Root
      disabled={disabled}
      className={clsx("zs-radio-group-field", className)}
    >
      {label && <BaseFieldset.Legend className="zs-field__label">{label}</BaseFieldset.Legend>}
      <BaseRadioGroup
        disabled={disabled}
        className="zs-radio-group"
        aria-invalid={error ? true : undefined}
        {...props}
      >
        {items?.map((item) => (
          <Radio
            key={item.value}
            value={item.value}
            label={item.label}
            disabled={item.disabled}
          />
        ))}
        {children}
      </BaseRadioGroup>
      {hint && !error && <p className="zs-field__hint">{hint}</p>}
      {error && <p className="zs-field__error">{error}</p>}
    </BaseFieldset.Root>
  );
}

export const RadioParts = BaseRadio;
export const RadioGroupParts = BaseRadioGroup;
