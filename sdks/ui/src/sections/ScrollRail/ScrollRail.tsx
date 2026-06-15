/*
 * ScrollRail — horizontal product/content rail with governed gutters.
 *
 * The Apple clone repeatedly needed the same shape: a full-width overflow rail
 * whose first/last items align to the page container, with optional scroll
 * snapping and a controlled gap. This primitive makes that pattern reusable
 * without turning it into a carousel framework.
 */
import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type CSSProperties,
} from "react";
import { classnames } from "../../components/_classnames";
import { type ContainerSize } from "../../layouts/Container";
import { type Gap, type Pad, spaceVar } from "../../layouts/_layout-primitives";

export type ScrollRailSnap = "none" | "start";

export interface ScrollRailProps extends ComponentPropsWithoutRef<"div"> {
  /** The governed page width the rail aligns to. Default `wide`. */
  size?: ContainerSize;
  /** Rail item gap from the spacing scale. Default `5`. */
  gap?: Gap;
  /** Minimum viewport-edge padding from the spacing scale. Default `6`. */
  padX?: Pad;
  /** Enable scroll snapping for direct-manipulation rails. Default `start`. */
  snap?: ScrollRailSnap;
  /** Root `data-slot` value. Defaults to `"scroll-rail"`. */
  "data-slot"?: string;
}

const RAIL_MAX_WIDTH: Record<ContainerSize, string> = {
  sm: "var(--zs-container-sm)",
  md: "var(--zs-container-md)",
  lg: "var(--zs-container-lg)",
  xl: "var(--zs-container-xl)",
  product: "var(--zs-container-product)",
  wide: "var(--zs-container-wide)",
  full: "100vw",
};

export const ScrollRail = forwardRef<HTMLDivElement, ScrollRailProps>(
  function ScrollRail(
    {
      size = "wide",
      gap = 5,
      padX = 6,
      snap = "start",
      className,
      style,
      role,
      tabIndex,
      "aria-label": ariaLabel,
      "aria-labelledby": ariaLabelledBy,
      "data-slot": dataSlot = "scroll-rail",
      ...rest
    },
    ref,
  ) {
    const railVars: CSSProperties = {
      "--zs-scroll-rail-max-width": RAIL_MAX_WIDTH[size],
      "--zs-scroll-rail-gap": spaceVar(gap),
      "--zs-scroll-rail-pad": spaceVar(padX),
      ...style,
    } as CSSProperties;

    return (
      <div
        {...rest}
        ref={ref}
        role={role ?? (ariaLabel != null || ariaLabelledBy != null ? "region" : undefined)}
        tabIndex={tabIndex ?? 0}
        aria-label={ariaLabel}
        aria-labelledby={ariaLabelledBy}
        data-slot={dataSlot}
        data-snap={snap}
        data-size={size}
        className={classnames("zs-scroll-rail", className)}
        style={railVars}
      />
    );
  },
);
ScrollRail.displayName = "ScrollRail";
