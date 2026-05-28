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
import { Menu as BaseMenu } from "@base-ui/react/menu";
import { composeBaseClass } from "../_classnames";
import {
  MenuPortal,
  MenuBackdrop,
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
export type ContextMenuTriggerProps = BaseTriggerProps & {
  /** Disable the trigger; drops it out of the tab sequence. */
  disabled?: boolean;
};

const ContextMenuTrigger = forwardRef<HTMLDivElement, ContextMenuTriggerProps>(
  function ContextMenuTrigger(
    { className, tabIndex, disabled, ...rest },
    ref,
  ) {
    // Base UI renders the trigger as a plain `<div>` with no
    // tabIndex/role, so Tab skips it and the Shift+F10 keyboard-open
    // path documented on the stories is unreachable. Stamp a default
    // tabIndex so keyboard users can focus the trigger and fire the
    // contextmenu via Shift+F10. Caller-supplied tabIndex wins. */
    const resolvedTabIndex =
      tabIndex !== undefined ? tabIndex : disabled ? -1 : 0;
    return (
      <BaseContextMenu.Trigger
        ref={ref}
        className={composeBaseClass("zs-contextmenu-trigger", className)}
        tabIndex={resolvedTabIndex}
        aria-disabled={disabled || undefined}
        {...rest}
      />
    );
  },
);
ContextMenuTrigger.displayName = "ContextMenu.Trigger";

/* ─── Popup ────────────────────────────────────────────────────────── *
 *
 * ContextMenu.Popup is a small wrapper that paints the shared
 * `.zs-menu-popup` class but does NOT inject Menu.Popup's `side`/
 * `align`/`sideOffset` defaults — Base UI's ContextMenu positioning
 * is pointer-anchored at the click coordinates, so the click-menu
 * defaults (`side="bottom"`, `align="start"`, `sideOffset=6`) would
 * shift the popup off the pointer. We forward Popup-level positioner
 * props (`side`, `align`, `sideOffset`) only when the consumer sets
 * them, otherwise let Base UI's defaults apply. */

type BaseContextPopupProps = ComponentPropsWithoutRef<
  typeof BaseContextMenu.Popup
>;
type BaseContextPositionerProps = ComponentPropsWithoutRef<
  typeof BaseContextMenu.Positioner
>;
export interface ContextMenuPopupProps extends BaseContextPopupProps {
  /** Override anchor side. Default: Base UI's pointer-anchored
   *  default (no specific side). */
  side?: BaseContextPositionerProps["side"];
  /** Override alignment along the chosen side. */
  align?: BaseContextPositionerProps["align"];
  /** Override pixel offset between pointer and popup edge. */
  sideOffset?: number;
}

const ContextMenuPopup = forwardRef<HTMLElement, ContextMenuPopupProps>(
  function ContextMenuPopup(
    { side, align, sideOffset, className, children, ...rest },
    ref,
  ) {
    const positionerProps: BaseContextPositionerProps = {};
    if (side !== undefined) positionerProps.side = side;
    if (align !== undefined) positionerProps.align = align;
    if (sideOffset !== undefined) positionerProps.sideOffset = sideOffset;
    return (
      <BaseContextMenu.Positioner
        className="zs-menu-positioner"
        {...positionerProps}
      >
        <BaseMenu.Popup
          {...rest}
          ref={ref as Ref<HTMLDivElement>}
          className={composeBaseClass("zs-menu-popup", className)}
        >
          {children}
        </BaseMenu.Popup>
      </BaseContextMenu.Positioner>
    );
  },
);
ContextMenuPopup.displayName = "ContextMenu.Popup";

/* ─── public namespace ─────────────────────────────────────────────── *
 *
 * Subparts mirror Menu's surface by reference. The same `zs-menu-*`
 * CSS classes paint both surfaces; ContextMenu only adds a trigger
 * subpart and a thin Root. */

export type ContextMenuComponent = typeof ContextMenuRoot & {
  Trigger: typeof ContextMenuTrigger;
  Portal: typeof MenuPortal;
  Backdrop: typeof MenuBackdrop;
  Popup: typeof ContextMenuPopup;
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
ContextMenu.Popup = ContextMenuPopup;
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
