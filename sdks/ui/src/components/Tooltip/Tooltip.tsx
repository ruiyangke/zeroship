import { isValidElement, type ReactElement, type ReactNode } from "react";
import { Tooltip as BaseTooltip } from "@base-ui/react/tooltip";

export interface TooltipProps {
  content: ReactNode;
  children: ReactNode;
  open?: boolean;
  defaultOpen?: boolean;
  onOpenChange?: (open: boolean) => void;
  side?: "top" | "right" | "bottom" | "left";
  align?: "start" | "center" | "end";
}

export function Tooltip({
  content,
  children,
  open,
  defaultOpen,
  onOpenChange,
  side = "top",
  align = "center",
}: TooltipProps) {
  const triggerRender = isValidElement(children) ? (children as ReactElement) : undefined;

  return (
    <BaseTooltip.Provider delay={160} closeDelay={80}>
      <BaseTooltip.Root
        open={open}
        defaultOpen={defaultOpen}
        onOpenChange={(next) => onOpenChange?.(next)}
      >
        <BaseTooltip.Trigger
          className={triggerRender ? undefined : "zs-tooltip__trigger"}
          render={triggerRender}
        >
          {triggerRender ? undefined : children}
        </BaseTooltip.Trigger>
        <BaseTooltip.Portal>
          <BaseTooltip.Positioner
            side={side}
            align={align}
            sideOffset={6}
            className="zs-tooltip__positioner"
          >
            <BaseTooltip.Popup className="zs-tooltip__popup">{content}</BaseTooltip.Popup>
          </BaseTooltip.Positioner>
        </BaseTooltip.Portal>
      </BaseTooltip.Root>
    </BaseTooltip.Provider>
  );
}

export const TooltipParts = BaseTooltip;
