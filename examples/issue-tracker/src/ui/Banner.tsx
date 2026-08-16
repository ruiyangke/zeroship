import type { ComponentPropsWithoutRef, ReactNode } from "react";

export type BannerIntent = "info" | "success" | "warning" | "danger";

export interface BannerProps extends Omit<ComponentPropsWithoutRef<"div">, "title"> {
  intent?: BannerIntent;
  title: ReactNode;
  description?: ReactNode;
  live?: boolean;
}

const INTENT_CLASSES: Record<BannerIntent, string> = {
  info: "bg-info-soft [border-inline-start-color:var(--it-blue-500)]",
  success: "bg-success-soft [border-inline-start-color:var(--it-green-500)]",
  warning: "bg-warning-soft [border-inline-start-color:var(--it-amber-500)]",
  danger: "bg-danger-soft [border-inline-start-color:var(--it-red-500)]",
};

const ICON_CLASSES: Record<BannerIntent, string> = {
  info: "[color:var(--it-blue-700)]",
  success: "[color:var(--it-green-700)]",
  warning: "[color:var(--it-amber-700)]",
  danger: "[color:var(--it-red-700)]",
};

function CircleAlertIcon() {
  return (
    <svg
      aria-hidden="true"
      className="size-full"
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

export function Banner({
  intent = "info",
  title,
  description,
  live = false,
  className,
  children,
  ...props
}: BannerProps) {
  return (
    <div
      {...props}
      role={
        live
          ? intent === "warning" || intent === "danger"
            ? "alert"
            : "status"
          : undefined
      }
      data-intent={intent}
      className={`rounded border border-s-4 border-line text-base text-ink ${INTENT_CLASSES[intent]}${
        className ? ` ${className}` : ""
      }`}
    >
      <div className="flex min-h-8 flex-row items-start gap-3 px-3 py-2">
        <div
          aria-hidden="true"
          className={`inline-flex size-4 flex-none items-center justify-center ${ICON_CLASSES[intent]}`}
        >
          <CircleAlertIcon />
        </div>
        <div className="flex min-w-0 flex-1 flex-col gap-1">
          <div className="font-semibold leading-[var(--it-leading-snug)] text-ink">
            {title}
          </div>
          {description != null ? (
            <p className="text-sm text-ink-secondary">{description}</p>
          ) : null}
          {children}
        </div>
      </div>
    </div>
  );
}
