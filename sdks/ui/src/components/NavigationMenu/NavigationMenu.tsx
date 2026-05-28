/*
 * NavigationMenu — multi-level topnav with mega-menu popouts.
 *
 *   <NavigationMenu>
 *     <NavigationMenu.List>
 *       <NavigationMenu.Item>
 *         <NavigationMenu.Trigger>
 *           Products
 *           <NavigationMenu.Icon />
 *         </NavigationMenu.Trigger>
 *         <NavigationMenu.Content>
 *           <NavigationMenu.Link href="/builder">Builder</NavigationMenu.Link>
 *           …
 *         </NavigationMenu.Content>
 *       </NavigationMenu.Item>
 *       <NavigationMenu.Item>
 *         <NavigationMenu.Link href="/pricing">Pricing</NavigationMenu.Link>
 *       </NavigationMenu.Item>
 *     </NavigationMenu.List>
 *
 *     <NavigationMenu.Portal>
 *       <NavigationMenu.Positioner sideOffset={8}>
 *         <NavigationMenu.Popup>
 *           <NavigationMenu.Arrow />
 *           <NavigationMenu.Viewport />  -- holds the active Item's Content
 *         </NavigationMenu.Popup>
 *       </NavigationMenu.Positioner>
 *     </NavigationMenu.Portal>
 *   </NavigationMenu>
 *
 * Shape decisions:
 *   - Decomposed shape mirrors Popover / Dialog. The Content panel is
 *     declared INSIDE its owning Item, but Base UI portals it into the
 *     shared `Viewport` so a single floating panel hosts the active
 *     Item's content. This is the canonical mega-menu shape.
 *   - The Trigger button hovers / clicks open. Base UI handles the
 *     delay timing (50ms default open/close, overridable via Root's
 *     `delay`/`closeDelay`). We don't override.
 *   - Content sizing: `min-inline-size` floors the popup so even a
 *     single-link panel reads as a panel, not a tooltip. `max-inline-size`
 *     uses `min(56rem, calc(100dvw - …))` so the panel never overflows
 *     the viewport. Brief contingency point.
 *   - Items inside the List use roving tabindex via Base UI (Tab enters
 *     the list at the active item; arrow-left/right roves between items).
 *
 * Anti-patterns we explicitly avoid:
 *   - Re-mounting the Content on every open. Base UI keeps the active
 *     item's Content alive in the Viewport and animates between siblings,
 *     so the panel reads as one surface that morphs — not as a sequence
 *     of pop-in panels.
 *   - Hard-coding `nav` semantics on a nested NavigationMenu. Base UI
 *     emits `<nav>` only at the root level; nested instances render `<div>`
 *     so AT users don't hear "navigation landmark" multiple times.
 *   - Auto-mounting a Backdrop. NavigationMenu is a topnav surface, not a
 *     modal; the page is still interactive while a panel is open.
 *
 * Glass-surface invariant: the Popup paints an opaque `background-color`
 * (--zs-surface). The optional `backdrop-filter` (gated by
 * --zs-dialog-backdrop-filter) gives themes a frosted look when the
 * surface token is translucent.
 */
import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type ComponentPropsWithRef,
  type ReactNode,
  type Ref,
} from "react";
import { NavigationMenu as BaseNavMenu } from "@base-ui/react/navigation-menu";
import { classnames, composeBaseClass } from "../_classnames";

export type NavMenuSide = "top" | "right" | "bottom" | "left";
export type NavMenuAlign = "start" | "center" | "end";
export type NavMenuOrientation = "horizontal" | "vertical";

type BaseRootProps = ComponentPropsWithRef<typeof BaseNavMenu.Root>;

/**
 * Re-export Base UI's nav-menu Root props (less `render`, which is
 * meaningless on a context-only Root). All Base UI escape hatches
 * (`value`, `defaultValue`, `onValueChange`, `delay`, `closeDelay`,
 * `actionsRef`, `onOpenChangeComplete`) pass through.
 */
