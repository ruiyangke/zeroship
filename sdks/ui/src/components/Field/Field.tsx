import { type ReactNode } from "react";
import { Field as BaseField } from "@base-ui/react/field";
import clsx from "clsx";

export interface FieldFrameProps {
  label?: ReactNode;
  hint?: ReactNode;
  error?: ReactNode;
  disabled?: boolean;
  className?: string;
  labelNative?: boolean;
  children: ReactNode;
}

export function FieldFrame({
  label,
  hint,
  error,
  disabled,
  className,
  labelNative = true,
  children,
}: FieldFrameProps) {
  return (
    <BaseField.Root
      disabled={disabled}
      invalid={Boolean(error)}
      className={clsx("zs-field", className)}
    >
      {label && (
        <BaseField.Label className="zs-field__label" nativeLabel={labelNative}>
          {label}
        </BaseField.Label>
      )}
      {children}
      {hint && !error && (
        <BaseField.Description className="zs-field__hint">{hint}</BaseField.Description>
      )}
      {error && (
        <BaseField.Error className="zs-field__error" match>
          {error}
        </BaseField.Error>
      )}
    </BaseField.Root>
  );
}

export const FieldParts = BaseField;
