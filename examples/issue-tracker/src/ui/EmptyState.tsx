import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type ReactNode,
} from "react";

export interface EmptyStateProps extends Omit<
  ComponentPropsWithoutRef<"div">,
  "title"
> {
  icon?: ReactNode;
  title?: ReactNode;
  description?: ReactNode;
  action?: ReactNode;
}

const COLUMN_CLASSES =
  "flex min-w-0 max-w-full flex-col items-center gap-[var(--it-space-3)]";
const ACTIONS_CLASSES =
  "flex flex-wrap items-center justify-center gap-[var(--it-space-2)]";

export const EmptyState = forwardRef<HTMLDivElement, EmptyStateProps>(
  function EmptyState(
    { icon, title, description, action, className, children, ...props },
    ref,
  ) {
    return (
      <div
        {...props}
        ref={ref}
        className={`grid min-h-[var(--it-row-h)] place-items-center px-[var(--it-space-4)] py-[var(--it-space-5)] text-center text-ink-secondary${
          className ? ` ${className}` : ""
        }`}
      >
        <div className={COLUMN_CLASSES}>
          {icon != null ? (
            <div
              aria-hidden="true"
              className="inline-flex size-[var(--it-control-h-lg)] items-center justify-center text-ink-muted [&>svg]:size-[var(--it-control-h-sm)]"
            >
              {icon}
            </div>
          ) : null}
          {title != null ? (
            <h2 className="m-0 text-lg leading-[var(--it-leading-tight)] [font-weight:var(--it-weight-semibold)] text-ink">
              {title}
            </h2>
          ) : null}
          {description != null ? (
            <p className="m-0 text-sm text-ink-secondary">{description}</p>
          ) : null}
          {action != null ? <div className={ACTIONS_CLASSES}>{action}</div> : null}
          {children}
        </div>
      </div>
    );
  },
);

EmptyState.displayName = "EmptyState";
