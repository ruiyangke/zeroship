/*
 * Stepper — a governed multi-step progress indicator / wizard nav.
 *
 * An ordered run of steps, each in one of three states — complete /
 * current / upcoming — laid out horizontally (default) or vertically,
 * with connectors drawn between the step indicators. Optionally
 * clickable: when `onStepChange` is supplied each step becomes a real
 * `<button>` so consumers can wire wizard navigation.
 *
 *   <Stepper
 *     steps={[
 *       { id: "account", label: "Account", description: "Your details" },
 *       { id: "plan",    label: "Plan" },
 *       { id: "billing", label: "Billing" },
 *       { id: "review",  label: "Review" },
 *     ]}
 *     current={1}
 *     onStepChange={(id, index) => goTo(index)}
 *   />
 *
 * Renders as (clickable horizontal, current = index 1):
 *
 *   <ol class="zs-stepper" data-slot="stepper" data-orientation="horizontal">
 *     <li class="zs-stepper__step" data-slot="stepper-step" data-status="complete">
 *       <button type="button" class="zs-stepper__trigger">
 *         <span class="zs-stepper__indicator" data-slot="stepper-indicator">
 *           <Icon as={Check} aria-hidden />
 *         </span>
 *         <span class="zs-stepper__text">
 *           <span class="zs-stepper__label" data-slot="stepper-label">Account</span>
 *           <span class="zs-stepper__description" …>Your details</span>
 *           <span class="zs-visually-hidden">completed</span>
 *         </span>
 *       </button>
 *       <span class="zs-stepper__connector" data-slot="stepper-connector" aria-hidden />
 *     </li>
 *     <li … data-status="current"  aria-current="step"> … </li>
 *     <li … data-status="upcoming"> … </li>
 *     …
 *   </ol>
 *
 * STATUS DERIVATION. Unless a step pins its own `status`, the status is
 * derived from its index relative to the active step: index < current →
 * `complete`; index === current → `current`; index > current →
 * `upcoming`. `current` accepts either a numeric index OR a step `id`;
 * an id is resolved to its index, and an unresolvable / out-of-range
 * value falls back to 0.
 *
 * COLOR IS NEVER THE ONLY SIGNAL (WCAG 1.4.1 Use of Color). Sighted
 * users read state from the accent fill / muted ring AND the check
 * glyph; AT users get it from a visually-hidden status WORD appended to
 * every step's text — "completed" / "current step" / "upcoming". So a
 * screen reader announces "Account, completed" even though the only
 * visible difference for a complete step is an accent tint + a check.
 * The check `Icon` and the connectors are decorative (`aria-hidden`) —
 * the status word, not the glyph, is the AT-facing carrier.
 *
 * CLICKABLE A11Y. When `onStepChange` is set every step's
 * indicator+text is a real `<button type="button">` — native keyboard
 * focus, Enter/Space activation, and a focus-visible ring, no
 * `tabIndex`/`role`/`onKeyDown` hand-rolling. The callback receives
 * `(id, index)`. All steps are clickable by design; a consumer that
 * only wants completed/current steps navigable gates inside the
 * callback (e.g. `if (index > current) return;`). When `onStepChange`
 * is omitted the step text is a static `<span>` — no button, no
 * pointer affordance.
 *
 * The current step's `<li>` carries `aria-current="step"`. The root is
 * an `<ol>` (ordered) so AT exposes the step count and position.
 */
import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type ReactNode,
} from "react";
import { Check } from "lucide-react";
import { classnames } from "../../components/_classnames";
import { Icon } from "../../components/Icon";

export type StepperOrientation = "horizontal" | "vertical";

export type StepStatus = "complete" | "current" | "upcoming";

export interface StepperStep {
  /** Stable identifier — passed back to `onStepChange` and usable as the
   *  `current` selector. Must be unique within `steps`. */
  id: string;
  /** The step's visible name. */
  label: ReactNode;
  /** Optional secondary line under the label (a short hint / sub-title). */
  description?: ReactNode;
  /** Pin this step's status explicitly, bypassing index-vs-`current`
   *  derivation. Omit to let the Stepper derive it. */
  status?: StepStatus;
}

