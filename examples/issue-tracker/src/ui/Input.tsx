import { Input as BaseInput } from "@base-ui/react/input";
import { forwardRef, type ReactNode } from "react";

export interface InputProps extends Omit<BaseInput.Props, "render"> {
  startSlot?: ReactNode;
  endSlot?: ReactNode;
  wrapperClassName?: string;
}

export const Input = forwardRef<HTMLInputElement, InputProps>(function Input(
  { startSlot, endSlot, wrapperClassName, ...props },
  ref,
) {
  return (
    <BaseInput
      {...props}
      ref={ref}
      render={(controlProps, state) => (
        <div
          className={wrapperClassName}
          data-slot="input"
          data-variant="outline"
          data-size="md"
          data-invalid={state.valid === false ? "" : undefined}
          data-disabled={state.disabled ? "" : undefined}
          data-readonly={props.readOnly ? "" : undefined}
          data-focused={state.focused ? "" : undefined}
          data-filled={state.filled ? "" : undefined}
        >
          {startSlot != null ? (
            <span data-slot="input-slot-start">{startSlot}</span>
          ) : null}
          <input
            {...controlProps}
            data-slot={[
              "input-control",
              (controlProps as { "data-slot"?: string })["data-slot"],
            ]
              .filter(Boolean)
              .join(" ")}
            aria-required={
              props.required || controlProps["aria-required"] || undefined
            }
          />
          {endSlot != null ? <span data-slot="input-slot-end">{endSlot}</span> : null}
        </div>
      )}
    />
  );
});

Input.displayName = "Input";