export interface NavigationMenuProps<Value = unknown>
  extends Omit<BaseRootProps, "render" | "value" | "defaultValue" | "onValueChange"> {
  /**
   * Layout axis. Forwarded to Base UI; affects the arrow-key roving
   * direction inside the List.
   *
   * @default "horizontal"
   */
  orientation?: NavMenuOrientation;
  /**
   * Controlled active-item value. Pass `null` to force the popup closed.
   * Bare items render fine without a value (Base UI auto-generates IDs).
   */
  value?: Value | null;
  /** Uncontrolled initial active-item value. */
  defaultValue?: Value | null;
  /** Callback fired when the active item changes (open / close / swap). */
  onValueChange?: BaseRootProps["onValueChange"];
  /** Optional class hook on the root. */
  className?: string;
  /** NavigationMenu contents — typically a single `List`, plus `Portal`. */
  children?: ReactNode;
}

/* ─── Root ──────────────────────────────────────────────────────────── */

function NavMenuRoot<Value = unknown>({
  orientation = "horizontal",
  className,
  children,
  ...rest
}: NavigationMenuProps<Value>) {
  return (
    <BaseNavMenu.Root
      {...(rest as BaseRootProps)}
      orientation={orientation}
      className={composeBaseClass(
        classnames("zs-navmenu", `zs-navmenu--${orientation}`),
        className,
      )}
      data-orientation={orientation}
    >
      {children}
    </BaseNavMenu.Root>
  );
}
NavMenuRoot.displayName = "NavigationMenu";

/* ─── List ──────────────────────────────────────────────────────────── *
 *
 * The horizontal (or vertical) strip of top-level Items. Renders `<ul>`
 * so AT users can hear "list, N items" inside the nav landmark. */

type BaseListProps = ComponentPropsWithoutRef<typeof BaseNavMenu.List>;
export type NavMenuListProps = BaseListProps;

const NavMenuList = forwardRef<HTMLUListElement, NavMenuListProps>(
  function NavMenuList({ className, ...rest }, ref) {
    return (
      <BaseNavMenu.List
        ref={ref}
        className={composeBaseClass("zs-navmenu-list", className)}
        {...rest}
      />
    );
  },
);
NavMenuList.displayName = "NavigationMenu.List";

/* ─── Item ──────────────────────────────────────────────────────────── *
 *
 * One slot in the List. Renders `<li>`. May contain either a Trigger +
 * Content pair (mega-menu) OR a plain Link (single-action top-level item). */

type BaseItemProps = ComponentPropsWithoutRef<typeof BaseNavMenu.Item>;
export type NavMenuItemProps = BaseItemProps;

const NavMenuItem = forwardRef<HTMLLIElement, NavMenuItemProps>(
  function NavMenuItem({ className, ...rest }, ref) {
    return (
      <BaseNavMenu.Item
        ref={ref}
        className={composeBaseClass("zs-navmenu-item", className)}
        {...rest}
      />
    );
  },
);
NavMenuItem.displayName = "NavigationMenu.Item";

/* ─── Trigger ───────────────────────────────────────────────────────── *
 *
 * The clickable / hoverable label that opens this Item's Content. Renders
 * a real `<button>` — Base UI emits `aria-expanded` and `data-popup-open`
 * which we hook for the chevron rotation and active surface paint. */

type BaseTriggerProps = ComponentPropsWithoutRef<typeof BaseNavMenu.Trigger>;
export type NavMenuTriggerProps = BaseTriggerProps;

const NavMenuTrigger = forwardRef<HTMLButtonElement, NavMenuTriggerProps>(
  function NavMenuTrigger({ className, ...rest }, ref) {
    return (
      <BaseNavMenu.Trigger
        ref={ref}
        className={composeBaseClass("zs-navmenu-trigger", className)}
        {...rest}
      />
    );
  },
);
NavMenuTrigger.displayName = "NavigationMenu.Trigger";

/* ─── Content ───────────────────────────────────────────────────────── *
 *
 * The panel of links / cards / etc. shown when this Item is active.
 * Declared inside the Item; Base UI portals it into the shared Viewport
 * during render so a single floating panel hosts whichever Item is open. */

type BaseContentProps = ComponentPropsWithoutRef<typeof BaseNavMenu.Content>;
export type NavMenuContentProps = BaseContentProps;

const NavMenuContent = forwardRef<HTMLDivElement, NavMenuContentProps>(
  function NavMenuContent({ className, ...rest }, ref) {
    return (
      <BaseNavMenu.Content
        ref={ref}
        className={composeBaseClass("zs-navmenu-content", className)}
        {...rest}
      />
    );
  },
);
NavMenuContent.displayName = "NavigationMenu.Content";

