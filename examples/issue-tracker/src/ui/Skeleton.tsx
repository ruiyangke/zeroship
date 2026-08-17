import type { ComponentPropsWithoutRef, CSSProperties } from "react";

import { cn } from "./cn";

export interface SkeletonProps extends ComponentPropsWithoutRef<"div"> {
  width?: string;
}

export function Skeleton({ width, className, style, ...props }: SkeletonProps) {
  const sizeStyle: CSSProperties = {
    ...(width === undefined ? null : { inlineSize: width }),
    ...style,
  };

  return (
    <div
      {...props}
      aria-hidden="true"
      className={cn(
        "skeleton block h-2 w-full animate-pulse overflow-hidden rounded bg-line-subtle motion-reduce:animate-none",
        className,
      )}
      style={sizeStyle}
    />
  );
}
