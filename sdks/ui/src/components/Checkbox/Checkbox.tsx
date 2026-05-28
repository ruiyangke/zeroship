/*
 * Checkbox — selection primitive (independent boolean).
 *
 * Wraps Base UI's headless `Checkbox.Root` + `Checkbox.Indicator`.
 * Renders as a focusable chip plus a hidden <input> Base UI emits
 * beside it for form submission and screen-reader semantics.
 *
 * Design guarantees (written here so future maintainers can see the
 * intent without consulting the brief):
 *
 *   1. Checkbox is for INDEPENDENT booleans — one chip = one yes/no.
 *      Use it for "I agree", "Subscribe to emails", "Make public".
 *      For mutually-exclusive choice use Radio; for binary settings
 *      that take effect immediately, use Switch.
 *
 *   2. Indeterminate = parent of partially-checked children. Never
 *      use indeterminate as a "maybe" or "unknown" state — that's
 *      a separate tri-state checkbox a future slice can introduce.
 *      The indeterminate glyph follows Base UI's COMPUTED state
 *      (`data-indeterminate`), not the wrapper prop alone — both
 *      glyphs render simultaneously and CSS swaps visibility from
 *      the chip's data attributes. That makes the parent-of-group
 *      pattern Just Work without the wrapper having to thread an
 *      explicit `indeterminate` prop (slice-4 review fix item 6).
 *
 *   3. The checked glyph (checkmark) and indeterminate glyph (minus)
 *      are visually distinct paths. Two state signals — fill + shape —
 *      so colorblind users get the same information.
 *
 *   4. The whole row (chip + label text) is the click surface. The
 *      visible chip is small for visual rhythm; the chip's invisible
 *      `::before` overlay extends the hit rect to ≥ 1.75rem on fine
 *      pointers and ≥ 2.75rem on coarse — so a bare chip without a
 *      wrapping label is still tappable (slice-4 review fix item 3).
 *
 *   5. The accent fill is the SYSTEM signal — overriding it loses
 *      the cross-control affordance Switch and Radio share. Consumers
 *      override via the `--zs-checkbox-fill-checked` CSS variable
 *      when they truly need to (rare).
 *
 * Field integration:
 *   - `size` inherits from `useFieldVisualSize()` when not set.
 *   - `disabled` inherits from `useFieldDisabledContext()`; the prop
 *     always wins so a consumer can re-enable one chip inside a
 *     disabled Field if needed.
 *   - `required` inherits from `useFieldContext()` so a
 *     `<Field required>` cascades to the contained Checkbox without
 *     having to set the prop twice (slice-4 review fix item 7,
 *     mirroring Input.tsx).
 *
 * The chip itself takes `className` for custom styling — Base UI's
 * `[data-checked]` / `[data-indeterminate]` data attributes do the
 * work without needing an asChild escape hatch. (Selection primitives
 * are *visual chips with a hidden input*, not button-shaped surfaces,
 * so the swap-the-whole-element idiom doesn't apply — see slice-4
 * review fix item 1.)
 */
import {
  forwardRef,
  type ComponentPropsWithRef,
  type ReactNode,
} from "react";
import { Checkbox as BaseCheckbox } from "@base-ui/react/checkbox";
import {
  useFieldContext,
  useFieldDisabledContext,
  useFieldVisualSize,
} from "../Field";
import { useFieldsetDisabledContext } from "../Fieldset";
import { classnames } from "../_classnames";
import { SelectionRow } from "../_selection-row";

export type CheckboxSize = "sm" | "md" | "lg";
export type CheckboxVariant = "default" | "tinted";

type BaseCheckboxRootProps = ComponentPropsWithRef<typeof BaseCheckbox.Root>;

export interface CheckboxProps
  extends Omit<BaseCheckboxRootProps, "className" | "render" | "children"> {
  /** Visual size — small 1rem, medium 1.125rem (default), large 1.25rem. */
  size?: CheckboxSize;

  /**
   * Visual variant.
   * - `default`: standard chip (border + accent fill on check).
   * - `tinted`: softens the unchecked surface for loud parents.
   */
  variant?: CheckboxVariant;

  /** Class name for the visible chip. */
  className?: string;

  /**
   * Optional text label rendered to the inline-end of the chip. When
   * present, the whole row becomes a single <label> so a click on the
   * text toggles the chip. Use Field.Label instead when the checkbox
   * lives inside a Field — the Field wires htmlFor automatically.
   */
  label?: ReactNode;

  /** Class name for the wrapping <label> row (the click surface). */
  fieldClassName?: string;

  /** Extra props for the wrapping <label> (when `label` is present). */
  fieldProps?: ComponentPropsWithRef<"label">;
}

