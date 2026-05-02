import { cn } from "../lib/utils";

export function Spinner({ size = 16, className }: { size?: number; className?: string }) {
  return (
    <span
      role="status"
      aria-label="Loading"
      style={{ width: size, height: size }}
      className={cn(
        "inline-block rounded-full border-2 border-current border-t-transparent spin",
        className,
      )}
    />
  );
}
