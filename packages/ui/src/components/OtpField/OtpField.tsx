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
 *      (matches Input md); sm = 2rem (matches Input sm). Every size is
 *      floored by `--zs-hit-min` on coarse-pointer pointers via a
 *      `max()` clamp so the cell always meets the WCAG 2.5.5 touch
 *      target (44 device-units / 2.75rem) without disturbing the
 *      desktop density.
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
 *      under high-contrast. Readonly + completed-focus included so the
 *      hairline + focus ring survive system-color swap.
 *
 *   7. RTL — cells flow inline; the row's `gap` is logical, so the cells
 *      reorder right-to-left without a JS branch.
 *
 *   8. The validation-input that Base UI emits for native form submission
 *      is hidden but participates in `<form>` requiredness validation —
 *      Field.Error's `match` works against it.
 *
 * Aria contract:
 *   - Every cell input receives a composed `aria-labelledby` pointing at
 *     a visually-hidden per-cell `<span>` so the announceable name is
 *     "<group label>, character N of M" on EVERY cell — including
 *     cell 0, which Base UI intentionally drops `aria-label` on. The
 *     group label comes from (in priority order) `aria-labelledby`,
 *     `aria-label` (mirrored into a hidden span we own), Field.Label
 *     (via Base UI's LabelableContext), or our own "Verification
 *     code" fallback span when none of those are present.
 *   - `aria-describedby` is forwarded to EACH focusable cell input
 *     (not just the non-focusable Root group) so the description is
 *     announced on focus. The Root still carries `role="group"` +
 *     describedby from Base UI for AT that announces groups.
 *   - Base UI auto-wires `aria-invalid` from the Field validation
 *     cascade, and the hidden form-submission input keeps
 *     `aria-hidden`. Tests that need to target a specific cell read
 *     by `data-index` (Base UI emits it) or via the rendered
 *     `<input data-testid>` we forward per-cell as
 *     `<root-testid>-cell-<index>`.
 */
import {
  forwardRef,
  Fragment,
  useId,
  type AriaAttributes,
  type ComponentPropsWithoutRef,
  type ComponentPropsWithRef,
  type CSSProperties,
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

/**
 * Visually-hidden style mirroring the slate's story-side `srOnly` helper.
 * Used for the cell-0 accessible-name fallback `<span>` when no Field
 * wrapper / aria-label / aria-labelledby is supplied. Inline so we don't
 * need to introduce a generic utility class.
 */
const visuallyHiddenStyle: CSSProperties = {
  position: "absolute",
  inlineSize: 1,
  blockSize: 1,
  margin: -1,
  padding: 0,
  overflow: "hidden",
  clip: "rect(0 0 0 0)",
  whiteSpace: "nowrap",
  border: 0,
};

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

    // ── Per-cell labelling ───────────────────────────────────────────
    //
    // Base UI's OTPFieldInput.js sets `ariaLabel = index === 0 ?
    // undefined : slotAriaLabel` — the first cell intentionally drops
    // any per-cell `aria-label` so it can inherit from a real `<label>`
    // / `<Field.Label>` association. Worse: when `aria-label` IS set
    // on cells 1..N-1, Base UI ALSO blanks out `aria-labelledby` on
    // those cells (input line 101), so the group label disappears for
    // every cell except 0. Either way, naming the row uniformly via
    // `aria-label` is broken.
    //
    // The fix is to ALWAYS use `aria-labelledby` per cell, composing:
    //   1. the group label id (whatever Base UI's Root resolves —
    //      consumer's `aria-labelledby`, Field.Label via
    //      LabelableContext, or our hidden-span fallback below), AND
    //   2. a visually-hidden per-cell `<span>` reading
    //      "Character N of M".
    //
    // Each cell then announces "<group>, character N of M" — including
    // cell 0, which Base UI was previously refusing to label.
    //
    // We read the resolved group-label id from `rootProps[
    // "aria-labelledby"]` inside the render callback (Base UI has
    // already chained Field.Label / consumer prop / its fallback into
    // that single string by then). For the fully-bare standalone case
    // (no Field, no consumer prop, no `<label>`-source), Base UI's
    // resolution returns `undefined`; we add our own hidden-label
    // fallback below so cell 0 still has a name.
    const hiddenLabelId = useId();
    const cellLabelIdPrefix = useId();
    // Hidden-label fallback text. Used only when neither a Field wraps
    // us NOR the consumer forwarded `aria-labelledby` (in the second
    // case the consumer's `<span>` already owns the group name).
    // When the consumer forwarded `aria-label` instead, we mirror that
    // text into our hidden span because Base UI strips `aria-label`
    // from the Root group, leaving nothing for an `aria-labelledby`
    // chain to point at.
    const renderGroupHiddenLabel =
      ariaLabelledBy == null && fieldCtx == null;
    const groupFallbackText: string =
      ariaLabel != null ? ariaLabel : "Verification code";

    // ── Root aria-* spread ───────────────────────────────────────────
    //
    // The Root is a `<div role="group">`. Its `aria-labelledby` is what
    // Base UI surfaces back via `rootProps["aria-labelledby"]` in the
    // render callback. We seed it with the consumer's id (if any) or
    // our hidden-span id (bare standalone case); inside a Field, Base
    // UI's LabelableContext puts the Field.Label id there for us.
    const rootAria: Record<string, AriaAttributes[keyof AriaAttributes]> = {};
    if (ariaLabel != null) rootAria["aria-label"] = ariaLabel;
    if (ariaLabelledBy != null) rootAria["aria-labelledby"] = ariaLabelledBy;
    else if (renderGroupHiddenLabel)
      rootAria["aria-labelledby"] = hiddenLabelId;
    if (ariaDescribedBy != null) rootAria["aria-describedby"] = ariaDescribedBy;

    return (
      <BaseOTPField.Root
        {...(rest as BaseRootProps)}
        ref={ref}
        length={length}
        required={required || undefined}
        disabled={disabled || undefined}
        {...rootAria}
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
          // Surface validity on the AT-focusable cells. Base UI stamps
          // `aria-invalid` only on the hidden validation `<input>` and
          // `data-invalid` on the Root group — neither of which a screen
          // reader announces when a *cell* is focused. Forward
          // `aria-invalid="true"` onto every cell whenever the field is
          // invalid so AT users hear the error state on the element they
          // are actually editing. `state.valid` is `false` (invalid),
          // `true` (valid), or `null` (not yet validated); only the
          // explicit-false case marks the cells invalid.
          const cellAriaInvalid =
            state.valid === false ? ("true" as const) : undefined;
          // Compose `aria-describedby` to forward onto each focusable
          // cell input. Base UI's Root merges the Field-auto-wired
          // description id with the consumer-provided one and stamps
          // the result on `rootProps["aria-describedby"]`. That id
          // chain is exactly what each cell needs to surface the
          // description on focus.
          const cellDescribedBy = rootProps["aria-describedby"];
          // Resolved group-label id — Base UI has already chained
          // consumer prop, Field.Label (via LabelableContext), and the
          // hidden-span fallback (we seeded it on the Root above) into
          // a single space-separated id string.
          const rootLabelledBy = rootProps["aria-labelledby"];
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
              {/* Group-name hidden span. Rendered when neither
                * aria-labelledby nor a Field wraps us. Cell 0 carries
                * no per-cell aria-label (Base UI strips it by design)
                * so the group label is the ONLY thing naming cell 0.
                * The hidden-span id is seeded on the Root's
                * aria-labelledby above, so Base UI resolves it back
                * onto `rootProps["aria-labelledby"]` for us to compose
                * into each per-cell aria-labelledby chain below. */}
              {renderGroupHiddenLabel ? (
                <span id={hiddenLabelId} style={visuallyHiddenStyle}>
                  {groupFallbackText}
                </span>
              ) : null}
              {Array.from({ length }, (_, index) => {
                const cellId = `${cellLabelIdPrefix}-${index}`;
                // Compose per-cell `aria-labelledby` as `<root group
                // labelledby> <cell index id>`. The root id chain is
                // whatever Base UI resolved (Field.Label / consumer
                // `aria-labelledby` / our hidden span). The cell index
                // id labels each individual cell with "Character N of
                // M" so each input announces "<group>, character N of
                // M" — including cell 0, which Base UI's per-cell
                // `aria-label` codepath refuses to label.
                const cellLabelledBy = rootLabelledBy
                  ? `${rootLabelledBy} ${cellId}`
                  : cellId;
                return (
                  // Base UI's OTPField.Input derives its `index` from the
                  // composite-list order (useCompositeListItem inside the
                  // primitive), so we DO NOT pass an `index` prop — we
                  // just render one Input per slot.
                  //
                  // Per-cell labelling: we DO NOT pass `aria-label`
                  // (Base UI drops it on cell 0 AND blanks
                  // `aria-labelledby` on cells 1..N when present, so
                  // the group name disappears). `aria-labelledby` is
                  // forwarded straight through Base UI's OTPFieldInput,
                  // so cells 0..N-1 all read "<group>, character N of
                  // M".
                  //
                  // Per-cell describedby: we also forward the merged
                  // describedby that Base UI's Root computes (Field
                  // description + caller-provided ids). Without this,
                  // describedby lives ONLY on the non-focusable group,
                  // so AT never announces it on cell focus.
                  <Fragment key={index}>
                    <span id={cellId} style={visuallyHiddenStyle}>
                      Character {index + 1} of {length}
                    </span>
                    <BaseOTPField.Input
                      className="zs-otp-field__input"
                      aria-labelledby={cellLabelledBy}
                      aria-describedby={cellDescribedBy}
                      aria-invalid={cellAriaInvalid}
                      data-testid={
                        dataTestId ? `${dataTestId}-cell-${index}` : undefined
                      }
                    />
                  </Fragment>
                );
              })}
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
