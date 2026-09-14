/*
 * Slider — continuous range control with one (or two) draggable thumbs.
 *
 * Wraps Base UI's headless `slider` primitive. Single-thumb is the default
 * (a scalar value); pass `value={[20, 60]}` to enter range mode (Base UI
 * auto-detects the array shape). The component renders Root + Control +
 * Track + Indicator + Thumb(s) internally so consumers write the simple
 * shape:
 *
 *   <Slider defaultValue={50} min={0} max={100} step={1} />
 *
 *   <Slider value={[20, 60]} onValueChange={setRange} />
 *
 * Anatomy:
 *   Slider.Root
 *     ├─ Slider.Value             (optional numeric badge — `showValue`)
 *     └─ Slider.Control
 *         └─ Slider.Track          (the rail)
 *             ├─ Slider.Indicator  (the filled progress segment)
 *             ├─ Slider.Thumb      (index 0)
 *             └─ Slider.Thumb      (index 1 — range only)
 *
 * Design principles encoded:
 *
 *   1. Size — sm 24 / md 32 / lg 40 — calls out the TRACK block-size, not
 *      the thumb's. The thumb's hit target is always ≥ 1.75rem; if the
 *      visual thumb is smaller, padding pushes the touchable area out so
 *      a finger lands. Brief floor.
 *
 *   2. Vertical orientation — pass `orientation="vertical"`. The track
 *      flips to the block-axis via CSS logical sizing, no JS branch.
 *
 *   3. Range mode is implicit — Base UI looks at `value` / `defaultValue`
 *      and renders one thumb for a scalar, two for a 2-element array. We
 *      detect the same shape so we can render the matching number of
 *      <Slider.Thumb> children (the part is REQUIRED to be present per
 *      thumb — Base UI doesn't synthesize them).
 *
 *   4. forced-colors mirror — every state selector inside the @media
 *      block at equal/higher specificity (Slice 5/6 lesson) so the
 *      system palette wins under high-contrast emulation. The aria-wiring
 *      assertion verifies this directly (Slider forced-colors hover
 *      thumb).
 *
 *   5. RTL flips via logical properties — Base UI emits inline-size on
 *      the indicator + inline-axis translates on the thumb so RTL flips
 *      the visual progress direction automatically.
 *
 *   6. Field cascade — required + disabled + size flow through the
 *      enclosing Field exactly like Input / NumberField / Combobox.
 *
 *   7. Reduced motion — every transition is honored by the
 *      `prefers-reduced-motion: reduce` override on root tokens; the
 *      thumb animation only fires on press / drag changes.
 *
 * Aria contract:
 *   Base UI's `Slider.Thumb` renders a real `<input type="range">` as a
 *   nested element — that's the AT-focusable node. It carries `aria-min`,
 *   `aria-max`, `aria-valuenow`, and `aria-valuetext` automatically.
 *   `Slider.Label` (when wrapped in Field) auto-associates with the
 *   thumb input.
 *
 *   For range mode, each Thumb gets its own input with its own value
 *   range so keyboard users can adjust either end independently. We
 *   forward `data-testid` to the THUMB element (not the input) so tests
 *   can locate the visible draggable; the Slice-6 Combobox lesson says
 *   testid + aria-* should hit the AT-focusable node — but for Slider,
 *   Base UI's input is the focusable, the Thumb is the draggable. Tests
 *   that want to drag use the Thumb's testid; tests that want to
 *   dispatch keyboard events use the input child.
 */
import {
  forwardRef,
  useCallback,
  useId,
  useRef,
  useState,
  type ComponentPropsWithoutRef,
  type CSSProperties,
  type JSX,
  type ReactNode,
  type RefAttributes,
} from "react";
import { Slider as BaseSlider } from "@base-ui/react/slider";
import { useFieldContext } from "../Field";
import { classnames } from "../_classnames";

export type SliderSize = "sm" | "md" | "lg";
export type SliderVariant = "default" | "outline";
export type SliderOrientation = "horizontal" | "vertical";

