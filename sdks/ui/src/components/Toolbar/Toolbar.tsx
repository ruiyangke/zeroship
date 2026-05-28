/*
 * Toolbar — `role="toolbar"` row that groups buttons, segmented controls,
 * and separators into a single keyboard-navigable cluster.
 *
 *   <Toolbar>
 *     <Toolbar.Button>Bold</Toolbar.Button>
 *     <Toolbar.Button>Italic</Toolbar.Button>
 *     <Toolbar.Separator />
 *     <Toggle.Group … />
 *     <Toolbar.Separator />
 *     <Toolbar.Button>Save</Toolbar.Button>
 *   </Toolbar>
 *
 * Shape decisions:
 *   - Thin wrapper around Base UI's `Toolbar.Root` / `Toolbar.Button` /
 *     `Toolbar.Link` / `Toolbar.Separator` primitives. Each item subpart
 *     registers with Base UI's composite-item context so the roving
 *     tabindex + disabled-skip behavior actually fires.
 *   - `Toolbar.Button` is the canonical item — a real `<button>` that
 *     accepts the same className / startSlot / endSlot props you'd use
 *     on a plain Button, but participates in the toolbar's composite
 *     focus. Plain `<Button>` children do NOT rove; the type signature
 *     on Toolbar's children prop documents that. Stories and the
 *     `Toolbar.Button` JSDoc point consumers at the right shape.
 *   - `Toolbar` LOCKS `role="toolbar"` via the underlying Base UI primitive.
 *     Consumers can't override it — that's the contract, and we strip
 *     any user-passed `role` at the TS layer (`Omit<…, "role">`) and at
 *     runtime so Base UI's `mergeProps` rightmost-wins ordering can't
 *     leak the override.
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
 *     the public props at the TypeScript level AND strip any `role` at
 *     runtime before forwarding into Base UI.
 *   - Painting a separator with `<hr>` — Base UI emits a `<div>` with
 *     the right ARIA so the cluster shorthand is unbroken inside the
 *     focus traversal.
 *   - Setting `data-orientation` explicitly on the root. Base UI's
 *     ToolbarRoot already emits the attribute from the `orientation`
 *     prop; doubling up risks drift.
 */
import { forwardRef, type ComponentPropsWithoutRef, type ReactNode } from "react";
import { Toolbar as BaseToolbar } from "@base-ui/react/toolbar";
import { classnames, composeBaseClass } from "../_classnames";

export type ToolbarOrientation = "horizontal" | "vertical";

type BaseToolbarRootProps = ComponentPropsWithoutRef<typeof BaseToolbar.Root>;

/**
 * Public Toolbar root props. We deliberately omit:
 *
 *   - `render`: we own the `<div>` rendering surface so the role
 *     contract sticks.
 *   - `role`: LOCKED to `"toolbar"` by Base UI. Surfacing it as a prop
 *     would be misleading, and Base UI's `mergeProps` puts caller-passed
 *     element props last (rightmost-wins), so a `<Toolbar role="…">`
 *     consumer would otherwise overwrite the contract. The `Omit` here
 *     plus the runtime strip in `ToolbarRoot` together enforce it.
 */
export interface ToolbarProps
  extends Omit<BaseToolbarRootProps, "render" | "role"> {
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
  /**
   * Toolbar contents — `Toolbar.Button`, `Toolbar.Link`, `Toolbar.Input`,
   * `Toggle.Group`, `Toolbar.Separator`. Plain `<Button>` children do NOT
   * participate in the roving tabindex; use `Toolbar.Button` so the
   * composite-item context registers the focus stop.
   */
  children?: ReactNode;
}

/* ─── Root ──────────────────────────────────────────────────────────── */

