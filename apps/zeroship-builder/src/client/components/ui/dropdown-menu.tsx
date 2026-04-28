// Minimal shadcn-style wrapper around radix-ui/react-dropdown-menu.
// Only the bits we actually use in TopBar's account menu.

import * as React from "react"
import * as Dd from "@radix-ui/react-dropdown-menu"
import { cn } from "@/lib/utils"

export const DropdownMenu = Dd.Root
export const DropdownMenuTrigger = Dd.Trigger
export const DropdownMenuPortal = Dd.Portal
export const DropdownMenuGroup = Dd.Group
export const DropdownMenuSub = Dd.Sub

export const DropdownMenuContent = React.forwardRef<
  React.ElementRef<typeof Dd.Content>,
  React.ComponentPropsWithoutRef<typeof Dd.Content>
>(({ className, sideOffset = 4, ...props }, ref) => (
  <Dd.Portal>
    <Dd.Content
      ref={ref}
      sideOffset={sideOffset}
      className={cn(
        "z-50 min-w-[10rem] overflow-hidden border border-border bg-popover p-1 text-popover-foreground shadow-md",
        "data-[state=open]:animate-in data-[state=closed]:animate-out data-[state=closed]:fade-out-0 data-[state=open]:fade-in-0",
        className,
      )}
      {...props}
    />
  </Dd.Portal>
))
DropdownMenuContent.displayName = "DropdownMenuContent"

export const DropdownMenuItem = React.forwardRef<
  React.ElementRef<typeof Dd.Item>,
  React.ComponentPropsWithoutRef<typeof Dd.Item>
>(({ className, ...props }, ref) => (
  <Dd.Item
    ref={ref}
    className={cn(
      "relative flex cursor-pointer select-none items-center px-2 py-1.5 text-xs font-mono outline-none",
      "data-[highlighted]:bg-accent data-[highlighted]:text-accent-foreground",
      "data-[disabled]:pointer-events-none data-[disabled]:opacity-50",
      className,
    )}
    {...props}
  />
))
DropdownMenuItem.displayName = "DropdownMenuItem"

export const DropdownMenuLabel = React.forwardRef<
  React.ElementRef<typeof Dd.Label>,
  React.ComponentPropsWithoutRef<typeof Dd.Label>
>(({ className, ...props }, ref) => (
  <Dd.Label
    ref={ref}
    className={cn("px-2 py-1.5 text-[11px] font-mono", className)}
    {...props}
  />
))
DropdownMenuLabel.displayName = "DropdownMenuLabel"

export const DropdownMenuSeparator = React.forwardRef<
  React.ElementRef<typeof Dd.Separator>,
  React.ComponentPropsWithoutRef<typeof Dd.Separator>
>(({ className, ...props }, ref) => (
  <Dd.Separator
    ref={ref}
    className={cn("-mx-1 my-1 h-px bg-border", className)}
    {...props}
  />
))
DropdownMenuSeparator.displayName = "DropdownMenuSeparator"
