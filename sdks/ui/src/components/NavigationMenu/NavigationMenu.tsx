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
 *   - `Link` accepts `asChild` (via `Slot` from `_slot.ts`), matching
 *     the rest of the package's anchor-flavored subparts. Router-link
 *     composition flows through the same idiom as `Menu.LinkItem` /
 *     `Dialog.Close`.
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
 * Ref-forwarding note: Base UI's `NavigationMenuRoot` is declared as a
 * plain function component (not `forwardRef`), so refs do not flow into
 * it. We mirror that shape — `NavigationMenuProps` extends
 * `ComponentPropsWithoutRef` so consumers don't pass a `ref` that would
 * silently drop on React 18.
 *
 * Glass-surface invariant: the Popup paints an opaque `background-color`
 * (--zs-surface). The optional `backdrop-filter` (gated by
 * --zs-dialog-backdrop-filter) gives themes a frosted look when the
 * surface token is translucent.
 */
import {
  forwardRef,
  isValidElement,
  type ComponentPropsWithoutRef,
  type ReactNode,
  type Ref,
} from "react";
import { NavigationMenu as BaseNavMenu } from "@base-ui/react/navigation-menu";
import { ChevronDown } from "lucide-react";
import { Icon } from "../Icon/Icon";
import { classnames, composeBaseClass } from "../_classnames";
import { Slot, composeRefs } from "../_slot";

export type NavMenuSide = "top" | "right" | "bottom" | "left";
export type NavMenuAlign = "start" | "center" | "end";
export type NavMenuOrientation = "horizontal" | "vertical";

type BaseRootProps = ComponentPropsWithoutRef<typeof BaseNavMenu.Root>;

/** Internal helper — pulls the second argument of Base UI's
 *  `onValueChange` so we can re-declare the signature against the
 *  consumer's `Value` generic without leaking `any`. */
type BaseValueChangeEventDetails = Parameters<
  NonNullable<BaseRootProps["onValueChange"]>
>[1];

/**
 * Public NavigationMenu root props. We deliberately omit `render`
 * (Root is context-only; the prop has no meaning here) and re-declare
 * the controlled-value triple (`value` / `defaultValue` / `onValueChange`)
 * against the consumer's `Value` generic so the callback's first
 * parameter is `Value | null`, not `any`.
 *
 * The `ComponentPropsWithoutRef` extension means consumers cannot pass
 * a `ref` — Base UI's `NavigationMenuRoot` is a plain function
 * component (not `forwardRef`), so ref forwarding would silently drop.
 *
 * All other Base UI escape hatches (`delay`, `closeDelay`,
 * `actionsRef`, `onOpenChangeComplete`) pass through.
 */
export interface NavigationMenuProps<Value = unknown>
  extends Omit<
    BaseRootProps,
    "render" | "value" | "defaultValue" | "onValueChange"
  > {
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
  /**
   * Callback fired when the active item changes (open / close / swap).
   * The first argument is typed against the consumer's `Value` generic
   * so callers don't lose type safety at the boundary.
   */
  onValueChange?: (
    value: Value | null,
    eventDetails: BaseValueChangeEventDetails,
  ) => void;
  /** Optional class hook on the root. */
  className?: string;
  /** NavigationMenu contents — typically a single `List`, plus `Portal`. */
  children?: ReactNode;
}

/* ─── Root ──────────────────────────────────────────────────────────── */

