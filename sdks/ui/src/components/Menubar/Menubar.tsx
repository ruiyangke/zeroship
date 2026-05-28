/*
 * Menubar — macOS-style horizontal strip of menu triggers.
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
 *
 * Anti-patterns we explicitly avoid:
 *   - Painting a hover-only open without a first-click affordance. Base
 *     UI's gating (first click arms, subsequent hovers swap) is correct
 *     and matches the macOS menubar; we keep it.
 *   - Spreading `role` from props. Menubar locks `role="menubar"` via
 *     the underlying Base UI primitive; consumers can't override it.
 *     The TS-level `Omit<…, "role">` and the runtime strip together
 *     enforce the contract — Base UI's `mergeProps` puts caller-passed
 *     element props last, so an unguarded spread would otherwise let
 *     `<Menubar role="…">` win.
 *   - Adding a click-outside backdrop. Menubar inherits Base UI's
 *     dismiss-on-outside-click; no scrim needed.
 *   - Setting `data-orientation` explicitly on the root. Base UI's
 *     Menubar already emits the attribute from the `orientation` prop.
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
 *   - `role`: LOCKED to `"menubar"` by Base UI. Surfacing it as a prop
 *     would be misleading, and Base UI's `mergeProps` puts caller-passed
 *     element props last (rightmost-wins), so an unguarded `<Menubar
 *     role="…">` would otherwise overwrite the contract. The `Omit` here
 *     plus the runtime strip in `MenubarRoot` together enforce it.
 *
 * Everything else passes through so `modal`, `loopFocus`, `disabled`, and
 * the data-attribute escape hatches remain available.
 */
export interface MenubarProps extends Omit<BaseMenubarProps, "render" | "role"> {
  /**
   * Layout axis.
   *
   * - `horizontal` (default) — the classic macOS strip; arrow-left/right
   *   roves between triggers.
   * - `vertical` — stacked triggers, arrow-up/down roves. Useful for
   *   left-rail menus inside complex authoring tools.
   *
   * @default "horizontal"
   */
  orientation?: MenubarOrientation;
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
      modal,
      loopFocus,
      disabled,
      ...rest
    } = props;
    // Strip `role` defensively in case a caller bypasses the type system
    // (e.g., `<Menubar {...untypedProps}>`). The `Omit<…, "role">` above
    // makes this a compile-time error in normal use; this guard keeps
    // the contract intact under runtime spread.
    const { role: _role, ...restNoRole } = rest as Record<string, unknown> & {
      role?: string;
    };
    void _role;
    return (
      <BaseMenubar
        {...(restNoRole as BaseMenubarProps)}
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
