/*
 * OtpField — N-digit one-time-password code entry.
 *
 * Wraps Base UI's headless `otp-field` primitive. Each cell is its own
 * single-character `<input>`; Base UI owns the auto-advance focus flow,
 * paste-splitting across cells, backspace-deletes-and-rewinds behavior,
 * and the hidden form-submission input. Consumers see the simple shape:
 *
 *   <OtpField length={6} onValueComplete={(code) => verify(code)} />
 *
 * Inside a `<Field>`, the cells inherit size + required + disabled like
 * every other form control. The Field's `<Field.Label>` auto-associates
 * with the first cell via Base UI's id chain.
 *
 * Anatomy:
 *   OtpField.Root          (the row container; data-* state surface)
 *     ├─ OtpField.Input    (cell index 0)
 *     ├─ OtpField.Input    (cell index 1)
 *     ├─ …
 *     └─ OtpField.Input    (cell index N-1)
 *
 * Design principles encoded:
 *
 *   1. Cells share the Input visual rhythm. Each cell is a bordered shell
 *      with the same focus-ring, the same hairline border, and the same
 *      `--zs-control-h-*` heights so a row of OtpField cells next to an
 *      Input reads coherent. The cell uses --zs-otp-cell-size for the
 *      square inline-size — height-equals-width by design.
 *
 *   2. Auto-advance + paste-split are Base UI's responsibility. We DO NOT
 *      reimplement the focus dance. The contingency in the brief was to
 *      verify paste; the aria-wiring suite asserts that a single
 *      paste of N digits fills all N cells AND advances focus to the end.
 *
 *   3. Sizes — sm/md/lg — drive `--zs-otp-cell-size`. Each cell is square,
 *      so the inline-size === block-size. lg cells = 3rem; md = 2.5rem
 *      (matches Input md); sm = 2rem (matches Input sm).
 *
 *   4. Variants — `default` filled / `outline` border-only. Mirrors Input
 *      so consumers can match an OtpField next to an Input without
 *      visual jarring.
 *
 *   5. Field cascade — size, required, disabled. Explicit prop wins, then
 *      Field context, then default. Same shape as every other form
 *      primitive in the slate.
 *
 *   6. forced-colors mirror — every state selector inside the @media
 *      block at equal/higher specificity so the system palette wins
 *      under high-contrast.
 *
 *   7. RTL — cells flow inline; the row's `gap` is logical, so the cells
 *      reorder right-to-left without a JS branch.
 *
 *   8. The validation-input that Base UI emits for native form submission
 *      is hidden but participates in `<form>` requiredness validation —
 *      Field.Error's `match` works against it.
 *
 * Aria contract:
 *   Base UI auto-wires per-cell `aria-label` ("Character N"), the
 *   hidden form input's `aria-hidden`, and the Field cascade's
 *   `aria-describedby` / `aria-invalid`. We forward `aria-label` /
 *   `aria-labelledby` / `aria-describedby` to the ROOT (it's the
 *   announceable group) and let Base UI propagate as needed. Tests that
 *   need to target a specific cell read by `data-index` (Base UI emits
 *   it) or via the rendered `<input data-testid>` we forward to cell 0.
 */
import {
  forwardRef,
  type AriaAttributes,
  type ComponentPropsWithoutRef,
  type ComponentPropsWithRef,
  type HTMLAttributes,
} from "react";
// Base UI ships the OTP Field as a `preview` namespace in 1.5.0 — the
// component is feature-complete but the API is reserved as in-preview
// (the eventual stable export name will likely be `OTPField`). We
// alias once here to keep the rest of the file naming-stable.
import { OTPFieldPreview as BaseOTPField } from "@base-ui/react/otp-field";
import { useFieldContext } from "../Field";
import { classnames } from "../_classnames";

export type OtpFieldSize = "sm" | "md" | "lg";
export type OtpFieldVariant = "default" | "outline";

type BaseRootProps = ComponentPropsWithRef<typeof BaseOTPField.Root>;

/**
 * Shape of `state` Base UI's OTPField.Root render callback hands us.
 * Subsetted to the flags we actually stamp on the rendered row.
 *
 * Sourced from `@base-ui/react/otp-field/root/OTPFieldRoot.d.ts`.
 */
type OtpFieldRootRenderState = {
  complete: boolean;
  disabled: boolean;
  filled: boolean;
  focused: boolean;
  readOnly: boolean;
  /** `true` valid · `false` invalid · `null` not-yet-validated */
  valid: boolean | null;
};

