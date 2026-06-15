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
 *   - `disabled` lives on the Root (mirrors Base UI's MenuRoot
 *     contract). Wave-9 🔴 fix: the previous shape exposed `disabled`
 *     on the Trigger but only stamped `aria-disabled`/`tabIndex`; the
 *     Base UI store wasn't informed, so a right-click still opened the
 *     popup. Pre-launch/no-shim rule says rename the surface rather
 *     than smuggle in a wrapper handler. With `disabled` on Root, Base
 *     UI's Trigger short-circuits its `contextmenu` / touch handlers
 *     AND the document-level contextmenu listener.
 *   - Trigger renders a `<div>` natively, and accepts `asChild` to
 *     route through the shared Slot helper. The default path is the
 *     wrapper `<div>` (the consumer's content lives inside); the
 *     `asChild` path lets a semantic element (e.g. a Card root) BE the
 *     trigger, so right-click bounds match the element's intrinsic
 *     bounding box.
 */
import {
  createContext,
  forwardRef,
  isValidElement,
  useCallback,
  useContext,
  type ComponentPropsWithoutRef,
  type ComponentPropsWithRef,
  type MouseEvent as ReactMouseEvent,
  type ReactElement,
  type ReactNode,
  type Ref,
} from "react";
import { ContextMenu as BaseContextMenu } from "@base-ui/react/context-menu";
import { Menu as BaseMenu } from "@base-ui/react/menu";
import type { BaseUIEvent } from "@base-ui/react/internals/types";
import { composeBaseClass } from "../_classnames";
import { Slot, composeRefs } from "../_slot";
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

/* ─── Disabled context ─────────────────────────────────────────────── *
 *
 * Wave-9 🔴 fix. `disabled` lives on the Root, but the Trigger needs
 * to defensively block the `contextmenu` event BEFORE Base UI's
 * internal handler routes it through `actionsRef.setOpen`. Base UI's
 * own disabled check works for real browser right-clicks, but the
 * useEvent path from @testing-library/user-event (storybook's play()
 * runner) can race the store-sync — so we own a small context here
 * and stop the event at our layer. The context is also a defensive
 * second line: even if Base UI's check is correct, ours runs first
 * (React event handlers fire in declaration order, ours via
 * onContextMenuCapture before Base UI's onContextMenu). */
const ContextMenuDisabledContext = createContext<boolean>(false);

/* ─── Root ─────────────────────────────────────────────────────────── */

type BaseRootProps = ComponentPropsWithRef<typeof BaseContextMenu.Root>;

export interface ContextMenuProps extends BaseRootProps {
  /**
   * The Root's children — Trigger, Portal, and any other subparts. Base
   * UI's ContextMenuRoot doesn't render an HTML element of its own; it
   * only provides context for the Trigger / Popup pair.
   */
  children?: ReactNode;
}

function ContextMenuRoot({ children, ...rest }: ContextMenuProps) {
  // `disabled` is forwarded to Base UI's Root AND seeded into our own
  // context for the Trigger to defensively gate. Both paths must agree
  // (Base UI's `disabled` prop AND our context value come from the
  // same destructured boolean).
  const disabled = rest.disabled ?? false;
  return (
    <ContextMenuDisabledContext.Provider value={disabled}>
      <BaseContextMenu.Root {...rest}>{children}</BaseContextMenu.Root>
    </ContextMenuDisabledContext.Provider>
  );
}
ContextMenuRoot.displayName = "ContextMenu";

/* ─── Trigger ──────────────────────────────────────────────────────── *
 *
 * Renders a `<div>` that listens for `contextmenu` (and `pointerdown`-
 * long-press on touch). The consumer's content lives inside the
 * trigger div. We stamp a class for an explicit hit area + focus ring
 * so right-click targets are obvious; the consumer's content sits
 * INSIDE without any layout pressure.
 *
 * `asChild` lets the consumer's own element BE the trigger via the
 * shared Slot helper (mirrors Dialog.Close, commit 3a64a726). The
 * Slot path composes className / style / event handlers / refs so a
 * Card root can carry both its own styling and the right-click
 * binding without a wrapper. */

type BaseTriggerProps = ComponentPropsWithoutRef<typeof BaseContextMenu.Trigger>;
export type ContextMenuTriggerProps = BaseTriggerProps & {
  /**
   * Render as the single child element rather than our `<div>`. Used
   * when the consumer wants their semantic element (a Card root, a
   * region, an article) to BE the right-click target — same Base UI
   * binding, no wrapper. Pass a single React element child; anything
   * else is a dev-error and renders nothing.
   *
   * `disabled` is NOT a trigger-level prop — pass it on the Root
   * (`<ContextMenu disabled>`). When the Root is disabled the Trigger
   * stamps `aria-disabled="true"`, falls out of the tab sequence
   * (`tabIndex={-1}`), and blocks the `contextmenu` event before it
   * reaches Base UI's open path.
   */
  asChild?: boolean;
};

const ContextMenuTrigger = forwardRef<HTMLDivElement, ContextMenuTriggerProps>(
  function ContextMenuTrigger(
    {
      asChild = false,
      className,
      tabIndex,
      children,
      onContextMenu: callerOnContextMenu,
      role,
      "aria-haspopup": ariaHasPopup,
      ...rest
    },
    ref,
  ) {
    const disabled = useContext(ContextMenuDisabledContext);
    // Base UI renders the trigger as a plain `<div>` with no
    // tabIndex/role, so Tab skips it and the Shift+F10 keyboard-open
    // path documented on the stories is unreachable. Stamp a default
    // tabIndex so keyboard users can focus the trigger and fire the
    // contextmenu via Shift+F10. Caller-supplied tabIndex wins; but
    // when the Root is disabled, the trigger is pulled out of the tab
    // sequence regardless of caller intent. */
    const resolvedTabIndex = disabled
      ? -1
      : tabIndex !== undefined
        ? tabIndex
        : 0;

    // Defensive contextmenu blocker — runs at our React layer BEFORE
    // Base UI's `onContextMenu` handler via the capture phase.
    // `preventDefault` blocks the native browser menu; `stopPropagation`
    // stops bubbling into Base UI's document-level listener;
    // `preventBaseUIHandler` short-circuits Base UI's own listener on
    // the same element. The early `return` keeps Base UI's store-driven
    // `setOpen` from firing in the same tick. Belt-and-suspenders
    // against any race between `useSyncedValues` and a synthesized
    // right-click (storybook play() runner).
    const handleContextMenu = useCallback(
      (event: BaseUIEvent<ReactMouseEvent<HTMLDivElement>>) => {
        if (disabled) {
          event.preventDefault();
          event.stopPropagation();
          event.preventBaseUIHandler();
          return;
        }
        callerOnContextMenu?.(event);
      },
      [disabled, callerOnContextMenu],
    );

    if (process.env.NODE_ENV !== "production" && asChild && !isValidElement(children)) {
      // eslint-disable-next-line no-console
      console.error(
        "ContextMenu.Trigger asChild expects a single React element child; received " +
          typeof children +
          "; rendering nothing.",
      );
    }

    return (
      <BaseContextMenu.Trigger
        {...rest}
        ref={ref}
        className={composeBaseClass("zs-contextmenu-trigger", className)}
        tabIndex={resolvedTabIndex}
        role={asChild ? role : role ?? "button"}
        aria-haspopup={asChild ? ariaHasPopup : ariaHasPopup ?? "menu"}
        aria-disabled={disabled || undefined}
        onContextMenuCapture={handleContextMenu}
        render={
          asChild
            ? (triggerProps) => {
                if (!isValidElement(children)) {
                  // Render an empty fragment so Base UI's render-prop
                  // contract (returns a ReactElement) is satisfied; the
                  // dev-error above already flagged the misuse.
                  return <></>;
                }
                const tp = triggerProps as Record<string, unknown>;
                const tpRef = (triggerProps as { ref?: Ref<unknown> }).ref;
                // Slot composes the child's own ref via composeRefs
                // internally, so we MUST NOT pre-merge the child ref
                // here — a double-compose would invoke the child ref
                // twice on every mount. Caller-supplied props were
                // already spread onto BaseContextMenu.Trigger above and
                // are now part of `triggerProps`, so we just forward
                // those — no double-spread. Mirrors Menu.LinkItem
                // asChild branch (commit 3a64a726).
                return (
                  <Slot
                    {...tp}
                    ref={composeRefs(ref as Ref<unknown>, tpRef)}
                  >
                    {children as ReactElement}
                  </Slot>
                );
              }
            : undefined
        }
      >
        {asChild ? undefined : children}
      </BaseContextMenu.Trigger>
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
