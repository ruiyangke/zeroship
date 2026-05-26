import {
  forwardRef,
  type ReactNode,
  type TextareaHTMLAttributes,
} from "react";
import { Field as BaseField } from "@base-ui/react/field";
import clsx from "clsx";
import { FieldFrame } from "../Field";

export interface TextareaProps extends TextareaHTMLAttributes<HTMLTextAreaElement> {
  label?: ReactNode;
  hint?: ReactNode;
  error?: ReactNode;
}

export const Textarea = forwardRef<HTMLTextAreaElement, TextareaProps>(
  function Textarea({ label, hint, error, className, id, disabled, ...props }, ref) {
    return (
      <FieldFrame label={label} hint={hint} error={error} disabled={disabled}>
        <BaseField.Control
          render={
            <textarea
              ref={ref}
              id={id}
              disabled={disabled}
              aria-invalid={error ? true : undefined}
              className={clsx("zs-textarea", className)}
              {...props}
            />
          }
        />
      </FieldFrame>
    );
  },
);
