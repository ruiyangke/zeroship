import type { ComponentPropsWithoutRef } from "react";

export function DashboardSection({
  className,
  ...props
}: ComponentPropsWithoutRef<"section">) {
  return (
    <section
      {...props}
      className={`dashboard-section mb-6 rounded-lg border border-line bg-surface p-4${
        className ? ` ${className}` : ""
      }`}
    />
  );
}