/* Visually-hidden style used for the per-thumb suffix `<span>`s that
 * carry the "(1 of N)" / "(2 of N)" text referenced by each thumb's
 * `aria-labelledby` chain. Same shape as OtpField's cell-0 fallback —
 * the @zeroship/ui slate doesn't have a generic visually-hidden
 * utility class yet, so we duplicate the inline style here instead of
 * inventing one just for this fix. */
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

/* ─── public API ────────────────────────────────────────────────────── */

type BaseRootProps = ComponentPropsWithoutRef<typeof BaseSlider.Root>;

/**
 * Shared props between the single-thumb and range branches. Re-declares
 * `value` / `defaultValue` / `onValueChange` in the branch types so the
 * discriminant — the value's array vs scalar shape — narrows correctly.
 */
interface SliderBaseProps
  extends Omit<
    BaseRootProps,
    | "className"
    | "render"
    | "value"
    | "defaultValue"
    | "onValueChange"
    | "onValueCommitted"
  > {
  /** Size — sm 24 / md 32 (default) / lg 40 (track block-size). */
  size?: SliderSize;
  /** Visual variant — `default` solid accent / `outline` rim. */
  variant?: SliderVariant;
  /** Show the current numeric value next to the slider. */
  showValue?: boolean;
  /**
   * Marks the slider as required. Drives `aria-required` on each thumb's
   * nested `<input type="range">` (the AT-focusable element). Cascades
   * from `<Field required>` — explicit prop wins, then Field context,
   * then `false`. Base UI's `Slider.Root` does not accept `required`
   * directly, so we attach the attribute via the Thumb's `inputRef`.
   */
  required?: boolean;
  /** Class hook for the Root. */
  className?: string;
  /** Optional thumb hit-target descriptor (slice contingency); for tests. */
  "data-testid"?: string;
  /** When `showValue`, an optional formatter via Intl.NumberFormatOptions. */
  format?: Intl.NumberFormatOptions;
  /** Optional ReactNode rendered above the track — e.g. a Field.Label. */
  children?: ReactNode;
}

/** Single-thumb (scalar value). Default branch. */
export interface SliderSingleProps extends SliderBaseProps {
  /** Controlled scalar value. */
  value?: number;
  /** Uncontrolled scalar default. */
  defaultValue?: number;
  /** Change callback — fires while dragging. */
  onValueChange?: (
    value: number,
    eventDetails: Parameters<NonNullable<BaseRootProps["onValueChange"]>>[1],
  ) => void;
  /** Commit callback — fires on pointer-up. */
  onValueCommitted?: (
    value: number,
    eventDetails: Parameters<NonNullable<BaseRootProps["onValueCommitted"]>>[1],
  ) => void;
}

/** Range (two thumbs, array value). Discriminated on Array.isArray(value). */
export interface SliderRangeProps extends SliderBaseProps {
  /** Controlled 2-element value (or longer — Base UI handles it). */
  value: readonly number[];
  /** Uncontrolled 2-element default. */
  defaultValue?: readonly number[];
  /** Change callback receives the next array. */
  onValueChange?: (
    value: number[],
    eventDetails: Parameters<NonNullable<BaseRootProps["onValueChange"]>>[1],
  ) => void;
  /** Commit callback receives the next array. */
  onValueCommitted?: (
    value: number[],
    eventDetails: Parameters<NonNullable<BaseRootProps["onValueCommitted"]>>[1],
  ) => void;
}

/** Range mode using only `defaultValue` (uncontrolled). */
export interface SliderRangeUncontrolledProps extends SliderBaseProps {
  value?: undefined;
  defaultValue: readonly number[];
  onValueChange?: (
    value: number[],
    eventDetails: Parameters<NonNullable<BaseRootProps["onValueChange"]>>[1],
  ) => void;
  onValueCommitted?: (
    value: number[],
    eventDetails: Parameters<NonNullable<BaseRootProps["onValueCommitted"]>>[1],
  ) => void;
}

export type SliderProps =
  | SliderSingleProps
  | SliderRangeProps
  | SliderRangeUncontrolledProps;

