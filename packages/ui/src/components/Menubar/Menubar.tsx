/*
 * Menubar — desktop menubar strip of menu triggers (the horizontal File / Edit /
 * View row at the top of a typical desktop application window).
 *
 *   <Menubar>
 *     <Menu>
 *       <Menu.Trigger data-chrome="menubar">File</Menu.Trigger>
 *       <Menu.Portal>
 *         <Menu.Popup data-chrome="menubar">
 *           <Menu.Item>New</Menu.Item>
 *           …
 *         </Menu.Popup>
 *       </Menu.Portal>
 *     </Menu>
 *
 *     <Menu>
 *       <Menu.Trigger data-chrome="menubar">Edit</Menu.Trigger>
 *       …
 *     </Menu>
 *   </Menubar>
 *
 * Shape decisions:
 *   - The component is a thin wrapper around Base UI's `Menubar` primitive.
 *     Menubar's superpower is auto-open-on-hover-after-first-click: once any
 *     trigger has been clicked open, hovering the sibling triggers swaps
 *     the open menu without an intermediate click. Base UI owns that
 *     behavior; we don't override it (brief contingency).
 *   - Children are wrapped `Menu` siblings from `@zeroship/ui`. The
 *     Menubar-flavored trigger / popup chrome is selected by the
 *     `data-chrome="menubar"` attribute that the Menu wrapper passes
 *     through transparently — `Menu.css` paints those variants. There
 *     is no separate `.zs-menubar-menu*` class set.
 *   - Keyboard contract: Tab focuses the menubar; arrow-left/right rove
 *     between triggers; ArrowDown opens the focused menu and moves to
 *     its first item; Esc closes the open menu. Base UI implements the
 *     roving and the loop-to-first/last behaviour — we surface `loopFocus`
 *     as a prop so consumers can flatten it.
 *   - `modal` defaults to `false`. Base UI's Menubar defaults `modal` to
 *     `true`, which scroll-locks the page and installs an interaction
 *     scrim every time a menu opens — that's appropriate for dialogs but
 *     wrong for a desktop menubar pinned to an application chrome. The
 *     wrapper explicitly defaults `modal = false` so omitted-prop callers
 *     get the non-modal desktop-style behavior; consumers who want the
 *     modal scrim can opt in with `modal`.
 *
 * Anti-patterns we explicitly avoid:
 *   - Painting a hover-only open without a first-click affordance. Base
 *     UI's gating (first click arms, subsequent hovers swap) is correct
 *     for a desktop menubar; we keep it.
 *   - Spreading `role` OR `render` from props. Menubar locks
 *     `role="menubar"` via the underlying Base UI primitive; consumers
 *     can't override it. The TS-level `Omit<…, "render" | "role">` and
 *     the runtime strip in MenubarRoot together enforce the contract.
 *     Base UI's `mergeProps` puts caller-passed element props last, so
 *     an unguarded spread would otherwise let `<Menubar role="…">` win;
 *     equally important, an unguarded `render={(props) => <nav …>}`
 *     injected via an untyped spread would replace our `<div>` with a
 *     caller-chosen tag, silently dropping the locked role. Stripping
 *     `render` at runtime closes that bypass — if `asChild`-style
 *     composition is needed it must be added explicitly via `_slot.ts`
 *     (mirror Dialog.Close commit 3a64a726), not via the raw Base UI
 *     render prop.
 *   - Adding a click-outside backdrop. Menubar inherits Base UI's
 *     dismiss-on-outside-click; no scrim needed.
 *   - Setting `data-orientation` explicitly on the root. Base UI's
 *     Menubar already emits the attribute from the `orientation` prop.
 *   - Letting `aria-orientation` be caller-overridden. Base UI computes
 *     `aria-orientation` from the `orientation` prop; a contradictory
 *     `<Menubar orientation="horizontal" aria-orientation="vertical">`
 *     would ship inconsistent ARIA. We omit the attribute from the public
 *     type and strip it at runtime (mirror the Toolbar pattern).
 */
import { forwardRef, type ComponentPropsWithoutRef, type ReactNode } from "react";
import { Menubar as BaseMenubar } from "@base-ui/react/menubar";
import { classnames, composeBaseClass } from "../_classnames";

export type MenubarOrientation = "horizontal" | "vertical";

type BaseMenubarProps = ComponentPropsWithoutRef<typeof BaseMenubar>;

/**
 * Public Menubar props. We deliberately omit:
 *
 *   - `render`: we own the rendering surface so the role contract sticks.
 *     The `Omit` here is the compile-time guard; the runtime strip in
 *     `MenubarRoot` is the spread-injection guard so an untyped
 *     `<Menubar {...untypedProps}>` carrying `render={…}` can NOT
 *     replace our `<div>` and ship a different element (e.g. `<nav>`)
 *     that would silently lose the `role="menubar"` lock.
 *   - `role`: LOCKED to `"menubar"` by Base UI. Surfacing it as a prop
 *     would be misleading, and Base UI's `mergeProps` puts caller-passed
 *     element props last (rightmost-wins), so an unguarded `<Menubar
 *     role="…">` would otherwise overwrite the contract. The `Omit` here
 *     plus the runtime strip in `MenubarRoot` together enforce it.
 *   - `aria-orientation`: LOCKED to track the `orientation` prop. Base
 *     UI emits the attribute itself from `orientation`; surfacing it
 *     would let `<Menubar orientation="horizontal"
 *     aria-orientation="vertical">` ship inconsistent ARIA. The `Omit`
 *     plus the runtime strip in `MenubarRoot` together enforce it.
 *
 * The supported pass-through props (`modal`, `loopFocus`, `disabled`) are
 * redeclared below so Storybook autodocs picks up local descriptions /
 * defaults instead of opaque inherited types. Other data-attribute escape
 * hatches remain available via the remaining inherited surface.
 */
