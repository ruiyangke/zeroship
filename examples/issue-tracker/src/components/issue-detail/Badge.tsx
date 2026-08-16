import type { ReactNode } from "react";

export function Badge({
  tone = "neutral",
  children,
}: {
  tone?: "neutral" | "danger" | "info" | "muted";
  children: ReactNode;
}) {
  return (
    <span
      className={`inline-flex items-center whitespace-nowrap rounded-full px-2 py-1 text-sm font-bold tracking-[0.01em] ${
        tone === "danger"
          ? "bg-danger-soft text-danger"
          : tone === "info"
            ? "bg-info-soft text-info"
            : tone === "muted"
              ? "bg-surface-sunken text-ink-muted"
              : "bg-surface-sunken text-ink-secondary"
      }`}
    >
      {children}
    </span>
  );
}