/* ─── Link ──────────────────────────────────────────────────────────── *
 *
 * An anchor inside an Item (top-level) or inside a Content panel
 * (nested). Renders `<a>` — Base UI emits `data-active` when the link
 * is the current page so the active state paints distinctly. */

type BaseLinkProps = ComponentPropsWithoutRef<typeof BaseNavMenu.Link>;
export type NavMenuLinkProps = BaseLinkProps;

const NavMenuLink = forwardRef<HTMLAnchorElement, NavMenuLinkProps>(
  function NavMenuLink({ className, ...rest }, ref) {
    return (
      <BaseNavMenu.Link
        ref={ref}
        className={composeBaseClass("zs-navmenu-link", className)}
        {...rest}
      />
    );
  },
);
NavMenuLink.displayName = "NavigationMenu.Link";

/* ─── Portal ────────────────────────────────────────────────────────── */

type BasePortalProps = ComponentPropsWithoutRef<typeof BaseNavMenu.Portal>;
export type NavMenuPortalProps = BasePortalProps;

function NavMenuPortal(props: NavMenuPortalProps) {
  return <BaseNavMenu.Portal {...props} />;
}
NavMenuPortal.displayName = "NavigationMenu.Portal";

/* ─── Positioner ────────────────────────────────────────────────────── *
 *
 * Floating UI anchored layer. The Popup mounts inside this — surfacing
 * `side`/`align`/`sideOffset` as direct props mirrors the Popover
 * shape (all-in-one knobs on the visible element). For NavigationMenu
 * the typical authored shape is `Positioner -> Popup -> Viewport`, but
 * we still let the Popup carry the anchor knobs for callers that build
 * their own Positioner. */

type BasePositionerProps = ComponentPropsWithoutRef<typeof BaseNavMenu.Positioner>;
export interface NavMenuPositionerProps extends BasePositionerProps {
  /** Which side of the active trigger to anchor on. Default `bottom`. */
  side?: NavMenuSide;
  /** Alignment along the chosen side. Default `center`. */
  align?: NavMenuAlign;
  /** Pixel offset between trigger and popup. Default `8`. */
  sideOffset?: number;
}

const NavMenuPositioner = forwardRef<HTMLDivElement, NavMenuPositionerProps>(
  function NavMenuPositioner(
    {
      side = "bottom",
      align = "center",
      sideOffset = 8,
      className,
      children,
      ...rest
    },
    ref,
  ) {
    return (
      <BaseNavMenu.Positioner
        ref={ref}
        side={side}
        align={align}
        sideOffset={sideOffset}
        className={composeBaseClass("zs-navmenu-positioner", className)}
        {...rest}
      >
        {children}
      </BaseNavMenu.Positioner>
    );
  },
);
NavMenuPositioner.displayName = "NavigationMenu.Positioner";

/* ─── Popup ─────────────────────────────────────────────────────────── *
 *
 * The opaque sheet that hosts the Viewport. Glass-surface invariant
 * applies — opaque background-color + optional backdrop-filter so axe
 * never sees through to the underlying page. */

type BasePopupProps = ComponentPropsWithoutRef<typeof BaseNavMenu.Popup>;
export type NavMenuPopupProps = BasePopupProps;

const NavMenuPopup = forwardRef<HTMLElement, NavMenuPopupProps>(
  function NavMenuPopup({ className, ...rest }, ref) {
    return (
      <BaseNavMenu.Popup
        ref={ref as Ref<HTMLElement>}
        className={composeBaseClass("zs-navmenu-popup", className)}
        {...rest}
      />
    );
  },
);
NavMenuPopup.displayName = "NavigationMenu.Popup";

/* ─── Viewport ──────────────────────────────────────────────────────── *
 *
 * The clipping window inside the Popup that morphs between Items'
 * Content. Base UI handles the sizing animation; we just paint the
 * border-radius so the morph reads continuous. */

type BaseViewportProps = ComponentPropsWithoutRef<typeof BaseNavMenu.Viewport>;
export type NavMenuViewportProps = BaseViewportProps;

