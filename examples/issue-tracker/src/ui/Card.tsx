import type { ComponentPropsWithoutRef } from "react";

export function Card({ className, ...props }: ComponentPropsWithoutRef<"div">) {
  return (
    <div
      {...props}
      className={`relative isolate flex min-w-0 flex-col gap-3 overflow-hidden rounded-lg border border-line bg-surface p-4 font-sans text-ink no-underline shadow-none${
        className ? ` ${className}` : ""
      }`}
    />
  );
}
