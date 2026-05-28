/*
 * Menu — anchored dropdown opened by a click trigger.
 *
 *   <Menu>
 *     <Menu.Trigger>Actions</Menu.Trigger>
 *     <Menu.Portal>
 *       <Menu.Backdrop />                  -- opt-in (popover-feel default)
 *       <Menu.Popup>
 *         <Menu.Arrow />                   -- opt-in
 *         <Menu.Group>
 *           <Menu.GroupLabel>Edit</Menu.GroupLabel>
 *           <Menu.Item onClick={…}>Cut</Menu.Item>
 *           <Menu.Item onClick={…}>Copy</Menu.Item>
 *         </Menu.Group>
 *         <Menu.Separator />
 *         <Menu.CheckboxItem checked={spell} onCheckedChange={setSpell}>
 *           Spellcheck
 *         </Menu.CheckboxItem>
 *         <Menu.RadioGroup value={theme} onValueChange={setTheme}>
 *           <Menu.RadioItem value="light">Light</Menu.RadioItem>
 *           <Menu.RadioItem value="dark">Dark</Menu.RadioItem>
 *         </Menu.RadioGroup>
 *         <Menu.Submenu trigger={<span>More</span>}>
 *           <Menu.Item>Nested</Menu.Item>
 *         </Menu.Submenu>
 *         <Menu.LinkItem href="/docs">Docs</Menu.LinkItem>
 *       </Menu.Popup>
 *     </Menu.Portal>
 *   </Menu>
 *
 * Shape decisions:
 *   - Decomposed Portal / Backdrop / Popup mirrors Dialog (commit
 *     3a64a726) so every popover-family member reads the same way. The
 *     Menu differs from Dialog in two structural details:
 *       1. It anchors via Floating UI, so we mount an internal
 *          `<BaseMenu.Positioner>` INSIDE Portal but OUTSIDE Popup —
 *          `side`, `align`, `sideOffset` are Popup-level props that
 *          project onto the positioner. Same trick Popover uses.
 *       2. Items / Groups / Separator / CheckboxItem / RadioGroup /
 *          RadioItem / LinkItem are leaf subparts that paint the popup
 *          contents. Each is a thin wrapper that stamps a `zs-menu-*`
 *          class so the CSS owns visuals.
 *   - Backdrop is OPT-IN. Default is popover-feel (no Backdrop, no
 *     scroll lock). `modal` defaults to FALSE for the same reason
 *     (Base UI's MenuRoot defaults `modal: true`).
 *   - CheckboxItem visual reuses the Checkbox accent fill — same
 *     hit-target floor (1.75rem), same indicator gutter as Select.Item.
 *   - RadioItem visual reuses the Radio dot pattern (filled circle in
 *     the same indicator gutter).
 *   - LinkItem renders an `<a>` natively; supports `href` + `target`.
 *     `asChild` via Slot for callers passing a router `<Link>`.
 *   - Submenu is a sugar wrapper around `SubmenuRoot + SubmenuTrigger +
 *     Portal + Positioner + Popup`. The trigger row reads as a
 *     Menu.Item with a chevron tail. `side="right"` default; Floating UI
 *     flips to left near the viewport edge.
 *   - Arrow renders an SVG triangle scoped to the popup surface color
 *     (same `M 0,0 L 8,8 L 16,0 Z` path Popover uses).
 *
 * Anti-patterns explicitly avoided:
 *   - Auto-mount Backdrop. Most menus are popovers, not modals — the
 *     Backdrop subpart is exposed but never auto-mounted.
 *   - role="menu"-style focus trap on by default. `modal={false}` keeps
 *     the popup non-trapping; consumers opt into `modal={true}` for the
 *     rare full-modal menu (e.g., mobile share-sheet pattern).
 *   - Glass-surface drift. The Popup paints an opaque background-color
 *     + optional backdrop-filter, matching Dialog / AlertDialog /
 *     Popover / Select.
 *   - Per-item-shape props on Root (`items={[…]}`). Children composition
 *     only — every subpart is a real React element so consumers slot
 *     icons, kbd hints, and custom render via `asChild` (LinkItem) or
 *     plain children (Item).
 */
