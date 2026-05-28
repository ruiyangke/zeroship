/*
 * ContextMenu — right-click / long-press anchored menu.
 *
 *   <ContextMenu>
 *     <ContextMenu.Trigger>
 *       <Card>Right-click me</Card>
 *     </ContextMenu.Trigger>
 *     <ContextMenu.Portal>
 *       <ContextMenu.Popup>
 *         <ContextMenu.Item>Open</ContextMenu.Item>
 *         <ContextMenu.Item>Rename…</ContextMenu.Item>
 *         <ContextMenu.Separator />
 *         <ContextMenu.Item>Delete</ContextMenu.Item>
 *       </ContextMenu.Popup>
 *     </ContextMenu.Portal>
 *   </ContextMenu>
 *
 * Shape decisions:
 *   - The Root is a thin pass-through onto Base UI's ContextMenuRoot,
 *     which internally renders a Menu with `openOnPointer={contextmenu}`
 *     semantics. The Trigger is a `<div>` that listens for `contextmenu`
 *     (and `pointerdown`-long-press on touch) to open the popup at the
 *     pointer coords. Same anchored-popup shape as Menu — same Backdrop /
 *     Portal / Popup / Item / Group / GroupLabel / Separator /
 *     CheckboxItem / RadioGroup / RadioItem / LinkItem / Submenu / Arrow
 *     subparts are exposed by reference.
 *   - The subparts are re-exports — NOT re-wrapped Base UI primitives.
 *     A re-wrap would mean two source files and two CSS surfaces drift
 *     apart over time; the re-export keeps the visual contract in one
 *     place (Menu.css owns the styles). The same `.zs-menu-*` class
 *     names paint ContextMenu popups, so the forced-colors mirror,
 *     hit-target floor, and indicator gutter all carry over for free.
 *   - Backdrop opt-in same as Menu — ContextMenu defaults to popover-
 *     feel; the modal-feel Backdrop subpart is available but never
 *     auto-mounted.
 *   - Trigger renders a `<div>` natively. `asChild` is NOT needed at
 *     this layer — the consumer's content (a card, a region, an image)
 *     lives INSIDE the trigger div, and the trigger's role is to listen
 *     for the contextmenu event on its bounding box.
 */
import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type ComponentPropsWithRef,
  type ReactNode,
  type Ref,
} from "react";
import { ContextMenu as BaseContextMenu } from "@base-ui/react/context-menu";
import { composeBaseClass } from "../_classnames";
import {
  MenuPortal,
  MenuBackdrop,
  MenuPopup,
  MenuItem,
  MenuGroup,
  MenuGroupLabel,
  MenuSeparator,
  MenuCheckboxItem,
  MenuRadioGroup,
  MenuRadioItem,
  MenuLinkItem,
  MenuSubmenu,
  MenuArrow,
} from "../Menu/Menu";

/* ─── Root ─────────────────────────────────────────────────────────── */

type BaseRootProps = ComponentPropsWithRef<typeof BaseContextMenu.Root>;

export interface ContextMenuProps extends BaseRootProps {
  children?: ReactNode;
}

function ContextMenuRoot({ children, ...rest }: ContextMenuProps) {
  return <BaseContextMenu.Root {...rest}>{children}</BaseContextMenu.Root>;
}
ContextMenuRoot.displayName = "ContextMenu";

/* ─── Trigger ──────────────────────────────────────────────────────── *
 *
 * Renders a `<div>` that listens for `contextmenu` (and `pointerdown`-
 * long-press on touch). The consumer's content lives inside the
 * trigger div. We stamp a class for an explicit hit area + focus ring
 * so right-click targets are obvious; the consumer's content sits
 * INSIDE without any layout pressure. */

type BaseTriggerProps = ComponentPropsWithoutRef<typeof BaseContextMenu.Trigger>;
export type ContextMenuTriggerProps = BaseTriggerProps;

const ContextMenuTrigger = forwardRef<HTMLDivElement, ContextMenuTriggerProps>(
  function ContextMenuTrigger({ className, ...rest }, ref) {
    return (
      <BaseContextMenu.Trigger
        ref={ref}
        className={composeBaseClass("zs-contextmenu-trigger", className)}
        {...rest}
      />
    );
  },
);
ContextMenuTrigger.displayName = "ContextMenu.Trigger";

/* ─── public namespace ─────────────────────────────────────────────── *
 *
 * Subparts mirror Menu's surface by reference. The same `zs-menu-*`
 * CSS classes paint both surfaces; ContextMenu only adds a trigger
 * subpart and a thin Root. */

export type ContextMenuComponent = typeof ContextMenuRoot & {
  Trigger: typeof ContextMenuTrigger;
  Portal: typeof MenuPortal;
  Backdrop: typeof MenuBackdrop;
  Popup: typeof MenuPopup;
  Item: typeof MenuItem;
  Group: typeof MenuGroup;
  GroupLabel: typeof MenuGroupLabel;
  Separator: typeof MenuSeparator;
  CheckboxItem: typeof MenuCheckboxItem;
  RadioGroup: typeof MenuRadioGroup;
  RadioItem: typeof MenuRadioItem;
  LinkItem: typeof MenuLinkItem;
  Submenu: typeof MenuSubmenu;
  Arrow: typeof MenuArrow;
};

export const ContextMenu = ContextMenuRoot as ContextMenuComponent;
ContextMenu.Trigger = ContextMenuTrigger;
ContextMenu.Portal = MenuPortal;
ContextMenu.Backdrop = MenuBackdrop;
ContextMenu.Popup = MenuPopup;
ContextMenu.Item = MenuItem;
ContextMenu.Group = MenuGroup;
ContextMenu.GroupLabel = MenuGroupLabel;
ContextMenu.Separator = MenuSeparator;
ContextMenu.CheckboxItem = MenuCheckboxItem;
ContextMenu.RadioGroup = MenuRadioGroup;
ContextMenu.RadioItem = MenuRadioItem;
ContextMenu.LinkItem = MenuLinkItem;
ContextMenu.Submenu = MenuSubmenu;
ContextMenu.Arrow = MenuArrow;