/* ─── component ─────────────────────────────────────────────────────── *
 *
 * `SliderComponent` is a call-signature overload pair so the consumer's
 * `onValueChange` parameter narrows from the value/defaultValue shape at
 * the call site. Pre-fix: the single `SliderProps` union signature locked
 * the callback to `(number | number[]) => void`, so a story shape
 * `<Slider value={[20, 60]} onValueChange={(next) => …}>` left `next`
 * typed implicit-any (TypeScript can't narrow a unionised callback
 * parameter by the discriminant). Post-fix: TS tries the Single branch
 * first, fails when `value` / `defaultValue` is an array, then tries
 * Range with `Value=number[]` narrowing intact. Same shape as the
 * Slice-6 Select discriminated-union fix. */
export interface SliderComponent {
  (props: SliderSingleProps & RefAttributes<HTMLDivElement>): JSX.Element;
  (props: SliderRangeProps & RefAttributes<HTMLDivElement>): JSX.Element;
  (
    props: SliderRangeUncontrolledProps & RefAttributes<HTMLDivElement>,
  ): JSX.Element;
  displayName?: string;
}

const SliderForward = forwardRef<HTMLDivElement, SliderProps>(function Slider(
  props,
  ref,
) {
  const {
    size: sizeProp,
    variant = "default",
    showValue = false,
    className,
    orientation = "horizontal",
    disabled: disabledProp,
    required: requiredProp,
    children,
    value,
    defaultValue,
    onValueChange,
    onValueCommitted,
    format,
    "data-testid": dataTestId,
    // The forwarded `aria-label` / `aria-labelledby` / `aria-describedby`
    // belong on the THUMB's nested <input type="range"> (that's the
    // AT-focusable element), not on the Root <div>. Pull them out of
    // `rest` so the Root spread doesn't double-label / leak the
    // description onto a non-focusable wrapper.
    //
    // Slice-7 review (codex) item 2: pre-fix only aria-label /
    // aria-labelledby were sifted; aria-describedby leaked onto Root.
    "aria-label": rootAriaLabel,
    "aria-labelledby": rootAriaLabelledBy,
    "aria-describedby": rootAriaDescribedBy,
    ...rest
  } = props as SliderBaseProps & {
    orientation?: SliderOrientation;
    disabled?: boolean;
    required?: boolean;
    value?: number | readonly number[];
    defaultValue?: number | readonly number[];
    onValueChange?: (
      next: number | number[],
      details: Parameters<NonNullable<BaseRootProps["onValueChange"]>>[1],
    ) => void;
    onValueCommitted?: (
      next: number | number[],
      details: Parameters<NonNullable<BaseRootProps["onValueCommitted"]>>[1],
    ) => void;
    "aria-label"?: string;
    "aria-labelledby"?: string;
    "aria-describedby"?: string;
  };

  const fieldCtx = useFieldContext();
  const size: SliderSize = sizeProp ?? fieldCtx?.size ?? "md";
  const disabled = disabledProp ?? fieldCtx?.disabled ?? false;
  // Required cascade — explicit prop wins, then enclosing <Field required>,
  // then false. Base UI's Slider.Root has no `required` prop, so we land
  // the attribute on each Thumb's nested <input type="range"> through the
  // `inputRef` callback below; that input is the AT-focusable element
  // (aria-required is what AT announces, mirroring the Input cascade in
  // Input.tsx).
  const required = requiredProp ?? fieldCtx?.required ?? false;

  // Detect range mode from the current value shape. Base UI uses the
  // value's array-ness as its own range discriminant; mirror that here so
  // we render the matching number of Thumb children. In uncontrolled mode
  // we mirror Base UI's emitted value into local state so the value badge's
  // CSS position follows keyboard/drag updates after the initial
  // `defaultValue` render.
  const isControlledValue = value !== undefined;
  const [uncontrolledObserved, setUncontrolledObserved] = useState<
    number | readonly number[] | undefined
  >(defaultValue);
  const observed = value ?? uncontrolledObserved ?? defaultValue;
  const isRange = Array.isArray(observed);
  const thumbCount = isRange ? (observed as readonly number[]).length : 1;

  const handleValueChange = useCallback(
    (
      next: number | number[],
      details: Parameters<NonNullable<BaseRootProps["onValueChange"]>>[1],
    ) => {
      if (!isControlledValue) {
        setUncontrolledObserved(next);
      }
      onValueChange?.(next, details);
    },
    [isControlledValue, onValueChange],
  );

  // ──────────────────────────────────────────────────────────────────
  // Value-label thumb-tracking (Slice 7 visual-polish item 1).
  //
  // Compute the active thumb's percentage and emit it as the
  // `--zs-slider-value-position` custom property on the Root. The
  // value badge (`.zs-slider__value`) consumes it via
  // `inset-inline-start: var(--zs-slider-value-position, 50%)` so the
  // "60%" output floats above the thumb instead of pinning to the
  // top-right corner of the grid row.
  //
  // The current value is read directly from the controlled `value`
  // prop OR the uncontrolled `defaultValue` so the static
  // screenshot/SSR pass renders the label in the right place without
  // a render-callback (Base UI's `state.value` would only update on
  // the client). For range mode we use the FIRST thumb's value — the
  // single-thumb shape is the dominant case; future work can paint a
  // pair of value badges if the brief calls for it.
  //
  // The computation is undefined (→ falls back to 50%) when neither
  // value nor defaultValue is provided AND `showValue` is set — the
  // Slider has no anchor to track, so center is the safest read.
  const min = (rest as { min?: number }).min ?? 0;
  const max = (rest as { max?: number }).max ?? 100;
  const trackedValue: number | undefined = isRange
    ? (observed as readonly number[])[0]
    : (observed as number | undefined);
  let valuePositionStyle: CSSProperties | undefined;
  if (
    showValue &&
    orientation === "horizontal" &&
    typeof trackedValue === "number" &&
    Number.isFinite(trackedValue) &&
    max > min
  ) {
    const ratio = (trackedValue - min) / (max - min);
    const clamped = Math.max(0, Math.min(1, ratio));
    const percent = clamped * 100;
    valuePositionStyle = {
      ["--zs-slider-value-position" as string]: `${percent}%`,
    };
  }

  // Merge the caller's `style` with our `--zs-slider-value-position`
  // custom property. Spreading `rest` after destructuring would
  // re-introduce the caller's style and clobber the custom property;
  // explicitly destructure here and rebuild the style attr so both
  // survive (consumer keys + our CSS var). Order: consumer style first,
  // then ours — ours wins on the custom-prop collision while the rest
  // of the consumer style passes through untouched.
  const { style: consumerStyle, ...restWithoutStyle } = rest as {
    style?: CSSProperties;
  } & Record<string, unknown>;
  const mergedStyle: CSSProperties | undefined =
    consumerStyle || valuePositionStyle
      ? { ...(consumerStyle ?? {}), ...(valuePositionStyle ?? {}) }
      : undefined;

  // Per-thumb suffix-label ids. Used in range mode when the caller wires
  // an external label via `aria-labelledby` — that id wins over the
  // index-suffixed `aria-label` we build below (Base UI puts
  // `aria-labelledby` ahead of `aria-label` on the input), so both thumbs
  // would inherit the SAME accessible name. We render hidden `<span>`s
  // with " (1 of N)" / " (2 of N)" / … text and append their ids to each
  // thumb's `aria-labelledby` chain. AT concatenates the names from the
  // chain, so thumb 1 announces "<external label> (1 of N)" and thumb 2
  // announces "<external label> (2 of N)" — distinct names without
  // bypassing the consumer's labelling source.
  const suffixIdBase = useId();
  const thumbSuffixIds = Array.from(
    { length: thumbCount },
    (_, i) => `${suffixIdBase}-${i}`,
  );

  // Attach `aria-required` to each Thumb's nested `<input type="range">`
  // via the `inputRef` callback. Base UI's `Slider.Root` does NOT expose
  // a `required` prop, and the wrapper-div spread does not forward
  // `aria-required` into Thumb's internal `inputProps` (only
  // aria-label/labelledby/describedby are forwarded). The AT-focusable
  // element is the input, so setting `aria-required` on the input is
  // what screen readers announce. Mirror the Input.tsx cascade so
  // `<Field required>` flows through.
  const requiredRef = useRef(required);
  requiredRef.current = required;
  const setInputAriaRequired = useCallback((el: HTMLInputElement | null) => {
    if (el == null) return;
    if (requiredRef.current) {
      el.setAttribute("aria-required", "true");
    } else {
      el.removeAttribute("aria-required");
    }
  }, []);

  return (
    <BaseSlider.Root
      {...(restWithoutStyle as BaseRootProps)}
      ref={ref}
      orientation={orientation}
      disabled={disabled || undefined}
      value={value as never}
      defaultValue={defaultValue as never}
      onValueChange={handleValueChange as never}
      onValueCommitted={onValueCommitted as never}
      format={format}
      className={classnames(
        "zs-slider",
        `zs-slider--${variant}`,
        `zs-slider--${size}`,
        `zs-slider--${orientation}`,
        className,
      )}
      style={mergedStyle}
      data-variant={variant}
      data-size={size}
      data-orientation={orientation}
      data-testid={dataTestId}
    >
      {children}
      {showValue ? (
        // Slider.Value renders an <output> by default; Base UI auto-wires
        // `for=` to the thumb input ids. The element is announced as part
        // of the slider — no extra aria needed.
        <BaseSlider.Value
          className="zs-slider__value"
          data-testid={dataTestId ? `${dataTestId}-value` : undefined}
        />
      ) : null}
      <BaseSlider.Control className="zs-slider__control">
        <BaseSlider.Track className="zs-slider__track">
          <BaseSlider.Indicator className="zs-slider__indicator" />
          {/* Per-thumb suffix labels — referenced by each thumb's
              aria-labelledby chain in range mode. Visually hidden
              (`clip: rect(0 0 0 0)`) but kept in the accessibility tree
              so AT concatenates the suffix with the consumer's external
              label. The HTML `hidden` attribute would remove them from
              the AT tree too, breaking the labelledby reference, so we
              use the same visually-hidden style OtpField uses for its
              cell-0 fallback. */}
          {thumbCount > 1
            ? thumbSuffixIds.map((id, index) => (
                <span
                  key={id}
                  id={id}
                  className="zs-slider__thumb-suffix"
                  style={visuallyHiddenStyle}
                >
                  {` (${index + 1} of ${thumbCount})`}
                </span>
              ))
            : null}
          {Array.from({ length: thumbCount }, (_, index) => {
            // Range mode: index-suffix the label so AT users can tell
            // "Price range, thumb 1" from "thumb 2". Single mode: forward
            // the label verbatim. aria-labelledby (if set) wins over
            // aria-label on the Base UI side, so when the consumer wires
            // an external label via `aria-labelledby` we ALSO append the
            // per-thumb suffix id — otherwise both thumbs would get the
            // same accessible name.
            const thumbAriaLabel =
              rootAriaLabel != null && thumbCount > 1
                ? `${rootAriaLabel} (${index + 1} of ${thumbCount})`
                : rootAriaLabel;
            const thumbAriaLabelledBy =
              rootAriaLabelledBy != null && thumbCount > 1
                ? `${rootAriaLabelledBy} ${thumbSuffixIds[index]}`
                : rootAriaLabelledBy;
            return (
              <BaseSlider.Thumb
                key={index}
                index={index}
                className="zs-slider__thumb"
                aria-label={thumbAriaLabel}
                aria-labelledby={thumbAriaLabelledBy}
                aria-describedby={rootAriaDescribedBy}
                inputRef={setInputAriaRequired}
                data-testid={
                  dataTestId
                    ? thumbCount > 1
                      ? `${dataTestId}-thumb-${index}`
                      : `${dataTestId}-thumb`
                    : undefined
                }
              />
            );
          })}
        </BaseSlider.Track>
      </BaseSlider.Control>
    </BaseSlider.Root>
  );
});
SliderForward.displayName = "Slider";

// Cast to the overloaded shape. forwardRef's inferred signature is the
// unionised one; the SliderComponent interface re-exposes it as two
// (or three) overloads so call-site type-narrowing works.
export const Slider = SliderForward as unknown as SliderComponent;