import {
  forwardRef,
  isValidElement,
  type ComponentPropsWithoutRef,
  type ComponentPropsWithRef,
  type ReactNode,
  type Ref,
} from "react";
import { Menu as BaseMenu } from "@base-ui/react/menu";
import { Slot, composeRefs } from "../_slot";
import { composeBaseClass } from "../_classnames";

/* Physical sides + logical (RTL-flipping) sides. Base UI's Positioner
 * accepts both — `inline-start` / `inline-end` flip with `dir="rtl"`,
 * which keeps Submenu anchoring on the correct visual side without
 * leaking direction state into consumers. The chevron tail is mirrored
 * separately by `[dir="rtl"]` CSS so it still points TOWARD the
 * anchored popup. */
export type MenuSide =
  | "top"
  | "right"
  | "bottom"
  | "left"
  | "inline-start"
  | "inline-end";
export type MenuAlign = "start" | "center" | "end";

/* ─── Root ─────────────────────────────────────────────────────────── */

type BaseRootProps = ComponentPropsWithRef<typeof BaseMenu.Root>;

/**
 * Props for the Menu root. Mirrors Base UI's `Menu.Root` so every
 * escape hatch (`onOpenChangeComplete`, `actionsRef`, `handle`,
 * `triggerId`, `defaultTriggerId`, payload child-render) is forwarded.
 *
 * `render` is omitted — Root is a context provider with no DOM, so
 * the prop has no meaning at this layer. Same shape Dialog/Popover use.
 */
export interface MenuProps extends Omit<BaseRootProps, "render"> {
  children?: ReactNode;
}

/**
 * Re-export Base UI's `createHandle` so consumers can imperatively
 * pair a Trigger to a Root. Matches Dialog / Popover.
 */
export const createMenuHandle = BaseMenu.createHandle;

function MenuRoot({ children, modal, ...rest }: MenuProps) {
  // Popover-feel default: `modal=false`. Base UI's MenuRoot defaults
  // to `true` (locks scroll + pointer-blocks outside). Consumers can
  // opt back into the modal-feel by passing `modal={true}` explicitly.
  return (
    <BaseMenu.Root modal={modal ?? false} {...rest}>
      {children}
    </BaseMenu.Root>
  );
}
MenuRoot.displayName = "Menu";

/* ─── Trigger ──────────────────────────────────────────────────────── */

type BaseTriggerProps = ComponentPropsWithoutRef<typeof BaseMenu.Trigger>;
export type MenuTriggerProps = BaseTriggerProps;

/* HTMLElement (not HTMLButtonElement): Base UI's `render` lets the
 * caller swap the rendered element (e.g. `<a>`), so the consumer's
 * ref must accept any DOM element. Same widen-then-cast as Popover. */
const MenuTrigger = forwardRef<HTMLElement, MenuTriggerProps>(
  function MenuTrigger(props, ref) {
    return (
      <BaseMenu.Trigger ref={ref as Ref<HTMLButtonElement>} {...props} />
    );
  },
);
MenuTrigger.displayName = "Menu.Trigger";

/* ─── Portal ───────────────────────────────────────────────────────── */

type BasePortalProps = ComponentPropsWithoutRef<typeof BaseMenu.Portal>;
export type MenuPortalProps = BasePortalProps;

function MenuPortal(props: MenuPortalProps) {
  return <BaseMenu.Portal {...props} />;
}
MenuPortal.displayName = "Menu.Portal";

/* ─── Backdrop (opt-in) ────────────────────────────────────────────── *
 *
 * Default is NO backdrop — menus are anchored panels, not modals. The
 * brief explicitly carves out a Backdrop subpart for the modal-feel
 * menu (mobile share-sheet pattern), so we provide the subpart but
 * never auto-mount it. */

type BaseBackdropProps = ComponentPropsWithoutRef<typeof BaseMenu.Backdrop>;
export type MenuBackdropProps = BaseBackdropProps;

