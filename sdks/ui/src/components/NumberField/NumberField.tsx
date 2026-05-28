/*
 * NumberField — text input + stepper buttons + optional drag-to-scrub area.
 *
 * Wraps Base UI's headless `NumberField` primitive. The input itself reads
 * as a sibling of `<Input>` (same height / padding / focus-ring rhythm),
 * with two flanking stepper buttons inside the same bordered shell. An
 * optional drag-to-scrub area on the leading edge lets keyboard-averse
 * users sweep through a range without touching the input.
 *
 *   <NumberField defaultValue={42} min={0} max={100} step={1} />
 *
 * Anatomy (the user sees one component; internally we mount):
 *   NumberField.Root
 *     ├─ NumberField.ScrubArea (optional — drives `showScrub`)
 *     │   └─ NumberField.ScrubAreaCursor
 *     └─ NumberField.Group               (the bordered shell)
 *         ├─ NumberField.Decrement       (the `-` stepper button)
 *         ├─ NumberField.Input           (the native <input>)
 *         └─ NumberField.Increment       (the `+` stepper button)
 *
 * Design principles encoded:
 *
 *   1. The Group reads as an Input. Same `--zs-control-h-{sm,md,lg}`,
 *      same Field cascade, same focus ring lives on the SHELL so the
 *      whole row lights up when the input gains focus.
 *
 *   2. Stepper buttons hit-target floor at 1.75rem (sm) so a finger can
 *      land them. On `pointer: coarse` they grow to `--zs-hit-min`
 *      (2.75rem ≈ 44 device-units) — Apple HIG floor. Brief contingency.
 *
 *   3. The scrub area lives at the input's `start` edge (logical-property
 *      `inset-inline-start`) so RTL flips it automatically. It mounts a
 *      ScrubAreaCursor for the hover-drag cursor swap (resize-EW glyph).
 *
 *   4. Currency / locale formatting is the consumer's call via the
 *      Base UI `format: Intl.NumberFormatOptions` prop. The Currency story
 *      sets `format={{ style: "currency", currency: "USD" }}` and
 *      `snapOnStep` so 0.50 increments don't drift into 0.499999.
 *
 *   5. Required cascades from the enclosing Field. Base UI exposes
 *      `required` on `NumberField.Root` itself; we forward our prop OR
 *      the Field context value so the wired <input> gets `aria-required`
 *      automatically.
 *
 *   6. Size + disabled cascade — same shape as Input + Select + Combobox.
 *      Explicit prop > Field context > default. The cascade is expressed
 *      ONCE here, not duplicated across subparts.
 *
 *   7. forced-colors mirror — every state selector inside the @media
 *      block at equal/higher specificity (Slice 5/6 lesson) so the
 *      system palette wins under high-contrast.
 *
 * Aria contract:
 *   The native `<input>` Base UI emits via NumberField.Input is the
 *   focusable element. It carries `role="spinbutton"`, `aria-valuemin`,
 *   `aria-valuemax`, `aria-valuenow`, and `aria-valuetext` automatically
 *   (Base UI internals). Field auto-wires `aria-describedby` /
 *   `aria-invalid` / `aria-labelledby`. We forward `data-testid` and
 *   `aria-*` to the input — NOT the Root — so tests that locate by
 *   testid hit the actually-focusable node (Combobox lesson, Slice 6).
 */
import {
  forwardRef,
  type AriaAttributes,
  type ComponentPropsWithoutRef,
  type Ref,
} from "react";
import { NumberField as BaseNumberField } from "@base-ui/react/number-field";
import { useFieldContext } from "../Field";
import { classnames } from "../_classnames";

export type NumberFieldSize = "sm" | "md" | "lg";
export type NumberFieldVariant = "default" | "outline";

type BaseRootProps = ComponentPropsWithoutRef<typeof BaseNumberField.Root>;

/* ─── public API ────────────────────────────────────────────────────── */