export interface OtpFieldProps
  extends Omit<BaseRootProps, "className" | "render" | "length"> {
  /** Number of digits — default 6. */
  length?: number;
  /** Visual size — sm 32 / md 40 (default) / lg 48. */
  size?: OtpFieldSize;
  /** Visual variant — `default` filled / `outline` border-only. Mirrors Input. */
  variant?: OtpFieldVariant;
  /** Class hook on the row container. */
  className?: string;
  /**
   * `aria-label` / `aria-labelledby` / `aria-describedby` forwarded to
   * the ROOT (which Base UI announces as a group). For the individual
   * cells the AT-focusable elements are the per-cell `<input>`s; Base UI
   * auto-labels them as "Character 1", "Character 2", etc. so the group
   * label here is what the user hears on focus.
   */
  "aria-label"?: AriaAttributes["aria-label"];
  "aria-labelledby"?: AriaAttributes["aria-labelledby"];
  "aria-describedby"?: AriaAttributes["aria-describedby"];
  /** Optional `data-testid` forwarded to the row container. Cells expose
   *  their index via `data-index` (Base UI). */
  "data-testid"?: string;
}

/* ─── component ─────────────────────────────────────────────────────── */

export const OtpField = forwardRef<HTMLDivElement, OtpFieldProps>(
  function OtpField(
    {
      length = 6,
      size: sizeProp,
      variant = "default",
      className,
      required: requiredProp,
      disabled: disabledProp,
      "aria-label": ariaLabel,
      "aria-labelledby": ariaLabelledBy,
      "aria-describedby": ariaDescribedBy,
      "data-testid": dataTestId,
      ...rest
    },
    ref,
  ) {
    const fieldCtx = useFieldContext();
    // Explicit prop > Field context > default. Identical to Input /
    // NumberField / Slider cascade.
    const size: OtpFieldSize = sizeProp ?? fieldCtx?.size ?? "md";
    const required = requiredProp ?? fieldCtx?.required ?? false;
    const disabled = disabledProp ?? fieldCtx?.disabled ?? false;

    // Build aria-* spread only with defined keys so undefined values
    // don't clobber Base UI's auto-wired labelledby chain from Field.
    const ariaForwarded: Record<string, AriaAttributes[keyof AriaAttributes]> =
      {};
    if (ariaLabel != null) ariaForwarded["aria-label"] = ariaLabel;
    if (ariaLabelledBy != null)
      ariaForwarded["aria-labelledby"] = ariaLabelledBy;
    if (ariaDescribedBy != null)
      ariaForwarded["aria-describedby"] = ariaDescribedBy;

    return (
      <BaseOTPField.Root
        {...(rest as BaseRootProps)}
        ref={ref}
        length={length}
        required={required || undefined}
        disabled={disabled || undefined}
        {...ariaForwarded}
        // Mirror the NumberField pattern: re-stamp data-* attributes from
        // Base UI's render-callback state so CSS attribute selectors
        // light up under bare AND Field-wrapped usage.
        render={(
          rootProps: HTMLAttributes<HTMLDivElement>,
          state: OtpFieldRootRenderState,
        ) => {
          const dataFocused = state.focused ? "" : undefined;
          const dataFilled = state.filled ? "" : undefined;
          const dataDisabled = state.disabled ? "" : undefined;
          const dataReadonly = state.readOnly ? "" : undefined;
          const dataInvalid = state.valid === false ? "" : undefined;
          const dataComplete = state.complete ? "" : undefined;
          return (
            <div
              {...rootProps}
              className={classnames(
                "zs-otp-field",
                `zs-otp-field--${variant}`,
                `zs-otp-field--${size}`,
                className,
                rootProps.className,
              )}
              data-variant={variant}
              data-size={size}
              data-focused={dataFocused}
              data-filled={dataFilled}
              data-disabled={dataDisabled}
              data-readonly={dataReadonly}
              data-invalid={dataInvalid}
              data-complete={dataComplete}
              data-testid={dataTestId}
            >
              {Array.from({ length }, (_, index) => (
                // Base UI's OTPField.Input derives its `index` from the
                // composite-list order (useCompositeListItem inside the
                // primitive), so we DO NOT pass an `index` prop — we
                // just render one Input per slot.
                //
                // Per-cell aria-label: Base UI ignores aria-label on
                // cell 0 (the first input is supposed to inherit from a
                // <label> or <Field.Label>); for cells 1..N-1 Base UI
                // synthesizes "Character N" automatically when no
                // external label is provided. axe still flags cell 0
                // when there's no Field wrap (the bare Basic / AllSizes
                // stories), so we forward an explicit per-cell aria-
                // label whose first-cell value composes with whatever
                // the Root's group label says: "Verification code,
                // Character 1 of 6". Inside a Field, Base UI's
                // labelledby chain wins (we don't override there
                // because the per-cell aria-label is additive — both
                // are announced).
                <BaseOTPField.Input
                  key={index}
                  className="zs-otp-field__input"
                  aria-label={
                    ariaLabel != null
                      ? `${ariaLabel}, character ${index + 1} of ${length}`
                      : `Character ${index + 1} of ${length}`
                  }
                  data-testid={
                    dataTestId ? `${dataTestId}-cell-${index}` : undefined
                  }
                />
              ))}
            </div>
          );
        }}
      />
    );
  },
);
OtpField.displayName = "OtpField";

// Re-export Base UI's Input type signature for consumers that need the
// per-cell prop shape (rare; consumers normally just pass length).
export type OtpFieldInputProps = ComponentPropsWithoutRef<
  typeof BaseOTPField.Input
>;
