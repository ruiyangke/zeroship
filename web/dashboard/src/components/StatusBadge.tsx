import { Badge } from "@/components/ui/badge"
import { cn } from "@/lib/utils"

interface StatusBadgeProps {
  status: "running" | "idle" | "stopped";
}

export default function StatusBadge({ status }: StatusBadgeProps) {
  return (
    <Badge variant={status}>
      <span
        className={cn(
          "inline-block h-1.5 w-1.5 rounded-full",
          status === "running" && "bg-primary shadow-[0_0_6px_var(--color-primary)] animate-[pulse_2s_ease-in-out_infinite]",
          status === "idle" && "bg-warning shadow-[0_0_4px_var(--color-warning)]",
          status === "stopped" && "bg-muted-foreground"
        )}
      />
      {status}
    </Badge>
  )
}
