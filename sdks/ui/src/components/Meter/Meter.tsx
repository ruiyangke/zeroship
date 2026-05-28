/*
 * Meter — static gauge (battery, disk, signal).
 *
 * Wraps Base UI's headless `meter` primitive. A Meter is NOT a progress
 * bar: progress shows ongoing work; a meter shows a known measurement
 * inside a known range. Reads as `role="meter"` to assistive tech.
 *
 *   <Meter value={67} aria-label="Disk usage" intent="warning" />
 *
 * Anatomy:
 *   Meter.Root                  (the row container; role=meter, aria-value*)
 *     ├─ Meter.Label             (optional — when `label` prop set)
 *     ├─ Meter.Value             (optional — when `showValue` set)
 *     └─ Meter.Track             (the rail)
 *         └─ Meter.Indicator     (the filled segment)
 *
 * Design principles encoded:
 *
 *   1. Intent — `neutral` (accent) / `success` (green) / `warning` (amber)
 *      / `danger` (red). The intent drives the indicator color, NOT the
 *      track. A consumer can paint a "low battery" red bar against the
 *      same fill-secondary track that a "good" green bar uses, so the
 *      eye reads the gauge consistently.
 *
 *   2. Ranges heuristic (brief contingency): the Ranges story uses a
 *      `getStatus(value)` helper at the call site to pick an intent based
 *      on a static threshold bucket — `<25% → danger`, `25-75% → warning`,
 *      `>75% → success`. The component itself takes intent as a prop; the
 *      heuristic is a story-side composition.
 *
 *   3. Sizes — sm/md/lg — drive the track block-size. sm 0.25rem / md
 *      0.375rem / lg 0.5rem (mirrors Slider's sizing). Same heights so
 *      a Meter and a Slider in the same column read coherent.
 *
 *   4. Value display — `showValue` mounts a Meter.Value rendered as a
 *      monospace numeric badge to the inline-end of the row. The label
 *      (when set) sits at the inline-start.
 *
 *   5. forced-colors mirror — every state selector inside the @media
 *      block at equal/higher specificity so the system palette wins.
 *      Under forced-colors, intent collapses to Highlight (positive)
 *      and Mark (warning/danger) — the system palette doesn't carry the
 *      four-step semantic gradient, so we collapse to its two slots.
 *
 *   6. RTL — Base UI emits inline-size on the indicator so RTL flips
 *      the fill direction automatically via logical properties.
 *
 *   7. Disabled — the row dims to fill-tertiary; the indicator collapses
 *      to label-quaternary (a quiet plate) so the disabled state reads
 *      as "presented, not actionable".
 *
 * Aria contract:
 *   Base UI emits `role="meter"`, `aria-valuemin`, `aria-valuemax`,
 *   `aria-valuenow`, and `aria-valuetext` on the Root. Meter.Label
 *   auto-associates via `aria-labelledby`. We forward `aria-label` /
 *   `aria-labelledby` / `aria-describedby` to the Root so consumers can
 *   override the auto-wired label without poking at Base UI internals.
 */
import {
  forwardRef,
  type AriaAttributes,
  type ComponentPropsWithRef,
  type ReactNode,
} from "react";
import { Meter as BaseMeter } from "@base-ui/react/meter";
import { classnames } from "../_classnames";

export type MeterSize = "sm" | "md" | "lg";
export type MeterIntent = "neutral" | "success" | "warning" | "danger";

type BaseRootProps = ComponentPropsWithRef<typeof BaseMeter.Root>;

export interface MeterProps
  extends Omit<BaseRootProps, "className" | "render"> {
  /** Visual size — sm 0.25rem / md 0.375rem (default) / lg 0.5rem track. */
  size?: MeterSize;
  /**
   * Intent — drives the indicator color.
   *
   * - `neutral` (default) accent
   * - `success` green
   * - `warning` amber
   * - `danger` red
   *
   * Reads as the semantic color of the measurement, NOT the track.
   */
  intent?: MeterIntent;
  /** Show the numeric value (as a formatted string via Base UI's Intl
   *  hookup) next to the gauge. */
  showValue?: boolean;
  /** Optional label rendered at the inline-start of the row. Auto-
   *  associates with the meter via Base UI's labelledby chain. */
  label?: ReactNode;
  /** Class hook on the row container. */
  className?: string;
  /** `aria-label` / `aria-labelledby` / `aria-describedby` for the row.
   *  When `label` is set, Base UI auto-wires labelledby for free. */
  "aria-label"?: AriaAttributes["aria-label"];
  "aria-labelledby"?: AriaAttributes["aria-labelledby"];
  "aria-describedby"?: AriaAttributes["aria-describedby"];
  /** Optional `data-testid` for tests. */
  "data-testid"?: string;
}

export const Meter = forwardRef<HTMLDivElement, MeterProps>(function Meter(
  {
    size = "md",
    intent = "neutral",
    showValue = false,
    label,
    className,
    "aria-label": ariaLabel,
    "aria-labelledby": ariaLabelledBy,
    "aria-describedby": ariaDescribedBy,
    "data-testid": dataTestId,
    ...rest
  },
  ref,
) {
  // Build aria-* spread only with defined keys. Passing `aria-label={undefined}`
  // to Base UI's spread merge unsets the labelId it auto-wires from
  // Meter.Label (defaultProps before elementProps in useRenderElement;
  // a literal-undefined still overwrites). Same shape Slider fixed in
  // slice-7 review item 2.
  const ariaForwarded: Record<string, AriaAttributes[keyof AriaAttributes]> = {};
  if (ariaLabel != null) ariaForwarded["aria-label"] = ariaLabel;
  if (ariaLabelledBy != null) ariaForwarded["aria-labelledby"] = ariaLabelledBy;
  if (ariaDescribedBy != null)
    ariaForwarded["aria-describedby"] = ariaDescribedBy;

  return (
    <BaseMeter.Root
      {...(rest as BaseRootProps)}
      ref={ref}
      {...ariaForwarded}
      className={classnames(
        "zs-meter",
        `zs-meter--${intent}`,
        `zs-meter--${size}`,
        className,
      )}
      data-intent={intent}
      data-size={size}
      data-testid={dataTestId}
    >
      {label != null || showValue ? (
        <div className="zs-meter__header">
          {label != null ? (
            <BaseMeter.Label className="zs-meter__label">
              {label}
            </BaseMeter.Label>
          ) : null}
          {showValue ? (
            <BaseMeter.Value className="zs-meter__value" />
          ) : null}
        </div>
      ) : null}
      <BaseMeter.Track className="zs-meter__track">
        <BaseMeter.Indicator className="zs-meter__indicator" />
      </BaseMeter.Track>
    </BaseMeter.Root>
  );
});
Meter.displayName = "Meter";

/**
 * Static-threshold helper for the Ranges story. Returns the intent
 * the brief's contingency calls for: <25% danger / 25-75% warning /
 * >75% success. Exported as a tiny composition primitive for consumers
 * who want to mirror the heuristic at their call site.
 */
export function meterStatus(
  value: number,
  min: number = 0,
  max: number = 100,
): MeterIntent {
  if (max <= min) return "neutral";
  const ratio = (value - min) / (max - min);
  if (ratio < 0.25) return "danger";
  if (ratio < 0.75) return "warning";
  return "success";
}
