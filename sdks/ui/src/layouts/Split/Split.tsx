/*
 * Split — fixed sidebar + fluid main, compound primitive.
 *
 *   <Split side="start" sideWidth="16rem">
 *     <Split.Side>…sidebar…</Split.Side>
 *     <Split.Main>…content…</Split.Main>
 *   </Split>
 *
 * The Side is a fixed-width rail (`flex: 0 0 var(--split-side-width)`);
 * the Main fills the rest (`flex: 1; min-width: 0` so long content can
 * shrink/scroll instead of blowing out the row). DOM order is always
 * Side-then-Main; `side="end"` reverses the VISUAL order via
 * `flex-direction: row-reverse` so the rail sits on the trailing edge
 * without reordering the markup (keeps reading order sane for AT).
 *
 * `collapseBelow` stacks the two regions into a column below the named
 * breakpoint — the standard "sidebar drops under the content on
 * mobile" behaviour.
 *
 * A Split paints NOTHING and carries no roles — it only arranges. The
 * Side is not a `<nav>`, the Main is not `<main>`; the consumer wraps
 * with semantic elements (or uses `asChild`) when those roles are
 * wanted.
 *
 * Design guarantees:
 *   - `sideWidth` is a free CSS length (the one place an explicit width
 *     is expected — it sizes a structural rail, not inter-item spacing).
 *   - `gap` is the closed `Gap` union → `--zs-space-*` via `spaceVar`.
 *   - `collapseBelow` rides on a data-attribute consumed by Split.css
 *     media queries; the breakpoint rem literals there carry a comment
 *     naming the --zs-bp-* token (custom properties can't appear in
 *     @media conditions).
 *   - RTL is automatic — `flex-direction: row` / `row-reverse` follow the
 *     writing direction, so "start"/"end" map to the correct inline edge.
 *   - `asChild` routes through `Slot` (React-19-safe refs) on root + parts.
 */
import {
  forwardRef,
  isValidElement,
  type ComponentPropsWithoutRef,
  type Ref,
} from "react";
import { Slot } from "../../components/_slot";
import { classnames } from "../../components/_classnames";
import { type Gap, spaceVar } from "../_layout-primitives";

export type SplitSide = "start" | "end";
export type SplitCollapse = "sm" | "md" | "lg";

export interface SplitProps extends ComponentPropsWithoutRef<"div"> {
  /** Which inline edge the fixed rail sits on. Default `start`. */
  side?: SplitSide;
  /** Width of the fixed rail (`Split.Side`). Default `"16rem"`. */
  sideWidth?: string;
  /** Gap between rail and main, from the `--zs-space-*` scale. */
  gap?: Gap;
  /** Stack into a column below this breakpoint (rail above main). */
  collapseBelow?: SplitCollapse;
  /** Render-as the single child element rather than a `<div>`. */
  asChild?: boolean;
  /**
   * Root `data-slot` value. Defaults to `"split"`. A composing block
   * (e.g. `AppShell.Body`) overrides it so consumers can target the
   * outer element via the block's own slot vocabulary. Mirrors Card.
   */
  "data-slot"?: string;
}

export type SplitSideProps = ComponentPropsWithoutRef<"div"> & {
  /** Render-as the single child element rather than a `<div>`. */
  asChild?: boolean;
  /**
   * `data-slot` value. Defaults to `"split-side"`. A composing block
   * (e.g. `AppShell.Sidebar`) overrides it. Mirrors Card.
   */
  "data-slot"?: string;
};
export type SplitMainProps = ComponentPropsWithoutRef<"div"> & {
  /** Render-as the single child element rather than a `<div>`. */
  asChild?: boolean;
  /**
   * `data-slot` value. Defaults to `"split-main"`. A composing block
   * (e.g. `AppShell.Main`) overrides it. Mirrors Card.
   */
  "data-slot"?: string;
};

/* ─── Split root ──────────────────────────────────────────────────────── */

const SplitRoot = forwardRef<HTMLDivElement, SplitProps>(function SplitRoot(
  {
    side = "start",
    sideWidth = "16rem",
    gap,
    collapseBelow,
    asChild = false,
    className,
    style,
    children,
    "data-slot": dataSlot = "split",
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
        "Split asChild requires a single React element child; rendering nothing.",
      );
    }
  }

  const composedClassName = classnames("zs-split", className);

  const layoutVars: React.CSSProperties = {
    "--split-side-width": sideWidth,
    ...(gap != null ? { "--split-gap": spaceVar(gap) } : null),
    ...style,
  } as React.CSSProperties;

  const Comp = asChild ? Slot : "div";

  return (
    <Comp
      {...rest}
      ref={ref as Ref<HTMLDivElement>}
      data-slot={dataSlot}
      data-side={side}
      data-collapse={collapseBelow}
      className={composedClassName}
      style={layoutVars}
    >
      {children}
    </Comp>
  );
});
SplitRoot.displayName = "Split";

/* ─── Parts ───────────────────────────────────────────────────────────── */

const SplitSideEl = forwardRef<HTMLDivElement, SplitSideProps>(
  function SplitSide(
    {
      asChild = false,
      className,
      children,
      "data-slot": dataSlot = "split-side",
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
          "Split.Side asChild requires a single React element child; rendering nothing.",
        );
      }
    }
    const Comp = asChild ? Slot : "div";
    return (
      <Comp
        {...rest}
        ref={ref as Ref<HTMLDivElement>}
        data-slot={dataSlot}
        className={classnames("zs-split__side", className)}
      >
        {children}
      </Comp>
    );
  },
);
SplitSideEl.displayName = "Split.Side";

const SplitMainEl = forwardRef<HTMLDivElement, SplitMainProps>(
  function SplitMain(
    {
      asChild = false,
      className,
      children,
      "data-slot": dataSlot = "split-main",
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
          "Split.Main asChild requires a single React element child; rendering nothing.",
        );
      }
    }
    const Comp = asChild ? Slot : "div";
    return (
      <Comp
        {...rest}
        ref={ref as Ref<HTMLDivElement>}
        data-slot={dataSlot}
        className={classnames("zs-split__main", className)}
      >
        {children}
      </Comp>
    );
  },
);
SplitMainEl.displayName = "Split.Main";

/* ─── public Split namespace ──────────────────────────────────────────── */

type SplitComponent = typeof SplitRoot & {
  Side: typeof SplitSideEl;
  Main: typeof SplitMainEl;
};

export const Split = SplitRoot as SplitComponent;
Split.Side = SplitSideEl;
Split.Main = SplitMainEl;
