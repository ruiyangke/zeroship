import * as React from "react"
import { Slot } from "@radix-ui/react-slot"
import { cva, type VariantProps } from "class-variance-authority"
import { cn } from "@/lib/utils"

const buttonVariants = cva(
  "inline-flex items-center justify-center whitespace-nowrap font-mono text-xs font-medium uppercase tracking-widest transition-all duration-150 cursor-pointer disabled:pointer-events-none disabled:opacity-40",
  {
    variants: {
      variant: {
        default:
          "border border-border bg-card text-foreground hover:border-muted-foreground hover:bg-white/5",
        primary:
          "border border-primary text-primary hover:bg-primary/10",
        destructive:
          "border border-destructive text-destructive hover:bg-destructive/10",
        outline:
          "border border-border bg-transparent text-foreground hover:border-muted-foreground hover:bg-white/5",
        ghost:
          "border border-transparent text-foreground hover:bg-white/5",
      },
      size: {
        default: "h-9 px-4 py-2",
        sm: "h-7 px-3 text-[11px]",
        lg: "h-11 px-6",
      },
    },
    defaultVariants: {
      variant: "default",
      size: "default",
    },
  }
)

interface ButtonProps
  extends React.ButtonHTMLAttributes<HTMLButtonElement>,
    VariantProps<typeof buttonVariants> {
  asChild?: boolean
}

const Button = React.forwardRef<HTMLButtonElement, ButtonProps>(
  ({ className, variant, size, asChild = false, ...props }, ref) => {
    const Comp = asChild ? Slot : "button"
    return (
      <Comp
        className={cn(buttonVariants({ variant, size, className }))}
        ref={ref}
        {...props}
      />
    )
  }
)
Button.displayName = "Button"

export { Button, buttonVariants }
export type { ButtonProps }
