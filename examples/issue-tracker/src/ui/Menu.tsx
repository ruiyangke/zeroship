import { Menu as BaseMenu } from "@base-ui/react/menu";
import {
  forwardRef,
  type ReactNode,
  type Ref,
} from "react";

export type MenuProps = BaseMenu.Root.Props;

const MenuRoot = ({ modal = false, ...props }: MenuProps) => {
  return <BaseMenu.Root {...props} modal={modal} />;
};

const MenuTrigger = forwardRef<HTMLElement, BaseMenu.Trigger.Props>(
  function MenuTrigger(props, ref) {
    return (
      <BaseMenu.Trigger
        {...props}
        ref={ref as Ref<HTMLButtonElement>}
      />
    );
  },
);

const MenuPortal = (props: BaseMenu.Portal.Props) => {
  return <BaseMenu.Portal {...props} />;
};

export interface MenuPopupProps extends BaseMenu.Popup.Props {
  side?: BaseMenu.Positioner.Props["side"];
  align?: BaseMenu.Positioner.Props["align"];
  sideOffset?: number;
  "data-slot"?: string;
}

const MenuPopup = forwardRef<HTMLDivElement, MenuPopupProps>(
  function MenuPopup(
    {
      side = "bottom",
      align = "start",
      sideOffset = 6,
      children,
      "data-slot": dataSlot,
      ...props
    },
    ref,
  ) {
    return (
      <BaseMenu.Positioner
        align={align}
        side={side}
        sideOffset={sideOffset}
        data-slot="menu-positioner"
      >
        <BaseMenu.Popup
          {...props}
          ref={ref}
          data-slot={["menu-popup", dataSlot].filter(Boolean).join(" ")}
        >
          {children}
        </BaseMenu.Popup>
      </BaseMenu.Positioner>
    );
  },
);

export interface MenuItemProps extends BaseMenu.Item.Props {
  shortcut?: ReactNode;
  "data-slot"?: string;
}

const MenuItem = forwardRef<HTMLElement, MenuItemProps>(function MenuItem(
  {
    children,
    shortcut,
    label,
    "data-slot": dataSlot,
    ...props
  },
  ref,
) {
  const textNavigationLabel =
    label === undefined && shortcut !== undefined && typeof children === "string"
      ? children
      : label;

  return (
    <BaseMenu.Item
      {...props}
      ref={ref}
      label={textNavigationLabel}
      data-slot={["menu-item", dataSlot].filter(Boolean).join(" ")}
    >
      <span aria-hidden="true" data-slot="menu-item-indicator" />
      <span data-slot="menu-item-text">{children}</span>
      {shortcut !== undefined ? (
        <span aria-hidden="true" data-slot="menu-item-shortcut">
          {shortcut}
        </span>
      ) : null}
    </BaseMenu.Item>
  );
});

const MenuGroup = forwardRef<HTMLDivElement, BaseMenu.Group.Props>(
  function MenuGroup(props, ref) {
    return <BaseMenu.Group {...props} ref={ref} data-slot="menu-group" />;
  },
);

const MenuGroupLabel = forwardRef<
  HTMLDivElement,
  BaseMenu.GroupLabel.Props
>(function MenuGroupLabel(props, ref) {
  return (
    <BaseMenu.GroupLabel
      {...props}
      ref={ref}
      data-slot="menu-group-label"
    />
  );
});

MenuTrigger.displayName = "Menu.Trigger";
MenuPopup.displayName = "Menu.Popup";
MenuItem.displayName = "Menu.Item";
MenuGroup.displayName = "Menu.Group";
MenuGroupLabel.displayName = "Menu.GroupLabel";

export const Menu = Object.assign(MenuRoot, {
  Trigger: MenuTrigger,
  Portal: MenuPortal,
  Popup: MenuPopup,
  Item: MenuItem,
  Group: MenuGroup,
  GroupLabel: MenuGroupLabel,
});
