import type { ComponentPropsWithoutRef, CSSProperties } from "react";

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
      className={`skeleton block h-2 w-full animate-pulse overflow-hidden rounded bg-line-subtle motion-reduce:animate-none${
        className ? ` ${className}` : ""
      }`}
      style={sizeStyle}
    />
  );
}