const MenuBackdrop = forwardRef<HTMLDivElement, MenuBackdropProps>(
  function MenuBackdrop({ className, ...rest }, ref) {
    return (
      <BaseMenu.Backdrop
        ref={ref as Ref<HTMLDivElement>}
        className={composeBaseClass("zs-menu-backdrop", className)}
        {...rest}
      />
    );
  },
);
MenuBackdrop.displayName = "Menu.Backdrop";

/* ─── Popup ────────────────────────────────────────────────────────── *
 *
 * The Popup mounts inside an internal `<BaseMenu.Positioner>` so the
 * Floating UI anchoring (`side`, `align`, `sideOffset`) is configurable
 * via Popup-level props without the consumer reaching for the positioner.
 * Same all-in-one shape Popover uses. */

type BasePopupProps = ComponentPropsWithoutRef<typeof BaseMenu.Popup>;
export interface MenuPopupProps extends BasePopupProps {
  /** Which side of the trigger to anchor on. Default `bottom`. */
  side?: MenuSide;
  /** Alignment along the chosen side. Default `start`. */
  align?: MenuAlign;
  /** Pixel offset between trigger and popup. Default `6`. */
  sideOffset?: number;
}

const MenuPopup = forwardRef<HTMLElement, MenuPopupProps>(function MenuPopup(
  {
    side = "bottom",
    align = "start",
    sideOffset = 6,
    className,
    children,
    ...rest
  },
  ref,
) {
  return (
    <BaseMenu.Positioner
      className="zs-menu-positioner"
      side={side}
      align={align}
      sideOffset={sideOffset}
    >
      <BaseMenu.Popup
        {...rest}
        ref={ref as Ref<HTMLDivElement>}
        className={composeBaseClass("zs-menu-popup", className)}
      >
        {children}
      </BaseMenu.Popup>
    </BaseMenu.Positioner>
  );
});
MenuPopup.displayName = "Menu.Popup";

/* ─── Arrow ────────────────────────────────────────────────────────── *
 *
 * Floating UI rotates the wrapping `<div>` per side; the inner SVG is
 * a fixed 16×8 downward triangle that the rotation reorients. Same
 * path Popover uses for visual continuity. */

type BaseArrowProps = ComponentPropsWithoutRef<typeof BaseMenu.Arrow>;
export type MenuArrowProps = BaseArrowProps;

const MenuArrow = forwardRef<HTMLDivElement, MenuArrowProps>(
  function MenuArrow({ className, children, ...rest }, ref) {
    return (
      <BaseMenu.Arrow
        ref={ref}
        className={composeBaseClass("zs-menu-arrow", className)}
        {...rest}
      >
        {children ?? <ArrowGlyph />}
      </BaseMenu.Arrow>
    );
  },
);
MenuArrow.displayName = "Menu.Arrow";