const ToolbarRoot = forwardRef<HTMLDivElement, ToolbarProps>(
  function ToolbarRoot(props, ref) {
    const {
      orientation = "horizontal",
      loopFocus = true,
      className,
      children,
      ...rest
    } = props;
    // Strip `role` defensively in case a caller bypasses the type system
    // (e.g., `<Toolbar {...untypedProps}>`). The `Omit<…, "role">` above
    // makes this a compile-time error in normal use; this guard keeps
    // the contract intact under runtime spread.
    const { role: _role, ...restNoRole } = rest as Record<string, unknown> & {
      role?: string;
    };
    void _role;
    return (
      <BaseToolbar.Root
        {...(restNoRole as BaseToolbarRootProps)}
        ref={ref}
        orientation={orientation}
        loopFocus={loopFocus}
        className={composeBaseClass(
          classnames("zs-toolbar", `zs-toolbar--${orientation}`),
          className,
        )}
      >
        {children}
      </BaseToolbar.Root>
    );
  },
);
ToolbarRoot.displayName = "Toolbar";

/* ─── Button ────────────────────────────────────────────────────────── *
 *
 * The canonical Toolbar item. A real `<button>` that registers with
 * Base UI's composite-item context — that's what makes arrow-key
 * roving and disabled-skip work. Plain `<Button>` children do NOT
 * register, so they break the cluster keyboard model.
 *
 * Styling matches the gray variant of Button (low-chrome, label-only
 * paint until hover) so the canonical toolbar item reads visually
 * identical to the rest of the package. */

type BaseButtonProps = ComponentPropsWithoutRef<typeof BaseToolbar.Button>;
export type ToolbarButtonProps = BaseButtonProps;

const ToolbarButton = forwardRef<HTMLButtonElement, ToolbarButtonProps>(
  function ToolbarButton({ className, ...rest }, ref) {
    return (
      <BaseToolbar.Button
        {...rest}
        ref={ref}
        className={composeBaseClass("zs-toolbar-button", className)}
      />
    );
  },
);
ToolbarButton.displayName = "Toolbar.Button";

/* ─── Link ──────────────────────────────────────────────────────────── *
 *
 * Anchor-flavored Toolbar item. Renders `<a>`; participates in roving. */

type BaseLinkProps = ComponentPropsWithoutRef<typeof BaseToolbar.Link>;
export type ToolbarLinkProps = BaseLinkProps;

const ToolbarLink = forwardRef<HTMLAnchorElement, ToolbarLinkProps>(
  function ToolbarLink({ className, ...rest }, ref) {
    return (
      <BaseToolbar.Link
        {...rest}
        ref={ref}
        className={composeBaseClass("zs-toolbar-button zs-toolbar-link", className)}
      />
    );
  },
);
ToolbarLink.displayName = "Toolbar.Link";

/* ─── Input ─────────────────────────────────────────────────────────── *
 *
 * Inline text input that lives inside a Toolbar cluster (e.g., a quick-
 * filter field next to row of action buttons). Participates in roving;
 * Base UI handles the focus management so the input gets the cluster's
 * Tab stop while internal cursor navigation isn't hijacked. */

type BaseInputProps = ComponentPropsWithoutRef<typeof BaseToolbar.Input>;
export type ToolbarInputProps = BaseInputProps;

const ToolbarInput = forwardRef<HTMLInputElement, ToolbarInputProps>(
  function ToolbarInput({ className, ...rest }, ref) {
    return (
      <BaseToolbar.Input
        {...rest}
        ref={ref}
        className={composeBaseClass("zs-toolbar-input", className)}
      />
    );
  },
);
ToolbarInput.displayName = "Toolbar.Input";

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
  Button: typeof ToolbarButton;
  Link: typeof ToolbarLink;
  Input: typeof ToolbarInput;
  Separator: typeof ToolbarSeparator;
};

export const Toolbar = ToolbarRoot as ToolbarComponent;
Toolbar.Button = ToolbarButton;
Toolbar.Link = ToolbarLink;
Toolbar.Input = ToolbarInput;
Toolbar.Separator = ToolbarSeparator;
