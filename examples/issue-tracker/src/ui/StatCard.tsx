import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type ReactNode,
} from "react";

export interface StatCardProps extends ComponentPropsWithoutRef<"div"> {
  label: ReactNode;
  value: ReactNode;
}

const ROOT_CLASSES =
  "relative isolate flex min-w-0 flex-col gap-3 overflow-hidden rounded-lg border border-line-strong bg-surface p-4 font-sans text-ink no-underline shadow-none";
const LABEL_CLASSES =
  "truncate text-xs text-ink-muted [font-weight:var(--it-weight-medium)] [letter-spacing:var(--it-tracking-wide)]";
const VALUE_CLASSES =
  "truncate font-mono text-2xl text-ink tabular-nums [font-weight:var(--it-weight-semibold)] [line-height:var(--it-leading-tight)]";

export const StatCard = forwardRef<HTMLDivElement, StatCardProps>(
  function StatCard({ label, value, className, ...props }, ref) {
    return (
      <div
        {...props}
        ref={ref}
        data-slot="card stat-card"
        data-variant="elevated"
        data-size="md"
        className={`${ROOT_CLASSES}${className ? ` ${className}` : ""}`}
      >
        <div
          data-slot="stat-card-head"
          className="flex items-center justify-between gap-2"
        >
          <div data-slot="stat-card-label" className={LABEL_CLASSES}>
            {label}
          </div>
        </div>
        <div data-slot="stat-card-value" className={VALUE_CLASSES}>
          {value}
        </div>
      </div>
    );
  },
);
StatCard.displayName = "StatCard";
