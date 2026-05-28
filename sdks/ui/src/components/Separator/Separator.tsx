/*
 * Separator — visual divider.
 *
 * Two modes, gated by the `decorative` prop:
 *
 *   - `decorative={true}` (default): renders `role="none"` +
 *     `aria-hidden="true"`. The divider is purely visual; assistive
 *     tech skips it. This is the right shape for the 80% case —
 *     between rows in a list, between sections in a card, etc.
 *
 *   - `decorative={false}`: renders Base UI's `<Separator>` which
 *     emits `role="separator"` + `aria-orientation="horizontal" |
 *     "vertical"`. Use when the divider carries semantic meaning
 *     (e.g. between unrelated groups in a complex page) so AT users
 *     hear the structural cut.
 *
 * Design guarantees encoded here (in source so they travel with the
 * code, not in a sibling doc):
 *
 *   1. The Separator owns NO outer margin. Spacing rhythm belongs to
 *      the parent (list rows, card section, toolbar). A Separator
 *      stamped between rows takes its position from the row gap; if
 *      a consumer wants a gap above/below, they wrap with a Box.
 *
 *   2. The line itself is rendered via `border-*` so it stays exactly
 *      one device pixel (the `--zs-selection-hairline` token resolves
 *      to 0.0625rem which devicePixelRatio rounds correctly). The
 *      `hairline` variant matches the inset hairlines on Card / Dialog
 *      so a Separator across a Card edge reads coherent.
 *
 *   3. Logical properties drive orientation. `border-block-end-width`
 *      for horizontal, `border-inline-end-width` for vertical. RTL is
 *      automatic — flipping `dir="rtl"` does NOT change the visual
 *      because the separator has no inline content; the verification
 *      story exists as a regression net.
 *
 *   4. forced-colors specificity mirror — `border-color: CanvasText` at
 *      the same selector depth so the system palette wins. The
 *      `thick` variant retains its block-size under forced-colors so
 *      it still reads as the heavier divider it is.
 *
 *   5. Reduced motion is a no-op here — Separator has no transitions.
 *      The gate exists for parity with the rest of the surface.
 *
 *   6. NOT a `<hr>`. The native `<hr>` has its own UA stylesheet, its
 *      own line-box behaviour, and an unconditional block-level
 *      role. Separator is a `<div>` that paints a hairline so the
 *      same component works both between list rows AND between
 *      toolbar buttons (vertical orientation) without consumers
 *      branching at the call site.
 */
import {
  forwardRef,
  type ComponentPropsWithRef,
  type Ref,
} from "react";
import { Separator as BaseSeparator } from "@base-ui/react/separator";
import { classnames } from "../_classnames";

export type SeparatorOrientation = "horizontal" | "vertical";
export type SeparatorVariant = "hairline" | "thick";

type BaseSeparatorProps = ComponentPropsWithRef<typeof BaseSeparator>;

export interface SeparatorProps
  extends Omit<BaseSeparatorProps, "className" | "render" | "orientation"> {
  /**
   * Layout axis — `horizontal` (default) cuts a row; `vertical` cuts a
   * column. Drives both the visible line direction (border on the end
   * inline-edge vs block-edge) AND the `aria-orientation` attribute
   * when `decorative={false}`.
   */
  orientation?: SeparatorOrientation;
  /**
   * Line weight. `hairline` (default) paints a one-device-pixel
   * divider using `--zs-selection-hairline`. `thick` paints a
   * `--zs-space-1` (0.25rem) bar — reserved for major section cuts.
   */
  variant?: SeparatorVariant;
  /**
   * Whether the separator is decorative (default `true`). When `true`,
   * renders a plain `<div>` with `role="none"` + `aria-hidden="true"`
   * — AT skips the node entirely. When `false`, defers to Base UI's
   * `<Separator>` which emits `role="separator"` + `aria-orientation`.
   *
   * Pick `false` when the cut carries meaning AT users should hear
   * (between unrelated groups in a complex page). Pick `true` (the
   * default) for purely visual rhythm dividers.
   */
  decorative?: boolean;
  /** Optional class hook on the line. */
  className?: string;
}

/**
 * The visible divider element. The two `decorative` branches diverge
 * at the JSX root so we never get into a state where Base UI's
 * `role="separator"` and our `aria-hidden` co-exist (which would
 * confuse AT — separator + aria-hidden is contradictory).
 */
export const Separator = forwardRef<HTMLDivElement, SeparatorProps>(
  function Separator(
    {
      orientation = "horizontal",
      variant = "hairline",
      decorative = true,
      className,
      ...rest
    },
    ref,
  ) {
    const composedClassName = classnames(
      "zs-separator",
      `zs-separator--${orientation}`,
      `zs-separator--${variant}`,
      className,
    );

    if (decorative) {
      // Plain `<div>` path — no role from Base UI to fight with. We
      // forward `rest` so consumers can still attach data-* / id /
      // style. `role="none"` + `aria-hidden="true"` is the
      // documented "skip me" combo for AT.
      return (
        <div
          {...(rest as React.HTMLAttributes<HTMLDivElement>)}
          ref={ref}
          role="none"
          aria-hidden="true"
          className={composedClassName}
          data-orientation={orientation}
          data-variant={variant}
        />
      );
    }

    // Semantic path — Base UI's Separator emits role="separator" and
    // aria-orientation. We forward all consumer props through `rest`;
    // Base UI's spread order (consumer wins over its defaults) keeps
    // data-* and id reaching the DOM. The orientation prop is named
    // identically so Base UI's value flows straight through.
    return (
      <BaseSeparator
        {...(rest as BaseSeparatorProps)}
        ref={ref as Ref<HTMLDivElement>}
        orientation={orientation}
        className={composedClassName}
        data-orientation={orientation}
        data-variant={variant}
      />
    );
  },
);
Separator.displayName = "Separator";