const NavMenuViewport = forwardRef<HTMLDivElement, NavMenuViewportProps>(
  function NavMenuViewport({ className, ...rest }, ref) {
    return (
      <BaseNavMenu.Viewport
        ref={ref}
        className={composeBaseClass("zs-navmenu-viewport", className)}
        {...rest}
      />
    );
  },
);
NavMenuViewport.displayName = "NavigationMenu.Viewport";

/* ─── Arrow ─────────────────────────────────────────────────────────── *
 *
 * Same 16×8 SVG triangle Popover uses (see Popover.tsx `ArrowGlyph`).
 * Base UI rotates the wrapping div per side; the SVG paints in
 * `currentColor` (= popup surface) so the triangle reads as an
 * extension of the popup. */

type BaseArrowProps = ComponentPropsWithoutRef<typeof BaseNavMenu.Arrow>;
export type NavMenuArrowProps = BaseArrowProps;

const NavMenuArrow = forwardRef<HTMLDivElement, NavMenuArrowProps>(
  function NavMenuArrow({ className, children, ...rest }, ref) {
    return (
      <BaseNavMenu.Arrow
        ref={ref}
        className={composeBaseClass("zs-navmenu-arrow", className)}
        {...rest}
      >
        {children ?? <ArrowGlyph />}
      </BaseNavMenu.Arrow>
    );
  },
);
NavMenuArrow.displayName = "NavigationMenu.Arrow";

function ArrowGlyph() {
  // SVG viewBox units are unitless; not subject to the no-raw-px rule.
  return (
    <svg
      width="16"
      height="8"
      viewBox="0 0 16 8"
      aria-hidden="true"
      focusable="false"
    >
      <path d="M 0,0 L 8,8 L 16,0 Z" fill="currentColor" />
    </svg>
  );
}

/* ─── Icon ──────────────────────────────────────────────────────────── *
 *
 * The chevron glyph that sits next to a Trigger's label and rotates
 * when the popup opens. Renders `<span>`. The SVG below is the canonical
 * 12×12 chevron-down; we rotate via CSS based on `data-open`. */

type BaseIconProps = ComponentPropsWithoutRef<typeof BaseNavMenu.Icon>;
export type NavMenuIconProps = BaseIconProps;

const NavMenuIcon = forwardRef<HTMLSpanElement, NavMenuIconProps>(
  function NavMenuIcon({ className, children, ...rest }, ref) {
    return (
      <BaseNavMenu.Icon
        ref={ref}
        className={composeBaseClass("zs-navmenu-icon", className)}
        {...rest}
      >
        {children ?? <ChevronGlyph />}
      </BaseNavMenu.Icon>
    );
  },
);
NavMenuIcon.displayName = "NavigationMenu.Icon";

function ChevronGlyph() {
  return (
    <svg
      width="12"
      height="12"
      viewBox="0 0 12 12"
      aria-hidden="true"
      focusable="false"
    >
      <path
        d="M 3,4.5 L 6,7.5 L 9,4.5"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

/* ─── public namespace ──────────────────────────────────────────────── */

export type NavigationMenuComponent = (<Value = unknown>(
  props: NavigationMenuProps<Value>,
) => React.JSX.Element) & {
  List: typeof NavMenuList;
  Item: typeof NavMenuItem;
  Trigger: typeof NavMenuTrigger;
  Content: typeof NavMenuContent;
  Link: typeof NavMenuLink;
  Portal: typeof NavMenuPortal;
  Positioner: typeof NavMenuPositioner;
  Popup: typeof NavMenuPopup;
  Viewport: typeof NavMenuViewport;
  Arrow: typeof NavMenuArrow;
  Icon: typeof NavMenuIcon;
  displayName?: string;
};

export const NavigationMenu = NavMenuRoot as unknown as NavigationMenuComponent;
NavigationMenu.List = NavMenuList;
NavigationMenu.Item = NavMenuItem;
NavigationMenu.Trigger = NavMenuTrigger;
NavigationMenu.Content = NavMenuContent;
NavigationMenu.Link = NavMenuLink;
NavigationMenu.Portal = NavMenuPortal;
NavigationMenu.Positioner = NavMenuPositioner;
NavigationMenu.Popup = NavMenuPopup;
NavigationMenu.Viewport = NavMenuViewport;
NavigationMenu.Arrow = NavMenuArrow;
NavigationMenu.Icon = NavMenuIcon;
NavigationMenu.displayName = "NavigationMenu";
