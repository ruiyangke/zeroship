import * as React from "react"
import { cva, type VariantProps } from "class-variance-authority"
import { cn } from "@/lib/utils"

const badgeVariants = cva(
  "inline-flex items-center gap-1.5 text-[11px] font-medium uppercase tracking-[0.08em]",
  {
    variants: {
      variant: {
        default: "text-foreground",
        primary: "text-primary",
        destructive: "text-destructive",
        warning: "text-warning",
        muted: "text-muted-foreground",
        running: "text-primary",
        idle: "text-warning",
        stopped: "text-muted-foreground",
      },
    },
    defaultVariants: {
      variant: "default",
    },
  }
)

interface BadgeProps
  extends React.HTMLAttributes<HTMLSpanElement>,
    VariantProps<typeof badgeVariants> {}

const Badge = React.forwardRef<HTMLSpanElement, BadgeProps>(
  ({ className, variant, ...props }, ref) => {
    return (
      <span
        ref={ref}
        className={cn(badgeVariants({ variant }), className)}
        {...props}
      />
    )
  }
)
Badge.displayName = "Badge"

export { Badge, badgeVariants }
export type { BadgeProps }