export interface StepperProps extends ComponentPropsWithoutRef<"ol"> {
  /** The ordered steps. Rendered as `<li>`s inside the root `<ol>`. */
  steps: StepperStep[];
  /**
   * The active step, as a numeric index OR a step `id`. Drives status
   * derivation for every step whose own `status` is unset (index <
   * current → complete; === → current; > → upcoming). Default `0`. An
   * id with no match, or an out-of-range index, falls back to 0.
   */
  current?: number | string;
  /**
   * Layout axis. `horizontal` (default) lays steps inline with
   * horizontal connectors; `vertical` stacks them with vertical
   * connectors.
   */
  orientation?: StepperOrientation;
  /**
   * When supplied, every step becomes a real `<button>` that calls
   * `onStepChange(id, index)` on activation — the opt-in that makes the
   * Stepper a clickable wizard nav. Omit for a read-only indicator.
   * Gate navigability (e.g. block jumping ahead) inside the callback.
   */
  onStepChange?: (id: string, index: number) => void;
}

/** Per-status visually-hidden word — the non-color signal AT announces.
 *  The trailing space keeps it from running into adjacent text in the
 *  flattened accessible name. */
const STATUS_WORD: Record<StepStatus, string> = {
  complete: "completed",
  current: "current step",
  upcoming: "upcoming",
};

/** Resolve the active index from a `current` index-or-id against the
 *  step list. Unresolvable / out-of-range → 0. */
function resolveCurrentIndex(
  steps: StepperStep[],
  current: number | string,
): number {
  if (typeof current === "string") {
    const byId = steps.findIndex((step) => step.id === current);
    return byId >= 0 ? byId : 0;
  }
  // Numeric: clamp to a valid index, defaulting out-of-range to 0.
  return current >= 0 && current < steps.length ? current : 0;
}

export const Stepper = forwardRef<HTMLOListElement, StepperProps>(
  function Stepper(
    {
      steps,
      current = 0,
      orientation = "horizontal",
      onStepChange,
      className,
      ...rest
    },
    ref,
  ) {
    const activeIndex = resolveCurrentIndex(steps, current);
    const clickable = typeof onStepChange === "function";

    return (
      <ol
        {...rest}
        ref={ref}
        data-slot="stepper"
        data-orientation={orientation}
        className={classnames("zs-stepper", className)}
      >
        {steps.map((step, index) => {
          // Pinned status wins; otherwise derive from position vs active.
          const status: StepStatus =
            step.status ??
            (index < activeIndex
              ? "complete"
              : index === activeIndex
                ? "current"
                : "upcoming");

          const isComplete = status === "complete";
          const isLast = index === steps.length - 1;

          // The indicator + text column — shared by the button and the
          // static paths so the two render-as targets stay identical.
          const inner = (
            <>
              <span
                className="zs-stepper__indicator"
                data-slot="stepper-indicator"
              >
                {isComplete ? (
                  // Check is the SHAPE signal for a done step; aria-hidden
                  // because the visually-hidden status word carries the
                  // AT meaning, not the glyph.
                  <Icon as={Check} size="sm" className="zs-stepper__check" />
                ) : (
                  // 1-based step number for current/upcoming. aria-hidden:
                  // the position is already exposed by the <ol>/<li>.
                  <span aria-hidden="true">{index + 1}</span>
                )}
              </span>
              <span className="zs-stepper__text">
                <span className="zs-stepper__label" data-slot="stepper-label">
                  {step.label}
                </span>
                {step.description != null ? (
                  <span
                    className="zs-stepper__description"
                    data-slot="stepper-description"
                  >
                    {step.description}
                  </span>
                ) : null}
                {/* The non-color status carrier (WCAG 1.4.1). */}
                <span className="zs-visually-hidden">
                  {STATUS_WORD[status]}
                </span>
              </span>
            </>
          );

          return (
            <li
              key={step.id}
              className="zs-stepper__step"
              data-slot="stepper-step"
              data-status={status}
              aria-current={status === "current" ? "step" : undefined}
            >
              {clickable ? (
                <button
                  type="button"
                  className="zs-stepper__trigger"
                  data-slot="stepper-trigger"
                  onClick={() => onStepChange?.(step.id, index)}
                >
                  {inner}
                </button>
              ) : (
                <span className="zs-stepper__trigger">{inner}</span>
              )}
              {/* Connector to the NEXT step. Decorative — its completion
                  tint is a visual reinforcement of the [data-status]
                  already on the steps. Omitted after the last step. */}
              {!isLast ? (
                <span
                  className="zs-stepper__connector"
                  data-slot="stepper-connector"
                  aria-hidden="true"
                />
              ) : null}
            </li>
          );
        })}
      </ol>
    );
  },
);
Stepper.displayName = "Stepper";
