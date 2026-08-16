import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type ReactNode,
} from "react";

export type ErrorStateIntent = "warning" | "danger";

export interface ErrorStateProps extends Omit<
  ComponentPropsWithoutRef<"div">,
  "title"
> {
  intent?: ErrorStateIntent;
  title?: ReactNode;
  description?: ReactNode;
  live?: boolean;
}

const COLUMN_CLASSES =
  "flex min-w-0 max-w-full flex-col items-center gap-[var(--it-space-3)]";
const ACTIONS_CLASSES =
  "flex flex-wrap items-center justify-center gap-[var(--it-space-2)]";

function CircleAlertIcon() {
  return (
    <svg
      aria-hidden="true"
      className="size-[var(--it-control-h-sm)]"
      fill="none"
      focusable="false"
      stroke="currentColor"
      strokeLinecap="round"
      strokeLinejoin="round"
      strokeWidth="2"
      viewBox="0 0 24 24"
    >
      <circle cx="12" cy="12" r="10" />
      <line x1="12" x2="12" y1="8" y2="12" />
      <line x1="12" x2="12.01" y1="16" y2="16" />
    </svg>
  );
}

const ErrorStateActions = forwardRef<
  HTMLDivElement,
  ComponentPropsWithoutRef<"div">
>(function ErrorStateActions({ className, ...props }, ref) {
  return (
    <div
      {...props}
      ref={ref}
      className={`${ACTIONS_CLASSES}${className ? ` ${className}` : ""}`}
    />
  );
});

ErrorStateActions.displayName = "ErrorState.Actions";

const ErrorStateRoot = forwardRef<HTMLDivElement, ErrorStateProps>(
  function ErrorState(
    {
      intent = "danger",
      title,
      description,
      live = false,
      className,
      children,
      ...props
    },
    ref,
  ) {
    return (
      <div
        {...props}
        ref={ref}
        role={live ? "alert" : undefined}
        data-intent={intent}
        className={`grid min-h-[var(--it-row-h)] place-items-center px-[var(--it-space-4)] py-[var(--it-space-5)] text-center text-ink-secondary${
          className ? ` ${className}` : ""
        }`}
      >
        <div className={COLUMN_CLASSES}>
          <div
            aria-hidden="true"
            className={`inline-flex size-[var(--it-control-h-lg)] items-center justify-center ${
              intent === "warning"
                ? "[color:var(--it-amber-700)]"
                : "[color:var(--it-red-700)]"
            }`}
          >
            <CircleAlertIcon />
          </div>
          {title != null ? (
            <h2 className="m-0 text-lg leading-[var(--it-leading-tight)] [font-weight:var(--it-weight-semibold)] text-ink">
              {title}
            </h2>
          ) : null}
          {description != null ? (
            <p className="m-0 text-sm text-ink-secondary">{description}</p>
          ) : null}
          {children}
        </div>
      </div>
    );
  },
);

ErrorStateRoot.displayName = "ErrorState";

type ErrorStateComponent = typeof ErrorStateRoot & {
  Actions: typeof ErrorStateActions;
};

export const ErrorState = ErrorStateRoot as ErrorStateComponent;
ErrorState.Actions = ErrorStateActions;
