/*
 * Badge — a small, static status/label chip.
 *
 * A Badge is a non-interactive token: a count, a status word, a
 * category label. It is NOT a button and NOT removable (that's Tag's
 * job — Tag generalizes the interactive removable / filter chip).
 *
 * Surface model (intent × variant × size):
 *
 *   intent:  neutral (default) | info | success | warning | danger
 *   variant: solid | soft (default) | outline
 *   size:    sm | md (default)
 *
 * Glass / axe-contrast invariant: EVERY variant paints an OPAQUE
 * background base — even `soft` and `outline`. A chip that relied on an
 * alpha tint over the page (e.g. `--zs-accent` at 14%) would let axe's
 * color-contrast walk see straight through to the mesh background and
 * either fail or report an incomplete. So each variant resolves an
 * opaque background via `color-mix(... , var(--zs-surface))` (a tint
 * blended INTO the opaque surface token) rather than `... , transparent`.
 * The per-intent / per-variant token plumbing lives in Badge.css; this
 * file only stamps the modifier classes.
 *
 * Ink contrast: the lighter system intents (success ≈ L0.6, warning ≈
 * L0.7) cannot carry near-white ink on a solid fill and still clear
 * 4.5:1, so the solid variant deepens those fills toward the label and
 * the soft/outline variants pull the ink toward the intent's deepest
 * tone. The combinations are tuned in Badge.css to be axe-clean across
 * all 5 × 3 cells — contrast is the whole point of a status chip.
 *
 * `asChild` lets a Badge render as the single child element (e.g. an
 * `<a>` linking to the filtered view) while keeping the Badge styling.
 * Routed through the shared `Slot` helper (React-19-safe refs); a
 * dev-warn fires if `asChild` is set without a single element child.
 */
import {
  forwardRef,
  isValidElement,
  type ComponentPropsWithoutRef,
  type ReactNode,
  type Ref,
} from "react";
import { Slot } from "../_slot";
import { classnames } from "../_classnames";
import type { Intent } from "../_intent";

/** Badge spans the full shared {@link Intent} vocabulary. */
export type BadgeIntent = Intent;
export type BadgeVariant = "solid" | "soft" | "outline";
export type BadgeSize = "sm" | "md";

export interface BadgeProps extends ComponentPropsWithoutRef<"span"> {
  /**
   * Semantic color family.
   * - `neutral` (default): the label/fill greys — a quiet, non-semantic
   *   chip (a count, a plain category).
   * - `info`: the accent palette — informational emphasis.
   * - `success`: system-green — a positive / completed status.
   * - `warning`: system-orange — a caution / degraded status.
   * - `danger`: system-red — an error / blocked status.
   *
   * Emitted as `data-intent` for styling/test hooks; not consumed by
   * assistive tech (the intent's meaning must also be carried by the
   * Badge's text — color alone is never the only signal).
   */
  intent?: BadgeIntent;

  /**
   * Prominence.
   * - `solid`: filled intent background + contrasting ink. The loudest
   *   form — reserve for the one status that must stand out.
   * - `soft` (default): an opaque tinted background (the intent blended
   *   INTO the surface, never alpha-over-page) + intent-toned ink. The
   *   everyday status chip.
   * - `outline`: opaque base + an intent-colored border + intent ink.
   *   The quietest form.
   *
   * Every variant paints an OPAQUE background so axe's contrast walk
   * always resolves against a known base.
   */
  variant?: BadgeVariant;

  /** Size — `sm` (caption type, tight) or `md` (footnote type). Default `md`. */
  size?: BadgeSize;

  /**
   * Render as the single child element rather than a `<span>`. Used for
   * a Badge that links somewhere (`<Badge asChild><a href="…">New</a></Badge>`).
   * Routed through the shared `Slot` helper so className / style / refs
   * compose with the child.
   */
  asChild?: boolean;

  /** Badge label. Keep it short — a word or a count. */
  children?: ReactNode;
}

export const Badge = forwardRef<HTMLElement, BadgeProps>(function Badge(
  {
    intent = "neutral",
    variant = "soft",
    size = "md",
    asChild = false,
    className,
    children,
    ...rest
  },
  ref,
) {
  const composedClassName = classnames(
    "zs-badge",
    `zs-badge--${intent}`,
    `zs-badge--${variant}`,
    `zs-badge--${size}`,
    className,
  );

  const dataProps = {
    "data-slot": "badge",
    "data-intent": intent,
    "data-variant": variant,
    "data-size": size,
  };

  if (asChild) {
    if (!isValidElement(children)) {
      if (process.env.NODE_ENV !== "production") {
        // eslint-disable-next-line no-console
        console.warn(
          "Badge asChild expects a single React element child; received " +
            typeof children +
            "; rendering nothing.",
        );
      }
      return null;
    }
    return (
      <Slot
        {...rest}
        {...dataProps}
        ref={ref as Ref<unknown>}
        className={composedClassName}
      >
        {children}
      </Slot>
    );
  }

  return (
    <span
      {...rest}
      {...dataProps}
      ref={ref as Ref<HTMLSpanElement>}
      className={composedClassName}
    >
      {children}
    </span>
  );
});

Badge.displayName = "Badge";
