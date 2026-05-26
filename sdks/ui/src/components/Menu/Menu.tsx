import { isValidElement, type ReactElement, type ReactNode } from "react";
import { Menu as BaseMenu } from "@base-ui/react/menu";
import clsx from "clsx";

export interface MenuItemOption {
  label?: ReactNode;
  value?: string;
  disabled?: boolean;
  onSelect?: () => void;
  separator?: boolean;
}

export interface MenuProps {
  trigger: ReactNode;
  items?: ReadonlyArray<MenuItemOption>;
  children?: ReactNode;
  open?: boolean;
  defaultOpen?: boolean;
  onOpenChange?: (open: boolean) => void;
  className?: string;
  side?: "top" | "right" | "bottom" | "left";
  align?: "start" | "center" | "end";
}

export function Menu({
  trigger,
  items,
  children,
  open,
  defaultOpen,
  onOpenChange,
  className,
  side = "bottom",
  align = "start",
}: MenuProps) {
  const triggerRender = isValidElement(trigger) ? (trigger as ReactElement) : undefined;

  return (
    <BaseMenu.Root
      open={open}
      defaultOpen={defaultOpen}
      onOpenChange={(next) => onOpenChange?.(next)}
    >
      <BaseMenu.Trigger
        className={triggerRender ? undefined : "zs-menu__trigger"}
        render={triggerRender}
      >
        {triggerRender ? undefined : trigger}
      </BaseMenu.Trigger>
      <BaseMenu.Portal>
        <BaseMenu.Positioner
          side={side}
          align={align}
          sideOffset={4}
          className="zs-menu__positioner"
        >
          <BaseMenu.Popup className={clsx("zs-menu__popup", className)}>
            {items?.map((item, index) =>
              item.separator ? (
                <BaseMenu.Separator key={item.value ?? index} className="zs-menu__separator" />
              ) : (
                <BaseMenu.Item
                  key={item.value ?? index}
                  disabled={item.disabled}
                  onClick={item.onSelect}
                  className="zs-menu__item"
                >
                  {item.label}
                </BaseMenu.Item>
              ),
            )}
            {children}
          </BaseMenu.Popup>
        </BaseMenu.Positioner>
      </BaseMenu.Portal>
    </BaseMenu.Root>
  );
}

export const MenuParts = BaseMenu;