function NavMenuRoot<Value = unknown>(props: NavigationMenuProps<Value>) {
  const { orientation = "horizontal", className, children, ...rest } = props;
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

/**
 * Props for the NavigationMenu List. Forwards Base UI's prop set —
 * children are typically `NavigationMenu.Item` siblings.
 */
export type NavMenuListProps = BaseListProps;

const NavMenuList = forwardRef<HTMLUListElement, NavMenuListProps>(
  function NavMenuList({ className, ...rest }, ref) {
    return (
      <BaseNavMenu.List
        {...rest}
        ref={ref}
        className={composeBaseClass("zs-navmenu-list", className)}
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

/**
 * Props for the NavigationMenu Item.
 *
 * Carries Base UI's `value` prop (any) — used to controllably target
 * this Item via the Root's `value` prop. Bare items work without one.
 */
export type NavMenuItemProps = BaseItemProps;

const NavMenuItem = forwardRef<HTMLLIElement, NavMenuItemProps>(
  function NavMenuItem({ className, ...rest }, ref) {
    return (
      <BaseNavMenu.Item
        {...rest}
        ref={ref}
        className={composeBaseClass("zs-navmenu-item", className)}
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

/**
 * Props for the NavigationMenu Trigger. Renders `<button>` by default.
 * Base UI emits `aria-expanded` + `data-popup-open` on the trigger so
 * the chevron rotation + active-surface paint can hook those state
 * attributes without callers wiring anything.
 */
export type NavMenuTriggerProps = BaseTriggerProps;

const NavMenuTrigger = forwardRef<HTMLButtonElement, NavMenuTriggerProps>(
  function NavMenuTrigger({ className, ...rest }, ref) {
    return (
      <BaseNavMenu.Trigger
        {...rest}
        ref={ref}
        className={composeBaseClass("zs-navmenu-trigger", className)}
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

/**
 * Props for the NavigationMenu Content panel.
 *
 * The Content lives INSIDE its owning Item but Base UI portals it into
 * the shared Viewport at render time — so you author it next to the
 * Trigger, and the active Content slot is what the viewport morphs to.
 */
export type NavMenuContentProps = BaseContentProps;

const NavMenuContent = forwardRef<HTMLDivElement, NavMenuContentProps>(
  function NavMenuContent({ className, ...rest }, ref) {
    return (
      <BaseNavMenu.Content
        {...rest}
        ref={ref}
        className={composeBaseClass("zs-navmenu-content", className)}
      />
    );
  },
);
NavMenuContent.displayName = "NavigationMenu.Content";

/* ─── Link ──────────────────────────────────────────────────────────── *
 *
 * An anchor inside an Item (top-level) or inside a Content panel
 * (nested). Renders `<a>` — Base UI emits `data-active` when the link
 * is the current page so the active state paints distinctly.
 *
 * Mirrors Dialog.Close / Menu.LinkItem: accepts `asChild` and routes
 * through the local `Slot` from `_slot.ts` so router-link composition
 * uses the same idiom as the rest of the package (Base UI's `render`
 * escape hatch is intentionally hidden from this surface). */

type BaseLinkProps = ComponentPropsWithoutRef<typeof BaseNavMenu.Link>;

/**
 * Props for the NavigationMenu Link.
 *
 * Renders `<a>` by default. Pass `asChild` + a single React element
 * child (e.g. a router `<Link>`) to compose this Link with a custom
 * router primitive while preserving the merged className, event
 * handlers, ref, and `data-active` state.
 */
export interface NavMenuLinkProps
  extends Omit<BaseLinkProps, "render" | "className"> {
  /**
   * Optional class hook on the rendered link. String only — the Base
   * UI function form (`(state) => string`) is intentionally hidden so
   * the prop carries one shape across the package. Compose your own
   * class string with the link's `data-active` attribute if you need
   * state-derived styling.
   */
  className?: string;
  /**
   * When `true`, render the single child element instead of an `<a>`.
   * Useful for composing with router-link components — the child
   * receives our merged props (className, event handlers, refs).
   *
   * @default false
   */
  asChild?: boolean;
}

const NavMenuLink = forwardRef<HTMLAnchorElement, NavMenuLinkProps>(
  function NavMenuLink({ asChild = false, className, children, ...rest }, ref) {
    // Public prop is string-only (see `NavMenuLinkProps.className`),
    // so we pass it straight through. The Base UI function form is
    // hidden at the type layer.
    const userClass = className;

    if (process.env.NODE_ENV !== "production" && asChild && !isValidElement(children)) {
      // eslint-disable-next-line no-console
      console.error(
        "NavigationMenu.Link asChild expects a single React element child; received " +
          typeof children +
          "; rendering nothing.",
      );
    }

    if (asChild) {
      // Slot path: route Base UI's emitted props onto the child element
      // via the local Slot helper. We MUST NOT pass `ref={ref}` to
      // `<BaseNavMenu.Link>` directly — Base UI's render-prop hands us
      // `linkProps.ref` already pointing at the element it would render.
      // Compose the outer forwarded ref with `linkProps.ref` INSIDE the
      // render-prop and let `Slot.getElementRef` compose the child's own
      // ref. This matches `Menu.LinkItem` (which itself mirrors
      // `Dialog.Close` commit 3a64a726). Doing the child-side
      // composition here would double-fire the child callback ref on
      // every attach (regression for wave10 review 🔴 #1).
      return (
        <BaseNavMenu.Link
          {...rest}
          render={(linkProps) => {
            const linkPropsRef = (linkProps as { ref?: Ref<unknown> }).ref;
            const baseClass = (linkProps as { className?: unknown }).className;
            const stringClass =
              typeof baseClass === "string" ? baseClass : undefined;
            if (!isValidElement(children)) {
              // Dev-error above already flagged the misuse. Return an
              // empty fragment so Base UI's render-prop contract
              // (returns ReactElement) is satisfied.
              return <></>;
            }
            return (
              <Slot
                {...(linkProps as Record<string, unknown>)}
                className={composeBaseClass(
                  "zs-navmenu-link",
                  // userClass on the wrapper outranks Base UI's state
                  // class (matches the default branch order below).
                  userClass ?? stringClass,
                )}
                ref={composeRefs(
                  ref as Ref<unknown>,
                  linkPropsRef,
                )}
              >
                {children}
              </Slot>
            );
          }}
        />
      );
    }
    return (
      <BaseNavMenu.Link
        {...rest}
        ref={ref}
        className={composeBaseClass("zs-navmenu-link", userClass)}
      >
        {children}
      </BaseNavMenu.Link>
    );
  },
);
NavMenuLink.displayName = "NavigationMenu.Link";

/* ─── Portal ────────────────────────────────────────────────────────── */

type BasePortalProps = ComponentPropsWithoutRef<typeof BaseNavMenu.Portal>;

/**
 * Props for the NavigationMenu Portal. The Portal teleports the
 * Positioner / Popup / Viewport tree out of the document flow so the
 * popup paints above the rest of the page; default container is
 * `document.body`.
 */
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

/**
 * Props for the NavigationMenu Positioner.
 *
 * Wraps Floating UI's anchored layer; the Popup mounts inside. The
 * `side` / `align` / `sideOffset` triple mirrors Popover's all-in-one
 * positioner knobs so authors don't reach for raw Floating UI middleware.
 */
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
        {...rest}
        ref={ref}
        side={side}
        align={align}
        sideOffset={sideOffset}
        className={composeBaseClass("zs-navmenu-positioner", className)}
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

/**
 * Props for the NavigationMenu Popup — the opaque sheet that hosts
 * the Viewport. The popup paints `--zs-surface` opaquely so axe-core's
 * color-contrast walk terminates here; optional `backdrop-filter`
 * (gated by `--zs-dialog-backdrop-filter`) gives the frosted look when
 * the surface token is translucent.
 */
export type NavMenuPopupProps = BasePopupProps;

const NavMenuPopup = forwardRef<HTMLElement, NavMenuPopupProps>(
  function NavMenuPopup({ className, ...rest }, ref) {
    return (
      <BaseNavMenu.Popup
        {...rest}
        ref={ref as Ref<HTMLElement>}
        className={composeBaseClass("zs-navmenu-popup", className)}
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

/**
 * Props for the NavigationMenu Viewport — the clipping window inside
 * the Popup that morphs between Items' Content. Base UI exposes
 * `--positioner-width` and `--positioner-height` custom properties on
 * the Positioner so this element can track the current Content
 * geometry during the morph.
 */
export type NavMenuViewportProps = BaseViewportProps;

const NavMenuViewport = forwardRef<HTMLDivElement, NavMenuViewportProps>(
  function NavMenuViewport({ className, ...rest }, ref) {
    return (
      <BaseNavMenu.Viewport
        {...rest}
        ref={ref}
        className={composeBaseClass("zs-navmenu-viewport", className)}
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

/**
 * Props for the NavigationMenu Arrow — the optional 16×8 triangle
 * that anchors the popup visually to the active trigger. Base UI
 * rotates the wrapping `<div>` per `data-side`; the inner SVG paints
 * `currentColor` (= the popup's surface token) so the triangle reads
 * as an extension of the popup.
 */
export type NavMenuArrowProps = BaseArrowProps;

const NavMenuArrow = forwardRef<HTMLDivElement, NavMenuArrowProps>(
  function NavMenuArrow({ className, children, ...rest }, ref) {
    return (
      <BaseNavMenu.Arrow
        {...rest}
        ref={ref}
        className={composeBaseClass("zs-navmenu-arrow", className)}
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

/**
 * Props for the NavigationMenu Icon — the chevron glyph that sits
 * next to a Trigger's label and rotates when the popup opens. Default
 * child is the canonical 12×12 chevron-down SVG.
 */
export type NavMenuIconProps = BaseIconProps;

const NavMenuIcon = forwardRef<HTMLSpanElement, NavMenuIconProps>(
  function NavMenuIcon({ className, children, ...rest }, ref) {
    return (
      <BaseNavMenu.Icon
        {...rest}
        ref={ref}
        className={composeBaseClass("zs-navmenu-icon", className)}
      >
        {/* The chevron is the governed `Icon` (Lucide chevron-down). It
            renders directly inside the `.zs-navmenu-icon` span, so the
            parent's `[data-popup-open]` rotation and the
            `.zs-navmenu-icon > svg { inline-size:100%; block-size:100% }`
            sizing rule keep matching it. */}
        {children ?? <Icon as={ChevronDown} />}
      </BaseNavMenu.Icon>
    );
  },
);
NavMenuIcon.displayName = "NavigationMenu.Icon";

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
