import type { ComponentPropsWithoutRef } from "react";

export interface TagProps extends ComponentPropsWithoutRef<"span"> {
  size?: "sm" | "md";
}

export function Tag({ size = "md", className, children, ...props }: TagProps) {
  return (
    <span
      {...props}
      className={`inline-flex min-w-0 items-center gap-1 whitespace-nowrap rounded border border-line bg-surface-sunken font-sans font-medium leading-tight text-ink-secondary ${
        size === "sm" ? "h-6 px-1 text-xs" : "h-7 px-2 text-sm"
      }${className ? ` ${className}` : ""}`}
    >
      <span className="min-w-0 overflow-hidden text-ellipsis">{children}</span>
    </span>
  );
}
