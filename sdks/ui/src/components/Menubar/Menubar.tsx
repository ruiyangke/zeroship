/*
 * Menubar — macOS-style horizontal strip of menu triggers.
 *
 *   <Menubar>
 *     <Menu.Root>
 *       <Menu.Trigger>File</Menu.Trigger>
 *       <Menu.Portal>
 *         <Menu.Positioner>
 *           <Menu.Popup>
 *             <Menu.Item>New</Menu.Item>
 *             …
 *           </Menu.Popup>
 *         </Menu.Positioner>
 *       </Menu.Portal>
 *     </Menu.Root>
 *
 *     <Menu.Root>
 *       <Menu.Trigger>Edit</Menu.Trigger>
 *       …
 *     </Menu.Root>
 *   </Menubar>
 *
 * Shape decisions:
 *   - The component is a thin wrapper around Base UI's `Menubar` primitive.
 *     Menubar's superpower is auto-open-on-hover-after-first-click: once any
 *     trigger has been clicked open, hovering the sibling triggers swaps
 *     the open menu without an intermediate click. Base UI owns that
 *     behavior; we don't override it (brief contingency).
 *   - The children are standard Menu siblings. Slice 11 ships our wrapped
 *     `Menu.*`; until then stories use Base UI's `Menu` directly. Because
 *     Menubar only enforces structure at the root level, both wrapped and
 *     bare-Base-UI menus compose identically as siblings.
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
 *   - Adding a click-outside backdrop. Menubar inherits Base UI's
 *     dismiss-on-outside-click; no scrim needed.
 */
import { forwardRef, type ComponentPropsWithoutRef, type ReactNode } from "react";
import { Menubar as BaseMenubar } from "@base-ui/react/menubar";
import { classnames, composeBaseClass } from "../_classnames";

export type MenubarOrientation = "horizontal" | "vertical";

type BaseMenubarProps = ComponentPropsWithoutRef<typeof BaseMenubar>;

/**
 * Public Menubar props. We omit `render` (we own the rendering surface so
 * the role contract sticks) but otherwise forward Base UI's prop set so
 * `modal`, `loopFocus`, `disabled`, and the data-attribute escape hatches
 * remain available.
 */
export interface MenubarProps extends Omit<BaseMenubarProps, "render"> {
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
  /** Menu siblings — typically `<Menu.Root>` from `@zeroship/ui`. */
  children?: ReactNode;
}

const MenubarRoot = forwardRef<HTMLDivElement, MenubarProps>(
  function MenubarRoot(
    {
      orientation = "horizontal",
      className,
      children,
      modal,
      loopFocus,
      disabled,
      ...rest
    },
    ref,
  ) {
    return (
      <BaseMenubar
        {...rest}
        ref={ref}
        orientation={orientation}
        modal={modal}
        loopFocus={loopFocus}
        disabled={disabled || undefined}
        className={composeBaseClass(
          classnames("zs-menubar", `zs-menubar--${orientation}`),
          className,
        )}
        data-orientation={orientation}
      >
        {children}
      </BaseMenubar>
    );
  },
);
MenubarRoot.displayName = "Menubar";

export const Menubar = MenubarRoot;
