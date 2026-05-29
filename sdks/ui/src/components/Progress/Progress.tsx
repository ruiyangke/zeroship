/*
 * Progress — determinate / indeterminate task bar.
 *
 * Wraps Base UI's headless `progress` primitive. A Progress is the
 * opposite of Meter: it shows ONGOING work, not a known measurement.
 * Reads as `role="progressbar"` to assistive tech.
 *
 *   <Progress value={42} aria-label="Uploading" />
 *   <Progress value={null} aria-label="Loading" />   // indeterminate mode
 *
 * Anatomy:
 *   Progress.Root                  (the row container; role=progressbar)
 *     ├─ Progress.Label             (optional — when `label` prop set)
 *     ├─ Progress.Value             (optional — when `showValue` set)
 *     └─ Progress.Track             (the rail)
 *         └─ Progress.Indicator     (the filled segment / shimmer)
 *
 * Design principles encoded:
 *
 *   1. `value === null` (or undefined) → indeterminate mode. Base UI
 *      emits `data-status="indeterminate"` on the Root + omits
 *      `aria-valuenow`; we hook the indeterminate animation to that
 *      attribute so a determinate bar with a 0% value still reads as
 *      a determinate (paused) bar, not as an indeterminate (loading)
 *      one.
 *
 *   2. Indeterminate animation — a linear shimmer (translateX) on the
 *      indicator. Respect `prefers-reduced-motion: reduce` → no
 *      animation; the bar shows a static dim accent so the user still
 *      sees "this is in progress" without the motion cue.
 *
 *   3. `data-status="complete"` (value === max) → the bar transitions
 *      to a success color via the `--zs-progress-color` token mapping
 *      so a finished bar reads green ("done") instead of staying accent
 *      ("in progress"). The CompletionCelebrate story exercises this.
 *
 *   4. Sizes — sm/md/lg — drive the track block-size. sm 0.25rem / md
 *      0.375rem / lg 0.5rem (mirrors Slider + Meter sizing).
 *
 *   5. Value display — `showValue` mounts a Progress.Value (the formatted
 *      percentage). The label (when set) sits at the inline-start of the
 *      header row.
 *
 *   6. forced-colors mirror — every state selector inside the @media
 *      block at equal/higher specificity so the system palette wins.
 *      Indeterminate animation is suppressed under forced-colors so the
 *      Highlight surface doesn't translate / flicker.
 *
 *   7. RTL — Base UI emits inline-size on the indicator so RTL flips
 *      the fill direction automatically. The indeterminate shimmer
 *      uses two `translateX` keyframe sets (LTR sweeps -100% → 400%;
 *      RTL sweeps 100% → -400%) switched by a `:dir(rtl)` selector on
 *      the Root so the sweep direction follows the writing-mode
 *      regardless of which ancestor element carries `dir="rtl"`.
 *
 * Aria contract:
 *   Base UI emits `role="progressbar"`, `aria-valuemin`, `aria-valuemax`,
 *   and (for determinate mode) `aria-valuenow` on the Root. For
 *   indeterminate mode Base UI sets `aria-valuetext="Loading"` (we don't
 *   override — Base UI's default already reads correctly to AT users).
 *   When a consumer supplies `aria-valuetext` we forward it.
 */
import {
  forwardRef,
  type AriaAttributes,
  type ComponentPropsWithRef,
  type HTMLAttributes,
  type ReactNode,
} from "react";
import { Progress as BaseProgress } from "@base-ui/react/progress";
import { classnames } from "../_classnames";

export type ProgressSize = "sm" | "md" | "lg";

type BaseRootProps = ComponentPropsWithRef<typeof BaseProgress.Root>;

/**
 * Shape of Base UI's Progress.Root render-callback state.
 *
 * Sourced from `@base-ui/react/progress/root/ProgressRoot.d.ts`.
 */
type ProgressRootRenderState = {
  status: "indeterminate" | "progressing" | "complete";
};

export interface ProgressProps
  extends Omit<BaseRootProps, "className" | "render"> {
  /** Visual size — sm 0.25rem / md 0.375rem (default) / lg 0.5rem track. */
  size?: ProgressSize;
  /** Show the percentage label next to the bar. */
  showValue?: boolean;
  /** Optional label rendered at the inline-start of the row. Auto-
   *  associates with the progress via Base UI's labelledby chain. */
  label?: ReactNode;
  /** Class hook on the row container. */
  className?: string;
  "aria-label"?: AriaAttributes["aria-label"];
  "aria-labelledby"?: AriaAttributes["aria-labelledby"];
  "aria-describedby"?: AriaAttributes["aria-describedby"];
  "aria-valuetext"?: AriaAttributes["aria-valuetext"];
  /** Optional `data-testid` for tests. */
  "data-testid"?: string;
}

export const Progress = forwardRef<HTMLDivElement, ProgressProps>(
  function Progress(
    {
      size = "md",
      showValue = false,
      label,
      className,
      value,
      "aria-label": ariaLabel,
      "aria-labelledby": ariaLabelledBy,
      "aria-describedby": ariaDescribedBy,
      "aria-valuetext": ariaValueText,
      "data-testid": dataTestId,
      ...rest
    },
    ref,
  ) {
    // Build aria-* spread only with defined keys — same shape as Meter +
    // the Slider slice-7 fix. Passing `aria-label={undefined}` clobbers
    // the labelId Base UI auto-wires from Progress.Label.
    const ariaForwarded: Record<string, AriaAttributes[keyof AriaAttributes]> =
      {};
    if (ariaLabel != null) ariaForwarded["aria-label"] = ariaLabel;
    if (ariaLabelledBy != null)
      ariaForwarded["aria-labelledby"] = ariaLabelledBy;
    if (ariaDescribedBy != null)
      ariaForwarded["aria-describedby"] = ariaDescribedBy;
    if (ariaValueText != null)
      ariaForwarded["aria-valuetext"] = ariaValueText;

    return (
      <BaseProgress.Root
        {...(rest as BaseRootProps)}
        ref={ref}
        // Forward value verbatim — Base UI treats `null` as indeterminate.
        value={value as never}
        {...ariaForwarded}
        render={(
          rootProps: HTMLAttributes<HTMLDivElement>,
          state: ProgressRootRenderState,
        ) => {
          // Re-stamp `data-status` from the render-callback state so CSS
          // attribute selectors (`.zs-progress[data-status="…"]`) light
          // up consistently. Base UI itself only emits the per-status
          // marker attributes (`data-progressing` / `data-complete` /
          // `data-indeterminate`) on the Root — NOT a unified
          // `data-status`. We synthesise the unified attribute here so
          // CSS + tests can branch on a single key and stay independent
          // of Base UI's individual-flag shape.
          return (
            <div
              {...rootProps}
              className={classnames(
                "zs-progress",
                `zs-progress--${size}`,
                className,
                rootProps.className,
              )}
              data-size={size}
              data-status={state.status}
              data-testid={dataTestId}
            >
              {label != null || showValue ? (
                <div className="zs-progress__header">
                  {label != null ? (
                    <BaseProgress.Label className="zs-progress__label">
                      {label}
                    </BaseProgress.Label>
                  ) : null}
                  {showValue ? (
                    <BaseProgress.Value className="zs-progress__value" />
                  ) : null}
                </div>
              ) : null}
              <BaseProgress.Track className="zs-progress__track">
                <BaseProgress.Indicator className="zs-progress__indicator" />
              </BaseProgress.Track>
            </div>
          );
        }}
      />
    );
  },
);
Progress.displayName = "Progress";
