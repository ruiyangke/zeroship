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
  "flex min-w-0 max-w-full flex-col items-center gap-3";
const ACTIONS_CLASSES =
  "flex flex-wrap items-center justify-center gap-2";

export const EmptyState = forwardRef<HTMLDivElement, EmptyStateProps>(
  function EmptyState(
    { icon, title, description, action, className, children, ...props },
    ref,
  ) {
    return (
      <div
        {...props}
        ref={ref}
        className={`grid min-h-8 place-items-center px-4 py-6 text-center text-ink-secondary${
          className ? ` ${className}` : ""
        }`}
      >
        <div className={COLUMN_CLASSES}>
          {icon != null ? (
            <div
              aria-hidden="true"
              className="inline-flex size-8 items-center justify-center text-ink-muted [&>svg]:size-6"
            >
              {icon}
            </div>
          ) : null}
          {title != null ? (
            <h2 className="m-0 text-lg leading-tight font-semibold text-ink">
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