export interface NumberFieldProps
  extends Omit<BaseRootProps, "className" | "render"> {
  /** Size — sm 32 / md 40 (default) / lg 48 — matches Input rhythm. */
  size?: NumberFieldSize;
  /** Visual variant — `default` filled / `outline` border-only. Mirrors Input. */
  variant?: NumberFieldVariant;
  /**
   * Show the drag-to-scrub area on the input's leading edge. When `true`
   * the cursor turns into a horizontal-resize glyph over the area and
   * dragging changes the value by `step` per pixel of `pixelSensitivity`.
   *
   * @default false
   */
  showScrub?: boolean;
  /** Placeholder for the input. Base UI forwards to the inner <input>. */
  placeholder?: string;
  /** Class hook for the bordered shell. */
  className?: string;
  /**
   * `aria-label` / `aria-labelledby` forwarded to the inner <input>
   * (NOT the Root). Slice-6 Combobox lesson: data-testid and aria-* live
   * on the actually-focusable element.
   */
  "aria-label"?: AriaAttributes["aria-label"];
  "aria-labelledby"?: AriaAttributes["aria-labelledby"];
  /** Optional `data-testid` forwarded to the inner <input>. */
  "data-testid"?: string;
  /** Optional `name` attribute forwarded to the form-submitting hidden input. */
  name?: string;
}

/* ─── component ─────────────────────────────────────────────────────── */

export const NumberField = forwardRef<HTMLDivElement, NumberFieldProps>(
  function NumberField(
    {
      size: sizeProp,
      variant = "default",
      showScrub = false,
      placeholder,
      className,
      required: requiredProp,
      disabled: disabledProp,
      "aria-label": ariaLabel,
      "aria-labelledby": ariaLabelledBy,
      "data-testid": dataTestId,
      ...rest
    },
    ref,
  ) {
    const fieldCtx = useFieldContext();
    // Explicit prop wins, then Field context, then default — same cascade
    // as Input / Select / Combobox.
    const size: NumberFieldSize = sizeProp ?? fieldCtx?.size ?? "md";
    const required = requiredProp ?? fieldCtx?.required ?? false;
    const disabled = disabledProp ?? fieldCtx?.disabled ?? false;

    return (
      <BaseNumberField.Root
        {...(rest as BaseRootProps)}
        ref={ref as Ref<HTMLDivElement>}
        required={required || undefined}
        disabled={disabled || undefined}
        className={classnames(
          "zs-number-field",
          `zs-number-field--${variant}`,
          `zs-number-field--${size}`,
          className,
        )}
        data-variant={variant}
        data-size={size}
      >
        {showScrub ? (
          <BaseNumberField.ScrubArea className="zs-number-field__scrub">
            <BaseNumberField.ScrubAreaCursor className="zs-number-field__scrub-cursor">
              {/* The cursor element gets pointer-locked while dragging.
                  We render a horizontal-resize glyph so the affordance
                  reads. aria-hidden because the spinbutton already
                  announces the value change. */}
              <svg
                viewBox="0 0 26 14"
                aria-hidden="true"
                focusable="false"
                width="26"
                height="14"
              >
                <path
                  fill="currentColor"
                  d="M0 7l5-5v3h16V2l5 5-5 5V9H5v3z"
                />
              </svg>
            </BaseNumberField.ScrubAreaCursor>
          </BaseNumberField.ScrubArea>
        ) : null}
        <BaseNumberField.Group className="zs-number-field__group">
          <BaseNumberField.Decrement
            className="zs-number-field__step zs-number-field__step--dec"
            aria-label="Decrement"
          >
            {/* Minus glyph. SVG so currentColor flows from CSS state. */}
            <svg
              viewBox="0 0 16 16"
              aria-hidden="true"
              focusable="false"
              width="16"
              height="16"
            >
              <path
                fill="currentColor"
                d="M3 7.25h10v1.5H3z"
              />
            </svg>
          </BaseNumberField.Decrement>
          <BaseNumberField.Input
            className="zs-number-field__input"
            placeholder={placeholder}
            aria-label={ariaLabel}
            aria-labelledby={ariaLabelledBy}
            data-testid={dataTestId}
          />
          <BaseNumberField.Increment
            className="zs-number-field__step zs-number-field__step--inc"
            aria-label="Increment"
          >
            {/* Plus glyph. */}
            <svg
              viewBox="0 0 16 16"
              aria-hidden="true"
              focusable="false"
              width="16"
              height="16"
            >
              <path
                fill="currentColor"
                d="M7.25 3h1.5v4.25H13v1.5H8.75V13h-1.5V8.75H3v-1.5h4.25z"
              />
            </svg>
          </BaseNumberField.Increment>
        </BaseNumberField.Group>
      </BaseNumberField.Root>
    );
  },
);
NumberField.displayName = "NumberField";
