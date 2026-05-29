/*
 * Stack — one-dimensional flex arrangement.
 *
 * The workhorse layout primitive: lay children out along one axis
 * (column by default), separated by a governed `gap` from the
 * `--zs-space-*` scale. A Stack paints NOTHING — no background, no
 * border, no padding of its own. It only arranges.
 *
 * Design guarantees encoded here (in source so they travel with the
 * code, not in a sibling doc):
 *
 *   1. Spacing is governed. `gap` is the closed `Gap` union mapped to
 *      `--zs-space-*` via `spaceVar`; a consumer cannot pass an
 *      arbitrary px/rem value. This is the governance lever — layouts
 *      stay on the 4pt grid.
 *
 *   2. Alignment flows through the shared `alignValue` / `justifyValue`
 *      maps so `align`/`justify` read the same on Stack, Grid, Cluster,
 *      and Center. No per-component flex-keyword drift.
 *
 *   3. Layout is driven by inline CSS custom properties set from props
 *      (`--stack-gap`, `--stack-direction`, …) and consumed by
 *      `Stack.css`. We never write raw flex values inline — the CSS
 *      owns the property names, the component owns the values.
 *
 *   4. `asChild` routes through `Slot` (React-19-safe refs) so a Stack
 *      can render-as a `<nav>`, `<ul>`, `<section>`, etc. without a
 *      wrapper div. We never hand-roll `cloneElement`.
 *
 *   5. RTL is automatic. `flex-direction: row` already follows the
 *      writing direction, so a row Stack flips inline order under
 *      `dir="rtl"` with no extra rule.
 */
import {
  forwardRef,
  isValidElement,
  type ComponentPropsWithoutRef,
  type Ref,
} from "react";
import { Slot } from "../../components/_slot";
import { classnames } from "../../components/_classnames";
import {
  type Gap,
  type Align,
  type Justify,
  spaceVar,
  alignValue,
  justifyValue,
} from "../_layout-primitives";

export interface StackProps extends ComponentPropsWithoutRef<"div"> {
  /** Main axis. `column` (default) stacks vertically; `row` lays out inline. */
  direction?: "row" | "column";
  /** Gap between children, from the `--zs-space-*` scale. Default `0`. */
  gap?: Gap;
  /** Cross-axis alignment (`align-items`). */
  align?: Align;
  /** Main-axis distribution (`justify-content`). */
  justify?: Justify;
  /** Allow children to wrap onto multiple lines (`flex-wrap: wrap`). */
  wrap?: boolean;
  /**
   * Render-as the single child element rather than a `<div>` (e.g. a
   * `<nav>` / `<ul>` / `<section>`). Routed through `Slot` so className /
   * style / refs compose under React 19.
   */
  asChild?: boolean;
  /**
   * Root `data-slot` value. Defaults to `"stack"`. A composing block
   * (e.g. `PageHeader.Text`) overrides it so consumers can target the
   * outer element via the block's own slot vocabulary. Mirrors Card.
   */
  "data-slot"?: string;
}

/**
 * Vertical (or horizontal) stack of children with a governed gap.
 * Arrange-only: no paint.
 */
export const Stack = forwardRef<HTMLDivElement, StackProps>(function Stack(
  {
    direction = "column",
    gap = 0,
    align,
    justify,
    wrap = false,
    asChild = false,
    className,
    style,
    children,
    "data-slot": dataSlot = "stack",
    ...rest
  },
  ref,
) {
  // Dev-mode parity with Card: when asChild is set but the child isn't a
  // single valid React element, Slot renders nothing silently — warn so
  // the disappearance is diagnosable. Gated on process.env.NODE_ENV so
  // the branch DCEs out of production builds.
  if (process.env.NODE_ENV !== "production") {
    if (asChild && !isValidElement(children)) {
      // eslint-disable-next-line no-console
      console.warn(
        "Stack asChild requires a single React element child; rendering nothing.",
      );
    }
  }

  const composedClassName = classnames("zs-stack", className);

  // Layout values flow as inline custom properties consumed by Stack.css.
  // Only set a property when the prop is supplied so the CSS defaults win.
  const layoutVars: React.CSSProperties = {
    "--stack-direction": direction,
    "--stack-gap": spaceVar(gap),
    "--stack-wrap": wrap ? "wrap" : "nowrap",
    ...(align ? { "--stack-align": alignValue(align) } : null),
    ...(justify ? { "--stack-justify": justifyValue(justify) } : null),
    ...style,
  } as React.CSSProperties;

  const Comp = asChild ? Slot : "div";

  return (
    <Comp
      {...rest}
      ref={ref as Ref<HTMLDivElement>}
      data-slot={dataSlot}
      className={composedClassName}
      style={layoutVars}
    >
      {children}
    </Comp>
  );
});
Stack.displayName = "Stack";
