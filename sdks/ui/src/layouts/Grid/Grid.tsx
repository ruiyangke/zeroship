/*
 * Grid — two-dimensional grid arrangement.
 *
 * Two ways to size columns, in priority order:
 *
 *   1. `minColWidth` (PREFERRED — breakpoint-free, intrinsic). Sets
 *      `repeat(auto-fit, minmax(<minColWidth>, 1fr))` so the grid packs
 *      as many equal columns as fit and reflows fluidly with no media
 *      queries. Reach for this first — it is responsive by construction.
 *      When set, `columns` is ignored.
 *
 *   2. `columns` (explicit count). A number fixes the column count at
 *      every width. A `{ sm, md, lg }` object changes the count at the
 *      governed breakpoints (--zs-bp-sm/md/lg) via media queries in
 *      Grid.css. The base (mobile-first) count is always 1; each
 *      provided breakpoint promotes the count upward from there.
 *
 * A Grid paints NOTHING — no background, no border. It only arranges.
 *
 * Design guarantees:
 *   - `gap` is the closed `Gap` union → `--zs-space-*` via `spaceVar`;
 *     no arbitrary spacing.
 *   - `align` flows through the shared `alignValue` map (align-items).
 *   - Responsive `columns` is set through data-attributes consumed by
 *     Grid.css media queries; the breakpoint rem literals there carry a
 *     comment naming the --zs-bp-* token (custom properties can't appear
 *     in @media conditions).
 *   - `asChild` routes through `Slot` (React-19-safe refs).
 */
import {
  forwardRef,
  isValidElement,
  type ComponentPropsWithoutRef,
  type Ref,
} from "react";
import { Slot } from "../../components/_slot";
import { classnames } from "../../components/_classnames";
import { type Gap, type Align, spaceVar, alignValue } from "../_layout-primitives";

/**
 * Responsive column counts keyed to the governed breakpoints. The base
 * (below `sm`) is always 1; each provided breakpoint promotes the count
 * upward from there (mobile-first).
 */
export interface GridColumns {
  /** Columns at the --zs-bp-sm breakpoint and up. */
  sm?: number;
  /** Columns at the --zs-bp-md breakpoint and up. */
  md?: number;
  /** Columns at the --zs-bp-lg breakpoint and up. */
  lg?: number;
}

export interface GridProps extends ComponentPropsWithoutRef<"div"> {
  /**
   * Column count. A number fixes the count at all widths. A
   * `{ sm, md, lg }` object is mobile-first: the base (below `sm`) is
   * always 1 and each provided breakpoint promotes the count upward, so
   * `{ lg: 4 }` is 1 column until --zs-bp-lg, then 4. Default `1`.
   * Ignored when `minColWidth` is set.
   */
  columns?: number | GridColumns;
  /** Gap between cells, from the `--zs-space-*` scale. */
  gap?: Gap;
  /** Cross-axis alignment of cells within their tracks (`align-items`). */
  align?: Align;
  /** Grid auto-placement flow (`grid-auto-flow`). */
  flow?: "row" | "column" | "dense";
  /**
   * Intrinsic, breakpoint-free sizing. When set, the grid becomes
   * `repeat(auto-fit, minmax(<minColWidth>, 1fr))` and `columns` is
   * ignored. Pass a CSS length (e.g. `"16rem"`). Prefer this.
   */
  minColWidth?: string;
  /** Render-as the single child element rather than a `<div>`. */
  asChild?: boolean;
  /**
   * Root `data-slot` value. Defaults to `"grid"`. A composing block can
   * override it so consumers target the outer element via the block's own
   * slot vocabulary. Mirrors Stack/Card.
   */
  "data-slot"?: string;
}

const GRID_AUTO_FLOW: Record<NonNullable<GridProps["flow"]>, string> = {
  row: "row",
  column: "column",
  dense: "dense",
};

/**
 * Two-dimensional grid with a governed gap. Prefer `minColWidth` for a
 * fluid, breakpoint-free grid; use `columns` for an explicit count.
 * Arrange-only: no paint.
 */
export const Grid = forwardRef<HTMLDivElement, GridProps>(function Grid(
  {
    columns = 1,
    gap,
    align,
    flow,
    minColWidth,
    asChild = false,
    className,
    style,
    children,
    "data-slot": dataSlot = "grid",
    ...rest
  },
  ref,
) {
  // Dev-mode parity with Card: warn when asChild has no single valid
  // element child (Slot would render nothing silently). DCEs in prod.
  if (process.env.NODE_ENV !== "production") {
    if (asChild && !isValidElement(children)) {
      // eslint-disable-next-line no-console
      console.warn(
        "Grid asChild requires a single React element child; rendering nothing.",
      );
    }
  }

  const intrinsic = minColWidth != null;
  const responsive = !intrinsic && typeof columns === "object";

  // Base column count for the explicit-count path. For a responsive
  // object the mobile-first base is ALWAYS 1 — the provided breakpoints
  // promote the count upward from there (the Tailwind mental model), so
  // `{ lg: 4 }` is 1 column until lg, never 4 at all widths. For a number
  // the base is the number.
  let baseColumns = 1;
  if (!intrinsic && typeof columns === "number") {
    baseColumns = columns;
  }

  const composedClassName = classnames(
    "zs-grid",
    intrinsic ? "zs-grid--intrinsic" : "zs-grid--explicit",
    className,
  );

  // Layout values flow as inline custom properties / data-attributes
  // consumed by Grid.css. The responsive object's per-breakpoint counts
  // ride on --grid-cols-sm/md/lg; Grid.css media queries (rem literals
  // commented with the --zs-bp-* token names) switch --grid-cols to them.
  const layoutVars: React.CSSProperties = {
    ...(gap != null ? { "--grid-gap": spaceVar(gap) } : null),
    ...(align ? { "--grid-align": alignValue(align) } : null),
    ...(flow ? { "--grid-flow": GRID_AUTO_FLOW[flow] } : null),
    ...(intrinsic ? { "--grid-min-col": minColWidth } : null),
    ...(!intrinsic ? { "--grid-cols": String(baseColumns) } : null),
    ...(responsive && (columns as GridColumns).sm != null
      ? { "--grid-cols-sm": String((columns as GridColumns).sm) }
      : null),
    ...(responsive && (columns as GridColumns).md != null
      ? { "--grid-cols-md": String((columns as GridColumns).md) }
      : null),
    ...(responsive && (columns as GridColumns).lg != null
      ? { "--grid-cols-lg": String((columns as GridColumns).lg) }
      : null),
    ...style,
  } as React.CSSProperties;

  const Comp = asChild ? Slot : "div";

  return (
    <Comp
      {...rest}
      ref={ref as Ref<HTMLDivElement>}
      data-slot={dataSlot}
      data-responsive={responsive ? "" : undefined}
      className={composedClassName}
      style={layoutVars}
    >
      {children}
    </Comp>
  );
});
Grid.displayName = "Grid";
