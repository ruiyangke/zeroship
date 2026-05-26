import {
  forwardRef,
  type InputHTMLAttributes,
  type ReactNode,
} from "react";
import { Input as BaseInput } from "@base-ui/react/input";
import clsx from "clsx";
import { FieldFrame } from "../Field";

export interface InputProps extends InputHTMLAttributes<HTMLInputElement> {
  label?: ReactNode;
  hint?: ReactNode;
  error?: ReactNode;
}

export const Input = forwardRef<HTMLInputElement, InputProps>(function Input(
  { label, hint, error, className, id, disabled, ...props },
  ref,
) {
  return (
    <FieldFrame label={label} hint={hint} error={error} disabled={disabled}>
      <BaseInput
        ref={ref}
        id={id}
        disabled={disabled}
        aria-invalid={error ? true : undefined}
        className={clsx("zs-input", className)}
        {...props}
      />
    </FieldFrame>
  );
});
