import { NumberField as BaseNumberField } from "@base-ui/react/number-field";
import {
  forwardRef,
  useState,
  type FocusEvent as ReactFocusEvent,
} from "react";

export type NumberFieldSize = "sm" | "md" | "lg";
export type NumberFieldVariant = "default" | "outline";

export interface NumberFieldProps extends Omit<
  BaseNumberField.Root.Props,
  "className" | "render"
> {
  size?: NumberFieldSize;
  variant?: NumberFieldVariant;
  showScrub?: boolean;
  placeholder?: string;
  className?: string;
  "aria-label"?: string;
  "aria-labelledby"?: string;
  "aria-describedby"?: string;
  "data-testid"?: string;
}

function StepperIcon({ kind }: { kind: "minus" | "plus" }) {
  return (
    <svg
      aria-hidden="true"
      data-size="sm"
      data-slot="icon"
      fill="none"
      focusable="false"
      height="24"
      stroke="currentColor"
      strokeLinecap="round"
      strokeLinejoin="round"
      strokeWidth="2"
      viewBox="0 0 24 24"
      width="24"
    >
      <path d="M5 12h14" />
      {kind === "plus" ? <path d="M12 5v14" /> : null}
    </svg>
  );
}

export const NumberField = forwardRef<HTMLDivElement, NumberFieldProps>(
  function NumberField(
    {
      size = "md",
      variant = "default",
      showScrub = false,
      placeholder,
      className,
      "aria-label": ariaLabel,
      "aria-labelledby": ariaLabelledBy,
      "aria-describedby": ariaDescribedBy,
      "data-testid": dataTestId,
      ...props
    },
    ref,
  ) {
    const [bareFocused, setBareFocused] = useState(false);

    return (
      <BaseNumberField.Root
        {...props}
        ref={ref}
        render={(rootProps, state) => {
          const handleFocus = (event: ReactFocusEvent<HTMLDivElement>) => {
            rootProps.onFocus?.(event);
            setBareFocused(true);
          };
          const handleBlur = (event: ReactFocusEvent<HTMLDivElement>) => {
            rootProps.onBlur?.(event);
            const next = event.relatedTarget as Node | null;
            if (!next || !event.currentTarget.contains(next)) {
              setBareFocused(false);
            }
          };

          return (
            <div
              {...rootProps}
              className={[rootProps.className, className]
                .filter(Boolean)
                .join(" ") || undefined}
              data-slot="number-field"
              data-size={size}
              data-variant={variant}
              data-focused={state.focused || bareFocused ? "" : undefined}
              data-filled={state.filled ? "" : undefined}
              data-disabled={state.disabled ? "" : undefined}
              data-readonly={state.readOnly ? "" : undefined}
              data-invalid={state.valid === false ? "" : undefined}
              onFocus={handleFocus}
              onBlur={handleBlur}
            />
          );
        }}
      >
        {showScrub ? (
          <BaseNumberField.ScrubArea data-slot="number-field-scrub">
            <BaseNumberField.ScrubAreaCursor data-slot="number-field-scrub-cursor">
              <svg
                aria-hidden="true"
                focusable="false"
                height="14"
                viewBox="0 0 26 14"
                width="26"
              >
                <path
                  d="M0 7l5-5v3h16V2l5 5-5 5V9H5v3z"
                  fill="currentColor"
                />
              </svg>
            </BaseNumberField.ScrubAreaCursor>
          </BaseNumberField.ScrubArea>
        ) : null}
        <BaseNumberField.Group data-slot="number-field-group">
          <BaseNumberField.Decrement
            aria-label="Decrement"
            data-slot="number-field-step-dec"
          >
            <StepperIcon kind="minus" />
          </BaseNumberField.Decrement>
          <BaseNumberField.Input
            data-slot="number-field-input"
            data-testid={dataTestId}
            placeholder={placeholder}
            render={(inputProps) => {
              const describedBy = [
                inputProps["aria-describedby"],
                ariaDescribedBy,
              ]
                .filter(Boolean)
                .join(" ") || undefined;

              return (
                <input
                  {...inputProps}
                  {...(ariaLabel === undefined
                    ? {}
                    : { "aria-label": ariaLabel })}
                  {...(ariaLabelledBy === undefined
                    ? {}
                    : { "aria-labelledby": ariaLabelledBy })}
                  aria-describedby={describedBy}
                />
              );
            }}
          />
          <BaseNumberField.Increment
            aria-label="Increment"
            data-slot="number-field-step-inc"
          >
            <StepperIcon kind="plus" />
          </BaseNumberField.Increment>
        </BaseNumberField.Group>
      </BaseNumberField.Root>
    );
  },
);

NumberField.displayName = "NumberField";
