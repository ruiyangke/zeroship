import { useId, type ComponentPropsWithoutRef } from "react";

export interface SpinnerProps extends ComponentPropsWithoutRef<"span"> {
  size?: "sm" | "md" | "lg";
  label?: string;
}

const RING_SIZE = {
  sm: "size-3",
  md: "size-4",
  lg: "size-6",
} as const;

export function Spinner({
  size = "md",
  label = "Loading",
  className,
  "aria-label": ariaLabel,
  "aria-labelledby": ariaLabelledBy,
  ...props
}: SpinnerProps) {
  const labelId = useId();
  const consumerNamed = ariaLabel !== undefined || ariaLabelledBy !== undefined;

  return (
    <span
      {...props}
      aria-label={ariaLabel}
      aria-labelledby={consumerNamed ? ariaLabelledBy : labelId}
      role="status"
      className={`inline-flex flex-none items-center justify-center text-ink-secondary${
        className ? ` ${className}` : ""
      }`}
    >
      <span
        aria-hidden="true"
        className={`block animate-spin rounded-full border border-line-strong border-t-current motion-reduce:animate-none ${RING_SIZE[size]}`}
      />
      {consumerNamed ? null : (
        <span id={labelId} className="sr-only">
          {label}
        </span>
      )}
    </span>
  );
}
