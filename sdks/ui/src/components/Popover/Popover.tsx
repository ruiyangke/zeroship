import { isValidElement, type ReactElement, type ReactNode } from "react";
import { Popover as BasePopover } from "@base-ui/react/popover";
import clsx from "clsx";

export interface PopoverProps {
  trigger: ReactNode;
  children: ReactNode;
  title?: ReactNode;
  description?: ReactNode;
  open?: boolean;
  defaultOpen?: boolean;
  onOpenChange?: (open: boolean) => void;
  className?: string;
  side?: "top" | "right" | "bottom" | "left";
  align?: "start" | "center" | "end";
}

export function Popover({
  trigger,
  children,
  title,
  description,
  open,
  defaultOpen,
  onOpenChange,
  className,
  side = "bottom",
  align = "start",
}: PopoverProps) {
  const triggerRender = isValidElement(trigger) ? (trigger as ReactElement) : undefined;

  return (
    <BasePopover.Root
      open={open}
      defaultOpen={defaultOpen}
      onOpenChange={(next) => onOpenChange?.(next)}
    >
      <BasePopover.Trigger
        className={triggerRender ? undefined : "zs-popover__trigger"}
        render={triggerRender}
      >
        {triggerRender ? undefined : trigger}
      </BasePopover.Trigger>
      <BasePopover.Portal>
        <BasePopover.Positioner
          side={side}
          align={align}
          sideOffset={6}
          className="zs-popover__positioner"
        >
          <BasePopover.Popup className={clsx("zs-popover__popup", className)}>
            {(title || description) && (
              <div className="zs-popover__header">
                {title && <BasePopover.Title className="zs-popover__title">{title}</BasePopover.Title>}
                {description && (
                  <BasePopover.Description className="zs-popover__description">
                    {description}
                  </BasePopover.Description>
                )}
              </div>
            )}
            <div className="zs-popover__body">{children}</div>
          </BasePopover.Popup>
        </BasePopover.Positioner>
      </BasePopover.Portal>
    </BasePopover.Root>
  );
}

export const PopoverParts = BasePopover;
