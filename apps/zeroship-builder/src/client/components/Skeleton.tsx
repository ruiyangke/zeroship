import { cn } from "../lib/utils";

export function Skeleton({ className }: { className?: string }) {
  return (
    <div
      aria-hidden="true"
      className={cn(
        "bg-paper-3 rounded animate-pulse",
        className,
      )}
    />
  );
}
