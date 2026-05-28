/*
 * Toolbar — `role="toolbar"` row that groups buttons, segmented controls,
 * and separators into a single keyboard-navigable cluster.
 *
 *   <Toolbar>
 *     <Button>Bold</Button>
 *     <Button>Italic</Button>
 *     <Toolbar.Separator />
 *     <Toggle.Group … />
 *     <Toolbar.Separator />
 *     <Button>Save</Button>
 *   </Toolbar>
 *
 * Shape decisions:
 *   - Thin wrapper around Base UI's `Toolbar.Root` / `Toolbar.Separator`
 *     primitives. Base UI ships those in 1.5; the brief contingency for a
 *     hand-rolled `role="toolbar"` path is therefore unused. We only need
 *     to add styling + the `Toggle.Group` interop note (Toggle.Group also
 *     paints `role="toolbar"`; when nested inside this Toolbar it inherits
 *     the parent's roving-tabindex and becomes part of the same Tab stop).
 *   - `Toolbar` LOCKS `role="toolbar"` via the underlying Base UI primitive.
 *     Consumers can't override it — that's the contract.
 *   - `Toolbar.Separator` paints a hairline (vertical or — when
 *     `orientation="vertical"` — horizontal) between groups of controls.
 *     `aria-orientation` flips to match the toolbar so it reads as a
 *     visual cluster boundary, not as a structural divider.
 *   - Items inside a Toolbar use roving tabindex (Base UI). Tab enters
 *     the toolbar at the first focusable item; arrow keys roam between
 *     items; Tab leaves the toolbar.
 *
 * Anti-patterns we explicitly avoid:
 *   - Spreading the consumer's `role` onto the root. The component name
 *     is "Toolbar"; semantics travel with the name. We omit `role` from
 *     the public props at the TypeScript level.
 *   - Painting a separator with `<hr>` — Base UI emits a `<div>` with
 *     the right ARIA so the cluster shorthand is unbroken inside the
 *     focus traversal.
 */
import { forwardRef, type ComponentPropsWithoutRef, type ReactNode } from "react";
import { Toolbar as BaseToolbar } from "@base-ui/react/toolbar";
import { classnames, composeBaseClass } from "../_classnames";

export type ToolbarOrientation = "horizontal" | "vertical";

type BaseToolbarRootProps = ComponentPropsWithoutRef<typeof BaseToolbar.Root>;

/**
 * Public Toolbar root props. We deliberately omit `render` (we own the
 * `<div>` rendering surface so the role contract sticks) and `role`
 * (LOCKED to `"toolbar"` by Base UI; surfacing it as a prop would be
 * misleading).
 */
export interface ToolbarProps extends Omit<BaseToolbarRootProps, "render"> {
  /**
   * Layout axis.
   *
   * - `horizontal` (default) — items flow inline; arrow-left/right roves.
   * - `vertical` — items stack block-wise; arrow-up/down roves.
   *
   * Forwarded as `aria-orientation` on the root by Base UI so screen
   * readers announce the direction.
   *
   * @default "horizontal"
   */
  orientation?: ToolbarOrientation;
  /**
   * When `true` (default), arrow-key roving wraps from the last item
   * back to the first (and vice versa). When `false`, the focus stops
   * at the ends. Forwarded to Base UI's `loopFocus`.
   *
   * @default true
   */
  loopFocus?: boolean;
  /** Optional class hook on the toolbar root. */
  className?: string;
  /** Toolbar contents — Buttons, Toggle.Group, Toolbar.Separator. */
  children?: ReactNode;
}

/* ─── Root ──────────────────────────────────────────────────────────── */

const ToolbarRoot = forwardRef<HTMLDivElement, ToolbarProps>(
  function ToolbarRoot(
    { orientation = "horizontal", loopFocus = true, className, children, ...rest },
    ref,
  ) {
    return (
      <BaseToolbar.Root
        {...rest}
        ref={ref}
        orientation={orientation}
        loopFocus={loopFocus}
        className={composeBaseClass(
          classnames("zs-toolbar", `zs-toolbar--${orientation}`),
          className,
        )}
        data-orientation={orientation}
      >
        {children}
      </BaseToolbar.Root>
    );
  },
);
ToolbarRoot.displayName = "Toolbar";

/* ─── Separator ─────────────────────────────────────────────────────── *
 *
 * Cluster boundary inside the toolbar — `aria-orientation` is the
 * perpendicular of the toolbar axis (a vertical hairline inside a
 * horizontal toolbar, and vice versa). Base UI handles the ARIA wiring;
 * we just paint. */

type BaseSeparatorProps = ComponentPropsWithoutRef<typeof BaseToolbar.Separator>;
export type ToolbarSeparatorProps = BaseSeparatorProps;

const ToolbarSeparator = forwardRef<HTMLDivElement, ToolbarSeparatorProps>(
  function ToolbarSeparator({ className, ...rest }, ref) {
    return (
      <BaseToolbar.Separator
        {...rest}
        ref={ref}
        className={composeBaseClass("zs-toolbar-separator", className)}
      />
    );
  },
);
ToolbarSeparator.displayName = "Toolbar.Separator";

/* ─── public namespace ──────────────────────────────────────────────── */

export type ToolbarComponent = typeof ToolbarRoot & {
  Separator: typeof ToolbarSeparator;
};

export const Toolbar = ToolbarRoot as ToolbarComponent;
Toolbar.Separator = ToolbarSeparator;
