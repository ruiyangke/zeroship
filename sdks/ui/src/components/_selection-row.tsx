/*
 * SelectionRow — shared layout helper for the selection primitives.
 *
 * Checkbox / Switch / Radio all wrap their visible chip plus an inline
 * label text in a `<label>` so a click on the text toggles the chip via
 * native semantics. The three blocks differ only in their data-slot
 * prefix, so this helper hoists the common JSX and the components carry
 * intent (which chip, what state) without re-stating the layout.
 *
 * Discipline (slice-4 review-fix items 2, 3, 10):
 *   - The row is PURELY a layout helper. It does NOT carry the focus
 *     ring (that lives on the chip's `:focus-visible`) and does NOT
 *     carry the hit-target floor (the chip's invisible `::before`
 *     overlay extends the tap rect). Earlier the row owned both, but
 *     that broke the canonical `<Field><Field.Label>…</Field.Label>
 *     <Chip /></Field>` pattern — the bare chip showed no ring and
 *     no enlarged hit target.
 *   - `gap`, `cursor`, `disabled-color` are the row's job. Nothing
 *     interactive lives here.
 *
 * Filename starts with an underscore so the directory listing makes it
 * obvious this isn't a public component — it's internal plumbing. The
 * components/ surface re-exports nothing from here.
 */
import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type ReactNode,
} from "react";
import { classnames } from "./_classnames";

export type SelectionBase = "checkbox" | "switch" | "radio";
export type SelectionSize = "sm" | "md" | "lg";

export interface SelectionRowProps {
  /** Which primitive this row wraps; drives the `${base}-field` data-slot. */
  base: SelectionBase;
  /** Visual size; stamps the row's `data-size` attribute. */
  size: SelectionSize;
  /** Mirror of the chip's `disabled` so the row's cursor + color flips. */
  disabled?: boolean;
  /** Extra CSS class names for the row. */
  className?: string;
  /** Extra props for the wrapping `<label>`. */
  fieldProps?: ComponentPropsWithoutRef<"label">;
  /** chip + inline label text. */
  children: ReactNode;
}

/**
 * Render a `<label>` wrapping the chip + its inline label text. The
 * native `<label>` semantics route a click on the text to the chip's
 * hidden input.
 */
export const SelectionRow = forwardRef<HTMLLabelElement, SelectionRowProps>(
  function SelectionRow(
    { base, size, disabled, className, fieldProps, children },
    ref,
  ) {
    return (
      <label
        ref={ref}
        {...fieldProps}
        className={classnames(className, fieldProps?.className)}
        data-slot={`${base}-field`}
        data-size={size}
        data-disabled={disabled || undefined}
      >
        {children}
      </label>
    );
  },
);
SelectionRow.displayName = "SelectionRow";
