/*
 * Center — centers its content on both axes.
 *
 * A flex box that places its children dead center horizontally AND
 * vertically. The 80% case is an empty/loading/error state ("nothing
 * here yet", a spinner, a 404 panel) that should sit in the middle of
 * its region.
 *
 *   - `inline` drops the vertical centering — content is centered
 *     horizontally only and sits at the top. Use when the Center has no
 *     defined height and you only want horizontal centering.
 *   - `minHeight` gives the box a floor so vertical centering has room
 *     to work even when the content is short (e.g. a full-viewport
 *     empty state: `minHeight="60vh"`).
 *
 * A Center paints NOTHING — no background, no border. It only arranges.
 *
 * Design guarantees:
 *   - `minHeight` is a free CSS length, applied as the logical
 *     `min-block-size` so it survives vertical writing modes.
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

export interface CenterProps extends ComponentPropsWithoutRef<"div"> {
  /**
   * Center horizontally only (drop the vertical centering). Content
   * sits at the top of the box, horizontally centered. Default `false`
   * (center on both axes).
   */
  inline?: boolean;
  /**
   * Minimum block-size (height) so vertical centering has room when the
   * content is short. A free CSS length, e.g. `"60vh"`.
   */
  minHeight?: string;
  /** Render-as the single child element rather than a `<div>`. */
  asChild?: boolean;
}

/**
 * Centers content on both axes (or horizontally only with `inline`).
 * Arrange-only: no paint.
 */
export const Center = forwardRef<HTMLDivElement, CenterProps>(function Center(
  {
    inline = false,
    minHeight,
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
        "Center asChild requires a single React element child; rendering nothing.",
      );
    }
  }

  const composedClassName = classnames(
    "zs-center",
    inline ? "zs-center--inline" : null,
    className,
  );

  const layoutVars: React.CSSProperties = {
    ...(minHeight != null ? { "--center-min-block": minHeight } : null),
    ...style,
  } as React.CSSProperties;

  const Comp = asChild ? Slot : "div";

  return (
    <Comp
      {...rest}
      ref={ref as Ref<HTMLDivElement>}
      data-slot="center"
      data-inline={inline ? "" : undefined}
      className={composedClassName}
      style={layoutVars}
    >
      {children}
    </Comp>
  );
});
Center.displayName = "Center";
