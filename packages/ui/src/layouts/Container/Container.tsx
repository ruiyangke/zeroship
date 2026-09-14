/*
 * Container — the page-width authority.
 *
 * Constrains its content to a governed max-width, centers it
 * horizontally, and applies symmetric inline padding so the content
 * never touches the viewport edge on narrow screens. This is the ONLY
 * layout primitive that sets `max-width` — width discipline lives in one
 * place so a page reads as a coherent column instead of a patchwork of
 * ad-hoc max-widths.
 *
 * A Container paints NOTHING — no background, no border. It only
 * constrains + centers + pads.
 *
 * Design guarantees:
 *   - `size` maps to the `--zs-container-*` foundation tokens; `full`
 *     opts out of any max-width (`none`).
 *   - `padX` is the closed `Pad` union → `--zs-space-*` via `spaceVar`;
 *     applied as `padding-inline` (logical) so RTL is automatic.
 *   - `center` toggles `margin-inline: auto`. Default `true` — the
 *     overwhelmingly common case.
 *   - `asChild` routes through `Slot` (React-19-safe refs) so a
 *     Container can render-as `<main>` / `<section>` / `<article>`.
 */
import {
  forwardRef,
  isValidElement,
  type ComponentPropsWithoutRef,
  type Ref,
} from "react";
import { Slot } from "../../components/_slot";
import { classnames } from "../../components/_classnames";
import { type Pad, spaceVar } from "../_layout-primitives";

export type ContainerSize =
  | "sm"
  | "md"
  | "lg"
  | "xl"
  | "product"
  | "wide"
  | "full";

export interface ContainerProps extends ComponentPropsWithoutRef<"div"> {
  /**
   * Max content width, from the `--zs-container-*` tokens. `full` opts
   * out of any max-width. Default `lg`.
   */
  size?: ContainerSize;
  /** Symmetric inline padding, from the `--zs-space-*` scale. Default `4`. */
  padX?: Pad;
  /** Center the column with `margin-inline: auto`. Default `true`. */
  center?: boolean;
  /** Render-as the single child element rather than a `<div>`. */
  asChild?: boolean;
  /**
   * Root `data-slot` value. Defaults to `"container"`. A composing block
   * can override it so consumers target the outer element via the block's
   * own slot vocabulary. Mirrors Stack/Card.
   */
  "data-slot"?: string;
}

// `full` resolves to `none`; every other size resolves to its width
// token. Kept as a map so the resolution is total over ContainerSize.
const CONTAINER_MAX_WIDTH: Record<ContainerSize, string> = {
  sm: "var(--zs-container-sm)",
  md: "var(--zs-container-md)",
  lg: "var(--zs-container-lg)",
  xl: "var(--zs-container-xl)",
  product: "var(--zs-container-product)",
  wide: "var(--zs-container-wide)",
  full: "none",
};

/**
 * Page-width authority — constrains, centers, and pads the content
 * column. The only primitive that sets `max-width`. Arrange-only: no
 * paint.
 */
export const Container = forwardRef<HTMLDivElement, ContainerProps>(
  function Container(
    {
      size = "lg",
      padX = 4,
      center = true,
      asChild = false,
      className,
      style,
      children,
      "data-slot": dataSlot = "container",
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
          "Container asChild requires a single React element child; rendering nothing.",
        );
      }
    }

    const composedClassName = classnames("zs-container", className);

    const layoutVars: React.CSSProperties = {
      "--container-max-width": CONTAINER_MAX_WIDTH[size],
      "--container-pad-x": spaceVar(padX),
      "--container-margin-inline": center ? "auto" : "0",
      ...style,
    } as React.CSSProperties;

    const Comp = asChild ? Slot : "div";

    return (
      <Comp
        {...rest}
        ref={ref as Ref<HTMLDivElement>}
        data-slot={dataSlot}
        data-size={size}
        className={composedClassName}
        style={layoutVars}
      >
        {children}
      </Comp>
    );
  },
);
Container.displayName = "Container";
