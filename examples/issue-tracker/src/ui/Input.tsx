import { Input as BaseInput } from "@base-ui/react/input";
import { forwardRef } from "react";

export type InputProps = Omit<BaseInput.Props, "render">;

export const Input = forwardRef<HTMLInputElement, InputProps>(function Input(
  props,
  ref,
) {
  return (
    <BaseInput
      {...props}
      ref={ref}
      render={(controlProps, state) => (
        <div
          data-slot="input"
          data-variant="outline"
          data-size="md"
          data-invalid={state.valid === false ? "" : undefined}
          data-disabled={state.disabled ? "" : undefined}
          data-readonly={props.readOnly ? "" : undefined}
          data-focused={state.focused ? "" : undefined}
          data-filled={state.filled ? "" : undefined}
        >
          <input
            {...controlProps}
            data-slot="input-control"
            aria-required={
              props.required || controlProps["aria-required"] || undefined
            }
          />
        </div>
      )}
    />
  );
});

Input.displayName = "Input";
