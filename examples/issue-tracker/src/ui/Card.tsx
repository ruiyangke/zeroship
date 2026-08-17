import type { ComponentPropsWithoutRef } from "react";

import { cn } from "./cn";

export function Card({ className, ...props }: ComponentPropsWithoutRef<"div">) {
  return (
    <div
      {...props}
      className={cn(
        "relative isolate flex min-w-0 flex-col gap-3 overflow-hidden rounded-lg border border-line bg-surface p-4 font-sans text-ink no-underline shadow-none",
        className,
      )}
    />
  );
}