function ArrowGlyph() {
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

/* ─── Item ─────────────────────────────────────────────────────────── *
 *
 * The flag-and-text item. Three-column grid (indicator gutter | text |
 * trailing) mirrors Select.Item's indicator gutter so the row layout
 * reads consistent across the popover-family — every Item reserves the
 * indicator column so adjacent CheckboxItem / RadioItem rows line up.
 * The trailing column hosts an optional keyboard-shortcut hint or the
 * submenu chevron — without it, plain Items pass children straight into
 * the text lane (which would otherwise land in the indicator track and
 * stack multi-word labels vertically).
 *
 * A `shortcut` prop projects into the trailing `.zs-menu-item__shortcut`
 * slot. Item / LinkItem / CheckboxItem / RadioItem all accept it; the
 * Submenu trigger reserves the slot for its chevron. */

type BaseItemProps = ComponentPropsWithoutRef<typeof BaseMenu.Item>;
export interface MenuItemProps extends BaseItemProps {
  /** Trailing keyboard-shortcut hint (e.g. `"⌘X"`). Presentational
   *  only — Base UI's text-navigation matches the row's label, not the
   *  shortcut text. */
  shortcut?: ReactNode;
}

const MenuItem = forwardRef<HTMLElement, MenuItemProps>(function MenuItem(
  { className, children, shortcut, label, ...rest },
  ref,
) {
  // Base UI's typeahead falls back to the item's `textContent` when
  // `label` is omitted. The `shortcut` span is aria-hidden but still
  // contributes to textContent, so typing "V" would land on
  // "Paste⌘V". When `children` is a plain string AND a shortcut is
  // set, project the string into `label` so typeahead matches just
  // the label text. Consumer-supplied `label` wins. */
  const autoLabel =
    label === undefined &&
    shortcut !== undefined &&
    typeof children === "string"
      ? children
      : label;
  return (
    <BaseMenu.Item
      ref={ref as Ref<HTMLDivElement>}
      className={composeBaseClass("zs-menu-item", className)}
      label={autoLabel}
      {...rest}
    >
      <span className="zs-menu-item__indicator" aria-hidden="true" />
      <span className="zs-menu-item__text">{children}</span>
      {shortcut !== undefined ? (
        <span className="zs-menu-item__shortcut" aria-hidden="true">
          {shortcut}
        </span>
      ) : null}
    </BaseMenu.Item>
  );
});
MenuItem.displayName = "Menu.Item";

/* ─── Group + GroupLabel ───────────────────────────────────────────── *
 *
 * Group wraps a logical cluster of items and auto-wires
 * `aria-labelledby` to GroupLabel inside Base UI. We only paint. */

type BaseGroupProps = ComponentPropsWithoutRef<typeof BaseMenu.Group>;
export type MenuGroupProps = BaseGroupProps;

const MenuGroup = forwardRef<HTMLDivElement, MenuGroupProps>(
  function MenuGroup({ className, ...rest }, ref) {
    return (
      <BaseMenu.Group
        ref={ref}
        className={composeBaseClass("zs-menu-group", className)}
        {...rest}
      />
    );
  },
);
MenuGroup.displayName = "Menu.Group";

type BaseGroupLabelProps = ComponentPropsWithoutRef<typeof BaseMenu.GroupLabel>;
export type MenuGroupLabelProps = BaseGroupLabelProps;

const MenuGroupLabel = forwardRef<HTMLDivElement, MenuGroupLabelProps>(
  function MenuGroupLabel({ className, ...rest }, ref) {
    return (
      <BaseMenu.GroupLabel
        ref={ref}
        className={composeBaseClass("zs-menu-group-label", className)}
        {...rest}
      />
    );
  },
);
MenuGroupLabel.displayName = "Menu.GroupLabel";

/* ─── Separator ────────────────────────────────────────────────────── *
 *
 * Base UI's menu re-exports a generic `Separator` from its top-level
 * `separator/` module — not a menu-namespaced subpart. We wrap it so
 * the consumer doesn't reach across namespaces. */

type BaseSeparatorProps = ComponentPropsWithoutRef<typeof BaseMenu.Separator>;
export type MenuSeparatorProps = BaseSeparatorProps;

const MenuSeparator = forwardRef<HTMLDivElement, MenuSeparatorProps>(
  function MenuSeparator({ className, ...rest }, ref) {
    return (
      <BaseMenu.Separator
        ref={ref}
        className={composeBaseClass("zs-menu-separator", className)}
        {...rest}
      />
    );
  },
);
MenuSeparator.displayName = "Menu.Separator";

/* ─── CheckboxItem + Indicator ─────────────────────────────────────── *
 *
 * Reuses the Checkbox accent-fill visual: the indicator gutter paints
 * an accent square when `data-checked` is on. We render the indicator
 * unconditionally with `keepMounted` so the layout doesn't shift when
 * the row flips between checked / unchecked states. Same trick
 * Select.Item uses for its checkmark gutter. */

type BaseCheckboxItemProps = ComponentPropsWithoutRef<
  typeof BaseMenu.CheckboxItem
>;
export interface MenuCheckboxItemProps extends BaseCheckboxItemProps {
  /** Trailing keyboard-shortcut hint. See `MenuItemProps.shortcut`. */
  shortcut?: ReactNode;
}

const MenuCheckboxItem = forwardRef<HTMLElement, MenuCheckboxItemProps>(
  function MenuCheckboxItem(
    { className, children, shortcut, label, ...rest },
    ref,
  ) {
    // Same typeahead-leak guard as MenuItem: project a string child
    // into `label` when a shortcut is set, so Base UI's text
    // navigation matches the row label and not the shortcut span. */
    const autoLabel =
      label === undefined &&
      shortcut !== undefined &&
      typeof children === "string"
        ? children
        : label;
    return (
      <BaseMenu.CheckboxItem
        ref={ref as Ref<HTMLDivElement>}
        className={composeBaseClass("zs-menu-item zs-menu-checkbox-item", className)}
        label={autoLabel}
        {...rest}
      >
        <span className="zs-menu-item__indicator" aria-hidden="true">
          <BaseMenu.CheckboxItemIndicator
            className="zs-menu-item__indicator-glyph"
            keepMounted
          >
            <CheckGlyph />
          </BaseMenu.CheckboxItemIndicator>
        </span>
        <span className="zs-menu-item__text">{children}</span>
        {shortcut !== undefined ? (
          <span className="zs-menu-item__shortcut" aria-hidden="true">
            {shortcut}
          </span>
        ) : null}
      </BaseMenu.CheckboxItem>
    );
  },
);
MenuCheckboxItem.displayName = "Menu.CheckboxItem";

function CheckGlyph() {
  return (
    <svg
      viewBox="0 0 16 16"
      aria-hidden="true"
      focusable="false"
    >
      <path
        d="M3.5 8.5l3 3 6-6"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.75"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

/* ─── RadioGroup + RadioItem + Indicator ───────────────────────────── *
 *
 * RadioGroup is the value-keyed parent; RadioItem stamps a value and
 * inherits selection from the parent. The indicator gutter paints a
 * filled dot when `data-checked` is on — reuses the Radio dot visual. */

type BaseRadioGroupProps = ComponentPropsWithoutRef<typeof BaseMenu.RadioGroup>;
export type MenuRadioGroupProps = BaseRadioGroupProps;

const MenuRadioGroup = forwardRef<HTMLDivElement, MenuRadioGroupProps>(
  function MenuRadioGroup({ className, ...rest }, ref) {
    return (
      <BaseMenu.RadioGroup
        ref={ref}
        className={composeBaseClass("zs-menu-radio-group", className)}
        {...rest}
      />
    );
  },
);
MenuRadioGroup.displayName = "Menu.RadioGroup";

type BaseRadioItemProps = ComponentPropsWithoutRef<typeof BaseMenu.RadioItem>;
export interface MenuRadioItemProps extends BaseRadioItemProps {
  /** Trailing keyboard-shortcut hint. See `MenuItemProps.shortcut`. */
  shortcut?: ReactNode;
}

const MenuRadioItem = forwardRef<HTMLElement, MenuRadioItemProps>(
  function MenuRadioItem(
    { className, children, shortcut, label, ...rest },
    ref,
  ) {
    // Same typeahead-leak guard as MenuItem; see MenuItem block. */
    const autoLabel =
      label === undefined &&
      shortcut !== undefined &&
      typeof children === "string"
        ? children
        : label;
    return (
      <BaseMenu.RadioItem
        ref={ref as Ref<HTMLDivElement>}
        className={composeBaseClass("zs-menu-item zs-menu-radio-item", className)}
        label={autoLabel}
        {...rest}
      >
        <span className="zs-menu-item__indicator" aria-hidden="true">
          <BaseMenu.RadioItemIndicator
            className="zs-menu-item__indicator-glyph"
            keepMounted
          >
            <RadioDotGlyph />
          </BaseMenu.RadioItemIndicator>
        </span>
        <span className="zs-menu-item__text">{children}</span>
        {shortcut !== undefined ? (
          <span className="zs-menu-item__shortcut" aria-hidden="true">
            {shortcut}
          </span>
        ) : null}
      </BaseMenu.RadioItem>
    );
  },
);
MenuRadioItem.displayName = "Menu.RadioItem";

function RadioDotGlyph() {
  return (
    <svg viewBox="0 0 16 16" aria-hidden="true" focusable="false">
      <circle cx="8" cy="8" r="3.25" fill="currentColor" />
    </svg>
  );
}

/* ─── LinkItem ─────────────────────────────────────────────────────── *
 *
 * LinkItem renders an `<a>` natively; supports `href` + `target` +
 * `rel`. `asChild` via Slot for callers passing a router-specific
 * `<Link>` (Next.js / TanStack Router / React Router). When asChild
 * is set we render through Slot with merged classNames; otherwise the
 * default `<a>` path renders via Base UI's MenuLinkItem. */

type BaseLinkItemProps = ComponentPropsWithoutRef<typeof BaseMenu.LinkItem>;
export interface MenuLinkItemProps extends BaseLinkItemProps {
  /** Render the single child element instead of our `<a>`. */
  asChild?: boolean;
  /** Trailing keyboard-shortcut hint. See `MenuItemProps.shortcut`.
   *  Ignored when `asChild` is set — caller owns the child layout. */
  shortcut?: ReactNode;
}

const MenuLinkItem = forwardRef<HTMLAnchorElement, MenuLinkItemProps>(
  function MenuLinkItem(
    { asChild = false, className, children, shortcut, label, ...rest },
    ref,
  ) {
    if (
      process.env.NODE_ENV !== "production" &&
      asChild &&
      !isValidElement(children)
    ) {
      // eslint-disable-next-line no-console
      console.error(
        "Menu.LinkItem asChild expects a single React element child; received " +
          typeof children +
          "; rendering nothing.",
      );
    }

    // Same typeahead-leak guard as MenuItem: project a string child
    // into `label` when a shortcut is set. Skipped under `asChild`
    // (the caller's element owns its own text content). */
    const autoLabel =
      !asChild &&
      label === undefined &&
      shortcut !== undefined &&
      typeof children === "string"
        ? children
        : label;

    return (
      <BaseMenu.LinkItem
        {...rest}
        ref={ref as Ref<HTMLAnchorElement>}
        className={composeBaseClass("zs-menu-item zs-menu-link-item", className)}
        label={autoLabel}
        render={(linkProps) => {
          const linkPropsRef = (linkProps as { ref?: Ref<unknown> }).ref;

          if (asChild) {
            // asChild defers row layout to the caller's element — we
            // can't safely inject the indicator/text/shortcut spans
            // around an arbitrary router <Link>. The caller owns the
            // grid lanes (or absorbs the column collapse).
            //
            // Slot composes the child's own ref via
            // `composeRefs(ourRef, getElementRef(child))` internally,
            // so we MUST NOT pre-merge the child ref here — a
            // double-compose would invoke the child ref twice on
            // every mount. Mirrors Dialog.Close (3a64a726). */
            if (!isValidElement(children)) return <></>;
            return (
              <Slot
                {...linkProps}
                ref={composeRefs(ref as Ref<unknown>, linkPropsRef)}
              >
                {children}
              </Slot>
            );
          }

          return (
            <a
              {...(linkProps as ComponentPropsWithoutRef<"a">)}
              ref={composeRefs(
                ref as Ref<HTMLAnchorElement>,
                linkPropsRef as Ref<HTMLAnchorElement>,
              )}
            >
              <span className="zs-menu-item__indicator" aria-hidden="true" />
              <span className="zs-menu-item__text">{children}</span>
              {shortcut !== undefined ? (
                <span className="zs-menu-item__shortcut" aria-hidden="true">
                  {shortcut}
                </span>
              ) : null}
            </a>
          );
        }}
      />
    );
  },
);
MenuLinkItem.displayName = "Menu.LinkItem";

/* ─── Submenu (SubmenuRoot + SubmenuTrigger composition) ───────────── *
 *
 * Submenu sugars the SubmenuRoot + SubmenuTrigger + Portal + Positioner
 * + Popup chain into a single composeable subpart. The `trigger` prop
 * is the row content (label + optional leading glyph); children are
 * the nested menu items.
 *
 * The trigger row reads as a Menu.Item with a chevron tail; the
 * chevron glyph is appended automatically so consumers don't need to
 * remember it. `side="right"` default; Floating UI flips to left when
 * the viewport edge would clip the popup. */

type BaseSubmenuRootProps = ComponentPropsWithoutRef<
  typeof BaseMenu.SubmenuRoot
>;
export interface MenuSubmenuProps
  extends Omit<BaseSubmenuRootProps, "children"> {
  /** The row content that opens the submenu (label / icons). */
  trigger: ReactNode;
  /** The nested menu items. */
  children?: ReactNode;
  /** Which side to anchor the submenu on. Default `inline-end`
   *  (visual right in LTR, visual left in RTL). Use a physical side
   *  (`right`/`left`) to opt out of the direction-aware default. */
  side?: MenuSide;
  /** Alignment along the chosen side. Default `start`. */
  align?: MenuAlign;
  /** Pixel offset between submenu trigger and popup. Default `2`. */
  sideOffset?: number;
  /** Optional data-testid on the SubmenuTrigger row. */
  "data-testid"?: string;
  /** Optional class on the SubmenuTrigger row. */
  triggerClassName?: string;
  /** Disable interaction on the trigger row. */
  disabled?: boolean;
}

function MenuSubmenu({
  trigger,
  children,
  side = "inline-end",
  align = "start",
  sideOffset = 2,
  triggerClassName,
  disabled,
  "data-testid": dataTestid,
  ...rootRest
}: MenuSubmenuProps) {
  return (
    <BaseMenu.SubmenuRoot {...rootRest}>
      <BaseMenu.SubmenuTrigger
        disabled={disabled}
        data-testid={dataTestid}
        className={composeBaseClass(
          "zs-menu-item zs-menu-submenu-trigger",
          triggerClassName,
        )}
      >
        <span className="zs-menu-item__indicator" aria-hidden="true" />
        <span className="zs-menu-item__text">{trigger}</span>
        <span className="zs-menu-submenu-trigger__chevron" aria-hidden="true">
          <ChevronRightGlyph />
        </span>
      </BaseMenu.SubmenuTrigger>
      <BaseMenu.Portal>
        <BaseMenu.Positioner
          className="zs-menu-positioner"
          side={side}
          align={align}
          sideOffset={sideOffset}
        >
          <BaseMenu.Popup className="zs-menu-popup zs-menu-popup--submenu">
            {children}
          </BaseMenu.Popup>
        </BaseMenu.Positioner>
      </BaseMenu.Portal>
    </BaseMenu.SubmenuRoot>
  );
}
MenuSubmenu.displayName = "Menu.Submenu";

function ChevronRightGlyph() {
  return (
    <svg viewBox="0 0 16 16" aria-hidden="true" focusable="false">
      <path
        d="M6 4l4 4-4 4"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

/* ─── public namespace ─────────────────────────────────────────────── */

export type MenuComponent = typeof MenuRoot & {
  Trigger: typeof MenuTrigger;
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
  createHandle: typeof createMenuHandle;
};

export const Menu = MenuRoot as MenuComponent;
Menu.Trigger = MenuTrigger;
Menu.Portal = MenuPortal;
Menu.Backdrop = MenuBackdrop;
Menu.Popup = MenuPopup;
Menu.Item = MenuItem;
Menu.Group = MenuGroup;
Menu.GroupLabel = MenuGroupLabel;
Menu.Separator = MenuSeparator;
Menu.CheckboxItem = MenuCheckboxItem;
Menu.RadioGroup = MenuRadioGroup;
Menu.RadioItem = MenuRadioItem;
Menu.LinkItem = MenuLinkItem;
Menu.Submenu = MenuSubmenu;
Menu.Arrow = MenuArrow;
Menu.createHandle = createMenuHandle;

/* ─── re-exports for ContextMenu consumption ───────────────────────── *
 *
 * ContextMenu (Slice 11) is a thin trigger over Menu — its Backdrop /
 * Portal / Popup / Item / Group / GroupLabel / Separator /
 * CheckboxItem / RadioGroup / RadioItem / LinkItem / Submenu / Arrow
 * are the same primitives. The ContextMenu component re-exports these
 * by reference; this block tags the symbols so the ContextMenu file
 * can import them by name without re-declaring the wrappers. */
export {
  MenuRoot,
  MenuTrigger,
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
};
