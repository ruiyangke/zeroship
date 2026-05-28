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
 *
 *   3. The checked glyph (checkmark) and indeterminate glyph (minus)
 *      are visually distinct paths. Two state signals — fill + shape —
 *      so colorblind users get the same information.
 *
 *   4. The whole row (chip + label text) is the click surface. The
 *      visible chip is small for visual rhythm; the underlying
 *      Field.Label or <label> wraps both so a finger lands inside.
 *      Hit target ≥ 1.75rem on fine pointers, ≥ 2.75rem on coarse.
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
 *
 * The asChild path swaps the chip's outer host (the focusable surface
 * Base UI renders) for a consumer-supplied element via Slot. The hidden
 * input still ships beside it for form submission.
 */
import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type ComponentPropsWithRef,
  type ReactNode,
} from "react";
import { Checkbox as BaseCheckbox } from "@base-ui/react/checkbox";
import { useFieldDisabledContext, useFieldVisualSize } from "../Field";
import { classnames } from "../_classnames";
import { Slot } from "../_slot";

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

  /**
   * Render-as a custom element. Composes via Slot — the consumer's
   * element receives our classNames + data attributes; the focusable
   * surface semantics still come from Base UI's CheckboxRoot.
   */
  asChild?: boolean;

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
  fieldProps?: ComponentPropsWithoutRef<"label">;
}

/* ─── indicator glyphs — two distinct paths so shape ≠ color ────────
 *
 * Both SVGs use `currentColor` so the parent chip's `color` token
 * paints them. The checkmark is the standard tick; the minus is a
 * single horizontal bar so the indeterminate state is unmistakable
 * even in monochrome.
 */
function IndicatorCheck() {
  return (
    <svg
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

export const Checkbox = forwardRef<HTMLButtonElement, CheckboxProps>(
  function Checkbox(
    {
      size: sizeProp,
      variant = "default",
      asChild = false,
      className,
      label,
      fieldClassName,
      fieldProps,
      disabled: disabledProp,
      indeterminate,
      ...rest
    },
    ref,
  ) {
    // Cascade: explicit prop wins; otherwise read the Field context;
    // otherwise fall to the canonical default. Hooks are called
    // unconditionally so React's call-order invariant holds even when
    // the explicit prop is set on one render and absent on the next.
    const fieldSize = useFieldVisualSize();
    const fieldDisabled = useFieldDisabledContext();
    const size: CheckboxSize = sizeProp ?? fieldSize ?? "md";
    const disabled = disabledProp ?? fieldDisabled;

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
        ref={ref}
        disabled={disabled || undefined}
        indeterminate={indeterminate || undefined}
        className={chipClassName}
        data-size={size}
        data-variant={variant}
        render={
          asChild
            ? (props, state) => (
                <Slot
                  {...props}
                  data-checked={state.checked || undefined}
                  data-indeterminate={state.indeterminate || undefined}
                  data-disabled={state.disabled || undefined}
                  data-readonly={state.readOnly || undefined}
                />
              )
            : undefined
        }
      >
        <BaseCheckbox.Indicator className="zs-checkbox__indicator">
          {/* Base UI re-mounts children on state flip, but we want
              the indicator container to persist (CSS fade reads
              opacity, not mount). Render BOTH glyphs and let CSS
              swap by selector. */}
          {indeterminate ? <IndicatorMinus /> : <IndicatorCheck />}
        </BaseCheckbox.Indicator>
      </BaseCheckbox.Root>
    );

    // When `label` is set, wrap chip + text in a <label> so a click on
    // the text toggles the chip via native semantics. The hidden input
    // is what receives the click; Base UI handles the propagation.
    if (label != null) {
      return (
        <label
          {...fieldProps}
          className={classnames(
            "zs-checkbox-field",
            `zs-checkbox-field--${size}`,
            fieldClassName,
            fieldProps?.className,
          )}
          data-size={size}
          data-disabled={disabled || undefined}
        >
          {chip}
          <span className="zs-checkbox-field__text">{label}</span>
        </label>
      );
    }

    return chip;
  },
);

Checkbox.displayName = "Checkbox";
