import * as React from "react"
import { cn } from "@/lib/utils"

const Input = React.forwardRef<
  HTMLInputElement,
  React.InputHTMLAttributes<HTMLInputElement>
>(({ className, type, ...props }, ref) => {
  return (
    <input
      type={type}
      className={cn(
        "flex w-full font-mono text-[13px] bg-background border border-border px-3 py-2.5 text-foreground caret-primary outline-none transition-colors duration-150",
        "placeholder:text-muted-foreground",
        "focus:border-primary",
        "disabled:cursor-not-allowed disabled:opacity-50",
        className
      )}
      ref={ref}
      {...props}
    />
  )
})
Input.displayName = "Input"

export { Input }
