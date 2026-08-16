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
 *   - `size` maps to the `--zeroship-container-*` foundation tokens; `full`
 *     opts out of any max-width (`none`).
 *   - `padX` is the closed `Pad` union → `--zeroship-space-*` via `spaceVar`;
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
import { type Pad, spaceVar } from "../_layout-primitives";

export type ContainerSize =
  "sm" | "md" | "lg" | "xl" | "product" | "wide" | "full";

export interface ContainerProps extends ComponentPropsWithoutRef<"div"> {
  /**
   * Max content width, from the `--zeroship-container-*` tokens. `full` opts
   * out of any max-width. Default `lg`.
   */
  size?: ContainerSize;
  /** Symmetric inline padding, from the `--zeroship-space-*` scale. Default `4`. */
  padX?: Pad;
  /** Center the column with `margin-inline: auto`. Default `true`. */
  center?: boolean;
  /** Render-as the single child element rather than a `<div>`. */
  asChild?: boolean;
  /**
   * Root `data-slot` value. Defaults to `"container"`. A composing block
   * can add its own token so both slot vocabularies remain available.
   * Mirrors Stack/Card.
   */
  "data-slot"?: string;
}

// `full` resolves to `none`; every other size resolves to its width
// token. Kept as a map so the resolution is total over ContainerSize.
const CONTAINER_MAX_WIDTH: Record<ContainerSize, string> = {
  sm: "var(--zeroship-container-sm)",
  md: "var(--zeroship-container-md)",
  lg: "var(--zeroship-container-lg)",
  xl: "var(--zeroship-container-xl)",
  product: "var(--zeroship-container-product)",
  wide: "var(--zeroship-container-wide)",
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
      "data-slot": dataSlot,
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
        data-slot={["container", dataSlot].filter(Boolean).join(" ")}
        data-size={size}
        className={className}
        style={layoutVars}
      >
        {children}
      </Comp>
    );
  },
);
Container.displayName = "Container";