/* ─── indicator glyphs — two distinct paths so shape ≠ color ────────
 *
 * Both SVGs use `currentColor` so the parent chip's `color` token
 * paints them. The checkmark is the standard tick; the minus is a
 * single horizontal bar so the indeterminate state is unmistakable
 * even in monochrome.
 *
 * Both glyphs render simultaneously inside the Indicator and CSS
 * swaps visibility off the chip's `data-checked` / `data-indeterminate`
 * attributes — that way the indeterminate state derives from Base UI's
 * COMPUTED state (the `CheckboxGroup` parent-of-children pattern
 * doesn't require the wrapper to set `indeterminate` explicitly).
 */
function IndicatorCheck() {
  return (
    <svg
      data-glyph="check"
      viewBox="0 0 16 16"
      role="presentation"
      focusable="false"
      aria-hidden="true"
    >
      <path
        className="zs-checkbox__indicator-check"
        d="M3.5 8.25l2.75 2.75L12.5 5"
      />
    </svg>
  );
}
function IndicatorMinus() {
  return (
    <svg
      data-glyph="minus"
      viewBox="0 0 16 16"
      role="presentation"
      focusable="false"
      aria-hidden="true"
    >
      <rect
        className="zs-checkbox__indicator-minus"
        x="3.5"
        y="7.25"
        width="9"
        height="1.5"
        rx="0.75"
      />
    </svg>
  );
}

// Base UI renders CheckboxRoot as a `<span>` with `tabIndex=0` (verified
// against @base-ui/react@1.5.0 — see node_modules/.pnpm/@base-ui+react@
// 1.5.0/.../checkbox/root/CheckboxRoot.d.ts:13: `RefAttributes<HTMLElement>`,
// and the file-header comment "Renders a <span> element"). Earlier the
// ref was typed `HTMLButtonElement` which let `inputRef.current.disabled`
// type-check but return undefined — slice-4 review fix item 5.
export const Checkbox = forwardRef<HTMLSpanElement, CheckboxProps>(
  function Checkbox(
    {
      size: sizeProp,
      variant = "default",
      className,
      label,
      fieldClassName,
      fieldProps,
      disabled: disabledProp,
      required: requiredProp,
      indeterminate,
      ...rest
    },
    ref,
  ) {
    // Cascade: explicit prop wins; otherwise read the Field context;
    // otherwise fall back to a wrapping Fieldset; otherwise the
    // canonical default. Hooks are called unconditionally so React's
    // call-order invariant holds even when the explicit prop is set
    // on one render and absent on the next. The Fieldset signal is a
    // separate context (`FieldsetDisabledContext`) because the visible
    // chip is a non-native `<span>` Base UI part and doesn't pick up
    // the native `<fieldset disabled>` cascade.
    const fieldSize = useFieldVisualSize();
    const fieldDisabled = useFieldDisabledContext();
    const fieldsetDisabled = useFieldsetDisabledContext();
    const fieldCtx = useFieldContext();
    const size: CheckboxSize = sizeProp ?? fieldSize ?? "md";
    // Both context hooks return `boolean` (default `false`), so we
    // OR them rather than ??-chain — `??` would short-circuit on a
    // legitimate `false` from the inner Field and never consult the
    // outer Fieldset.
    const disabled = disabledProp ?? (fieldDisabled || fieldsetDisabled);
    const required = requiredProp ?? fieldCtx?.required ?? false;

    const chipClassName = classnames(
      "zs-checkbox",
      `zs-checkbox--${size}`,
      `zs-checkbox--${variant}`,
      className,
    );

    // The visible chip is what Base UI renders as a <span>. We only
    // ever touch the className + data attributes from here; the hidden
    // <input> Base UI renders is the real form-submission element.
    const chip = (
      <BaseCheckbox.Root
        {...rest}
        ref={ref as React.Ref<HTMLElement>}
        disabled={disabled || undefined}
        required={required || undefined}
        indeterminate={indeterminate || undefined}
        className={chipClassName}
        data-size={size}
        data-variant={variant}
      >
        {/*
         * `keepMounted` keeps the Indicator span in the DOM across
         * state flips so the CSS opacity-fade has something to animate
         * against (slice-4 review fix item 6). Both glyphs render
         * simultaneously inside — CSS reads `data-glyph` on the SVG
         * and the chip's `data-checked` / `data-indeterminate` to
         * decide which one is visible.
         */}
        <BaseCheckbox.Indicator keepMounted className="zs-checkbox__indicator">
          <IndicatorCheck />
          <IndicatorMinus />
        </BaseCheckbox.Indicator>
      </BaseCheckbox.Root>
    );

    // When `label` is set, wrap chip + text in a <label> so a click on
    // the text toggles the chip via native semantics. The hidden input
    // is what receives the click; Base UI handles the propagation.
    if (label != null) {
      return (
        <SelectionRow
          base="checkbox"
          size={size}
          disabled={disabled}
          className={fieldClassName}
          fieldProps={fieldProps}
        >
          {chip}
          <span className="zs-checkbox-field__text">{label}</span>
        </SelectionRow>
      );
    }

    return chip;
  },
);

Checkbox.displayName = "Checkbox";
