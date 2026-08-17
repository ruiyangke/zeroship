import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type ReactNode,
} from "react";

import { cn } from "./cn";

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
  "flex min-w-0 max-w-full flex-col items-center gap-3";
const ACTIONS_CLASSES =
  "flex flex-wrap items-center justify-center gap-2";

function CircleAlertIcon() {
  return (
    <svg
      aria-hidden="true"
      className="size-6"
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
      className={cn(ACTIONS_CLASSES, className)}
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
        className={cn(
          "grid min-h-8 place-items-center px-4 py-6 text-center text-ink-secondary",
          className,
        )}
      >
        <div className={COLUMN_CLASSES}>
          <div
            aria-hidden="true"
            className={cn(
              "inline-flex size-8 items-center justify-center",
              intent === "warning"
                ? "text-warning-strong"
                : "text-danger-strong",
            )}
          >
            <CircleAlertIcon />
          </div>
          {title != null ? (
            <h2 className="m-0 text-lg leading-tight font-semibold text-ink">
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