export interface MenubarProps
  extends Omit<BaseMenubarProps, "render" | "role" | "aria-orientation"> {
  /**
   * Layout axis.
   *
   * - `horizontal` (default) — the classic desktop menubar strip;
   *   arrow-left/right roves between triggers.
   * - `vertical` — stacked triggers, arrow-up/down roves. Useful for
   *   left-rail menus inside complex authoring tools.
   *
   * @default "horizontal"
   */
  orientation?: MenubarOrientation;
  /**
   * Modal behavior of the open menu.
   *
   * - `false` (default) — non-modal: the page does NOT scroll-lock and
   *   the open popup does NOT install an interaction scrim. This is the
   *   correct shape for a desktop-style menubar pinned to application
   *   chrome. Note this OVERRIDES Base UI's `modal=true` default.
   * - `true` — modal: opens with a backdrop, scroll-locks the page, and
   *   blocks pointer interaction with the rest of the document while a
   *   menu is open. Opt in only for menus that own the user's attention
   *   (e.g., a destructive confirm flow exposed through the menubar).
   *
   * @default false
   */
  modal?: boolean;
  /**
   * Whether arrow-key roving loops at the strip's edges.
   *
   * - `true` (default) — ArrowRight at the last trigger wraps to the
   *   first; ArrowLeft at the first wraps to the last. Matches the
   *   desktop-menubar convention.
   * - `false` — roving stops at the edges. Useful if the menubar is
   *   embedded in a larger composite where Tab/Shift+Tab should escape
   *   the strip on edge-arrow.
   *
   * @default true
   */
  loopFocus?: boolean;
  /**
   * Whether the entire menubar is disabled.
   *
   * When `true`, Base UI marks every trigger inside the menubar with
   * `data-disabled` so the chrome (`.zs-menubar` trigger paint) reads
   * disabled across the strip, and pointer / keyboard activation no
   * longer opens any menu.
   *
   * @default false
   */
  disabled?: boolean;
  /** Optional class hook on the root. */
  className?: string;
  /** Menu siblings — typically `<Menu>` from `@zeroship/ui`. */
  children?: ReactNode;
}

const MenubarRoot = forwardRef<HTMLDivElement, MenubarProps>(
  function MenubarRoot(props, ref) {
    const {
      orientation = "horizontal",
      className,
      children,
      // Explicit `modal = false` — Base UI's Menubar defaults `modal`
      // to `true`, which scroll-locks the page and installs an
      // interaction scrim on every open. That's wrong for a desktop
      // menubar pinned to application chrome; we flip the default so
      // omitted-prop callers get the non-modal behavior, and consumers
      // who want the modal scrim opt in via `modal`.
      modal = false,
      loopFocus,
      disabled,
      ...rest
    } = props;
    // Strip `role`, `render`, AND `aria-orientation` defensively in case
    // a caller bypasses the type system (e.g., `<Menubar
    // {...untypedProps}>`). The `Omit<…, "render" | "role" |
    // "aria-orientation">` above makes them compile-time errors in
    // normal use; this guard keeps the contract intact under runtime
    // spread.
    //
    // Why strip `render`: Base UI's Menubar accepts a `render` prop
    // that REPLACES the default `<div>` element entirely (it is the
    // documented escape hatch for `asChild`-style composition). An
    // unstripped `render={(props) => <nav {...props} />}` injected via
    // an untyped spread would ship an element whose tag is whatever
    // the caller chose, and Base UI's `mergeProps` rightmost-wins
    // ordering would then let `role` on that render-prop element
    // silently overrule the locked `role="menubar"`. Stripping `render`
    // here closes the bypass — paired with the type-level `Omit`, an
    // untyped consumer's `render={…}` is dropped at the wrapper layer
    // and Base UI renders its own `<div role="menubar">`. Pre-launch
    // no-back-compat: if a consumer needs render composition, it should
    // be added as an explicit `asChild` API routed through `_slot.ts`
    // (mirror Dialog.Close commit 3a64a726), not smuggled in via the
    // raw Base UI render prop.
    //
    // Why strip `aria-orientation`: Base UI computes `aria-orientation`
    // from the `orientation` prop, so letting a caller-passed
    // `aria-orientation` through `mergeProps` (rightmost-wins) would
    // ship inconsistent ARIA (e.g. `orientation="horizontal"
    // aria-orientation="vertical"`). Mirror the Toolbar pattern.
    const {
      role: _role,
      render: _render,
      "aria-orientation": _ariaOrientation,
      ...restNoRoleNoRenderNoAriaOrientation
    } = rest as Record<string, unknown> & {
      role?: string;
      render?: unknown;
      "aria-orientation"?: string;
    };
    void _role;
    void _render;
    void _ariaOrientation;
    return (
      <BaseMenubar
        {...(restNoRoleNoRenderNoAriaOrientation as BaseMenubarProps)}
        ref={ref}
        orientation={orientation}
        modal={modal}
        loopFocus={loopFocus}
        disabled={disabled || undefined}
        className={composeBaseClass(
          classnames("zs-menubar", `zs-menubar--${orientation}`),
          className,
        )}
      >
        {children}
      </BaseMenubar>
    );
  },
);
MenubarRoot.displayName = "Menubar";

export const Menubar = MenubarRoot;
