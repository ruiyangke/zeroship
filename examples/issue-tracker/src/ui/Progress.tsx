import { Progress as BaseProgress } from "@base-ui/react/progress";
import { forwardRef, type ReactNode } from "react";

export type ProgressSize = "sm" | "md" | "lg";

export interface ProgressProps extends Omit<
  BaseProgress.Root.Props,
  "className" | "render"
> {
  size?: ProgressSize;
  showValue?: boolean;
  label?: ReactNode;
  className?: string;
}

export const Progress = forwardRef<HTMLDivElement, ProgressProps>(
  function Progress(
    { size = "md", showValue = false, label, className, value, ...props },
    ref,
  ) {
    return (
      <BaseProgress.Root
        {...props}
        ref={ref}
        value={value}
        render={(rootProps, state) => (
          <div
            {...rootProps}
            className={[rootProps.className, className]
              .filter(Boolean)
              .join(" ") || undefined}
            data-slot="progress"
            data-size={size}
            data-status={state.status}
          >
            {label != null || showValue ? (
              <div data-slot="progress-header">
                {label != null ? (
                  <BaseProgress.Label data-slot="progress-label">
                    {label}
                  </BaseProgress.Label>
                ) : null}
                {showValue ? (
                  <BaseProgress.Value data-slot="progress-value" />
                ) : null}
              </div>
            ) : null}
            <BaseProgress.Track data-slot="progress-track">
              <BaseProgress.Indicator data-slot="progress-indicator" />
            </BaseProgress.Track>
          </div>
        )}
      />
    );
  },
);

Progress.displayName = "Progress";
