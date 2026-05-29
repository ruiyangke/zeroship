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
 *      (2.75rem ≈ 44 device-units) — the coarse-pointer minimum target
 *      we adopt platform-wide. Brief contingency.
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
  useState,
  type AriaAttributes,
  type ComponentPropsWithoutRef,
  type FocusEvent as ReactFocusEvent,
  type HTMLAttributes,
  type InputHTMLAttributes,
  type Ref,
} from "react";
import { NumberField as BaseNumberField } from "@base-ui/react/number-field";
import { useFieldContext } from "../Field";
import { classnames } from "../_classnames";

export type NumberFieldSize = "sm" | "md" | "lg";
export type NumberFieldVariant = "default" | "outline";

type BaseRootProps = ComponentPropsWithoutRef<typeof BaseNumberField.Root>;

/**
 * Shape of `state` Base UI's NumberField.Root render callback hands us.
 * Mirrors `NumberFieldRootState` (which extends `FieldRootState`); we
 * subset to the flags we actually stamp as data-* on the rendered Root.
 *
 * Sourced from `@base-ui/react/number-field/root/NumberFieldRoot.d.ts`.
 * Re-declared here (rather than imported) because Base UI doesn't
 * publish this as a named export and we only need the read-only shape.
 */
type NumberFieldRootRenderState = {
  disabled: boolean;
  focused: boolean;
  filled: boolean;
  readOnly: boolean;
  /** `true` valid · `false` invalid · `null` not-yet-validated */
  valid: boolean | null;
  scrubbing: boolean;
};

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
  /**
   * Class hook for the Root (`.zs-number-field`) — the positioning
   * context that anchors the scrub area. The bordered shell is the
   * inner `.zs-number-field__group`; consumers wanting to retheme just
   * the shell should target that descendant from the Root class.
   */
  className?: string;
  /**
   * `aria-label` / `aria-labelledby` / `aria-describedby` forwarded to
   * the inner <input> (NOT the Root). Slice-6 Combobox lesson:
   * data-testid and aria-* live on the actually-focusable element so
   * screen readers announce help text against the spinbutton, not a
   * decorative wrapper div.
   */
  "aria-label"?: AriaAttributes["aria-label"];
  "aria-labelledby"?: AriaAttributes["aria-labelledby"];
  "aria-describedby"?: AriaAttributes["aria-describedby"];
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
      "aria-describedby": ariaDescribedBy,
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

    // ─────────────────────────────────────────────────────────────────
    // BARE focus tracking.
    //
    // Base UI's `NumberField.Root` reads `focused` from
    // `useFieldRootContext()`. When mounted OUTSIDE a `Field.Root` the
    // default field context's `setFocused` is NOOP, so `state.focused`
    // stays `false` regardless of where focus actually lives — meaning
    // the focus ring never fires for bare (Field-less) usage (the
    // Slice-7 review finding).
    //
    // Mirror what Field.Root would do: track focus via focusin /
    // focusout on the rendered Root and union with Base UI's state at
    // render time. When the component IS wrapped in a Field, Base UI's
    // state.focused will already be true on focus; our local flag is
    // additive and harmless.
    const [bareFocused, setBareFocused] = useState(false);
    const handleFocus = (event: ReactFocusEvent<HTMLDivElement>) => {
      // React's onFocus bubbles (it's the focusin synthetic), so any
      // descendant gaining focus flips us on.
      if (event.currentTarget.contains(event.target)) setBareFocused(true);
    };
    const handleBlur = (event: ReactFocusEvent<HTMLDivElement>) => {
      // focusout-equivalent: relatedTarget is the node receiving focus.
      // If that's still inside the Root, the focus didn't leave — ignore.
      const next = event.relatedTarget as Node | null;
      if (next && event.currentTarget.contains(next)) return;
      setBareFocused(false);
    };

    return (
      <BaseNumberField.Root
        {...(rest as BaseRootProps)}
        ref={ref as Ref<HTMLDivElement>}
        required={required || undefined}
        disabled={disabled || undefined}
        // ─────────────────────────────────────────────────────────────
        // Render-callback: stamp data-focused / data-filled / data-invalid
        // on the Root manually. Base UI emits these from its
        // `useFieldRootContext()` lookup — when NumberField is mounted
        // BARE (outside Field.Root), the default context's `setFocused`
        // is NOOP and `focused` stays `false`, so the focus ring never
        // fires (Slice-7 review). By accepting Base UI's `state` and
        // re-stamping the attributes ourselves, the rest-state focus ring
        // works for bare NumberField (Basic / MinMaxStep / Currency /
        // ScrubArea stories) as well as Field-wrapped usage.
        //
        // Mirrors Input.tsx:234-269. `data-disabled` already lands via
        // the `disabled={disabled || undefined}` HTML attribute on the
        // div, but we re-stamp explicitly so the CSS attribute selector
        // `.zs-number-field[data-disabled]` fires whether disabled
        // comes from a bare prop, the Field cascade, or Base UI's
        // internal disabled flow.
        render={(
          rootProps: HTMLAttributes<HTMLDivElement>,
          state: NumberFieldRootRenderState,
        ) => {
          // Union Base UI's state (which goes true under Field cascade)
          // with our local focusin/focusout flag (which goes true under
          // bare usage). Either path lights the ring.
          const isFocused = state.focused || bareFocused;
          const dataFocused = isFocused ? "" : undefined;
          const dataFilled = state.filled ? "" : undefined;
          const dataDisabled = state.disabled ? "" : undefined;
          const dataReadonly = state.readOnly ? "" : undefined;
          // `valid` is null until the field has been touched/submitted;
          // surface `data-invalid` only when explicitly false so the
          // styling-only invalid path matches Input's contract.
          const dataInvalid = state.valid === false ? "" : undefined;
          // Compose Base UI's rootProps.onFocus / onBlur (which may
          // carry handlers a parent forwarded down via inherited root
          // props) with our local bare-focus tracking. Replacing the
          // handlers — the pre-fix shape — silently dropped any
          // consumer-attached focus listeners (Wave-9 review item 2).
          const composedFocus = (
            event: ReactFocusEvent<HTMLDivElement>,
          ) => {
            rootProps.onFocus?.(event);
            handleFocus(event);
          };
          const composedBlur = (
            event: ReactFocusEvent<HTMLDivElement>,
          ) => {
            rootProps.onBlur?.(event);
            handleBlur(event);
          };
          return (
            <div
              {...rootProps}
              className={classnames(
                "zs-number-field",
                `zs-number-field--${variant}`,
                `zs-number-field--${size}`,
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
              onFocus={composedFocus}
              onBlur={composedBlur}
            />
          );
        }}
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
            data-testid={dataTestId}
            // ───────────────────────────────────────────────────────
            // Aria merge — render-callback path (Wave-9 review item 1).
            //
            // Base UI's NumberField.Input auto-wires `aria-labelledby`
            // from the enclosing Field's label and `aria-describedby`
            // from Field.Description / Field.Error via
            // `validation.getValidationProps()`. Its internal
            // `mergeProps` writes the caller's value EVEN WHEN
            // UNDEFINED (`mergedProps[propName] = externalPropValue`,
            // sdks/@base-ui/merge-props/mergeProps.js), so passing
            // `aria-label={undefined}` / `aria-labelledby={undefined}`
            // /`aria-describedby={undefined}` at the prop layer
            // silently wipes Field's auto-wired ids. That breaks the
            // real-path aria-wiring contract — the spinbutton would
            // not announce the description / error against the
            // input.
            //
            // Fix: take Base UI's merged inputProps (which already
            // carry the auto-wired aria) inside the render callback
            // and only apply the caller's aria when it's actually
            // defined. `aria-describedby` is UNIONED so external
            // help text composes with Field's announcements (Input
            // pattern, sdks/ui/src/components/Input/Input.tsx:243-250).
            render={(
              inputProps: InputHTMLAttributes<HTMLInputElement>,
            ) => {
              const mergedDescribedBy =
                [inputProps["aria-describedby"], ariaDescribedBy]
                  .filter(Boolean)
                  .join(" ") || undefined;
              return (
                <input
                  {...inputProps}
                  // Defined-only overrides — preserve Base UI's
                  // auto-wired ids when the caller didn't pass one.
                  {...(ariaLabel !== undefined
                    ? { "aria-label": ariaLabel }
                    : null)}
                  {...(ariaLabelledBy !== undefined
                    ? { "aria-labelledby": ariaLabelledBy }
                    : null)}
                  aria-describedby={mergedDescribedBy}
                />
              );
            }}
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
