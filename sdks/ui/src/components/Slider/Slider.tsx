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
  type ComponentPropsWithoutRef,
  type ReactNode,
} from "react";
import { Slider as BaseSlider } from "@base-ui/react/slider";
import { useFieldContext } from "../Field";
import { classnames } from "../_classnames";

export type SliderSize = "sm" | "md" | "lg";
export type SliderVariant = "default" | "outline";
export type SliderOrientation = "horizontal" | "vertical";

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

/* ─── component ─────────────────────────────────────────────────────── */

export const Slider = forwardRef<HTMLDivElement, SliderProps>(function Slider(
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
    children,
    value,
    defaultValue,
    onValueChange,
    onValueCommitted,
    format,
    "data-testid": dataTestId,
    // The forwarded `aria-label` / `aria-labelledby` belong on the THUMB's
    // nested <input type="range"> (that's the AT-focusable element), not
    // on the Root <div>. Pull them out of `rest` so the Root spread
    // doesn't double-label.
    "aria-label": rootAriaLabel,
    "aria-labelledby": rootAriaLabelledBy,
    ...rest
  } = props as SliderBaseProps & {
    orientation?: SliderOrientation;
    disabled?: boolean;
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
  };

  const fieldCtx = useFieldContext();
  const size: SliderSize = sizeProp ?? fieldCtx?.size ?? "md";
  const disabled = disabledProp ?? fieldCtx?.disabled ?? false;

  // Detect range mode from the value/defaultValue shape. Base UI uses the
  // value's array-ness as its own range discriminant; mirror that here so
  // we render the matching number of Thumb children.
  const observed = value ?? defaultValue;
  const isRange = Array.isArray(observed);
  const thumbCount = isRange ? (observed as readonly number[]).length : 1;

  return (
    <BaseSlider.Root
      {...(rest as BaseRootProps)}
      ref={ref}
      orientation={orientation}
      disabled={disabled || undefined}
      value={value as never}
      defaultValue={defaultValue as never}
      onValueChange={onValueChange as never}
      onValueCommitted={onValueCommitted as never}
      format={format}
      className={classnames(
        "zs-slider",
        `zs-slider--${variant}`,
        `zs-slider--${size}`,
        `zs-slider--${orientation}`,
        className,
      )}
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
          {Array.from({ length: thumbCount }, (_, index) => {
            // Range mode: index-suffix the label so AT users can tell
            // "Price range, thumb 1" from "thumb 2". Single mode: forward
            // the label verbatim. aria-labelledby (if set) wins over
            // aria-label on the Base UI side.
            const thumbAriaLabel =
              rootAriaLabel != null && thumbCount > 1
                ? `${rootAriaLabel} (${index + 1} of ${thumbCount})`
                : rootAriaLabel;
            return (
              <BaseSlider.Thumb
                key={index}
                index={index}
                className="zs-slider__thumb"
                aria-label={thumbAriaLabel}
                aria-labelledby={rootAriaLabelledBy}
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
Slider.displayName = "Slider";
