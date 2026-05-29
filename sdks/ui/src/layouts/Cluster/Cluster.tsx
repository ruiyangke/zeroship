/*
 * Cluster — wrapping inline group.
 *
 * A horizontal flex row that wraps onto multiple lines, with a governed
 * `gap` on both axes. The canonical use is a bag of chips / tags /
 * metadata pills / filter badges that should flow and wrap as the
 * container narrows. Defaults are tuned for that case: a small gap,
 * vertically centered, packed to the start.
 *
 * Non-interactive visual wrap. For an interactive group with roving
 * tabindex (arrow-key navigation across a set of controls), use
 * `Toolbar`. Cluster is layout-only: it emits no role, no tabindex, and
 * manages no focus — it just arranges children that wrap.
 *
 * A Cluster paints NOTHING — no background, no border. It only arranges.
 *
 * Design guarantees:
 *   - `gap` is the closed `Gap` union → `--zs-space-*` via `spaceVar`;
 *     no arbitrary spacing. Applies on both row and cross axes (one
 *     `gap` shorthand) so wrapped lines breathe the same as inline gaps.
 *   - `align`/`justify` flow through the shared `alignValue`/
 *     `justifyValue` maps.
 *   - `asChild` routes through `Slot` (React-19-safe refs).
 *   - RTL is automatic — `flex-direction: row` follows writing direction.
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

export interface ClusterProps extends ComponentPropsWithoutRef<"div"> {
  /** Gap between items (both axes), from the `--zs-space-*` scale. Default `2`. */
  gap?: Gap;
  /** Cross-axis alignment (`align-items`). Default `center`. */
  align?: Align;
  /** Main-axis distribution (`justify-content`). Default `start`. */
  justify?: Justify;
  /** Render-as the single child element rather than a `<div>`. */
  asChild?: boolean;
}

/**
 * Non-interactive wrapping inline group (chips, tags, metadata). For an
 * interactive group with roving tabindex, use `Toolbar`. Arrange-only:
 * no paint.
 */
export const Cluster = forwardRef<HTMLDivElement, ClusterProps>(function Cluster(
  {
    gap = 2,
    align = "center",
    justify = "start",
    asChild = false,
    className,
    style,
    children,
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
        "Cluster asChild requires a single React element child; rendering nothing.",
      );
    }
  }

  const composedClassName = classnames("zs-cluster", className);

  const layoutVars: React.CSSProperties = {
    "--cluster-gap": spaceVar(gap),
    "--cluster-align": alignValue(align),
    "--cluster-justify": justifyValue(justify),
    ...style,
  } as React.CSSProperties;

  const Comp = asChild ? Slot : "div";

  return (
    <Comp
      {...rest}
      ref={ref as Ref<HTMLDivElement>}
      data-slot="cluster"
      className={composedClassName}
      style={layoutVars}
    >
      {children}
    </Comp>
  );
});
Cluster.displayName = "Cluster";
